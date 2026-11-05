//! 极简 HTTP/1.1 服务：只支持 JSON API 需要的 GET/POST/PATCH/DELETE。
//!
//! 不引入 HTTP 框架；每个连接一个线程，每个请求开一个 SQLite 连接（WAL）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::{
    CreateIssue, CreateSubscription, CreateWorkItem, IssueError, MyWorkQuery, ReportResult,
    SCOPE_ADMIN, SCOPE_READ, SCOPE_REPORT, SCOPE_SUBSCRIBE, SCOPE_VERIFY, SCOPE_WRITE, SearchQuery,
    UpdateIssue, VerifyWorkItem, ack_events, authenticate, create_issue, create_subscription,
    create_token, create_work_item, deactivate_subscription, events_for_issue, list_subscriptions,
    list_work_items_public, my_work, now_ms, open_db, report_result, require_scope, revoke_token,
    rotate_token, run_due, search_issues, update_issue, verify_work_item,
};

pub struct ServerState {
    pub db: PathBuf,
    pub admin: String,
}

pub fn serve(listener: TcpListener, state: Arc<ServerState>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    let _ = handle_connection(stream, &state);
                });
            }
            Err(error) => eprintln!("issue-service: accept failed: {error}"),
        }
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

fn handle_connection(mut stream: TcpStream, state: &ServerState) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let Some(request) = read_request(&mut stream)? else {
        return Ok(());
    };
    let conn = match open_db(&state.db) {
        Ok(conn) => conn,
        Err(error) => {
            return write_json(
                &mut stream,
                500,
                &json!({"error": {"code": "db", "message": error.to_string()}}),
            );
        }
    };
    let now = now_ms();
    let _ = run_due(&conn, now, &state.admin);
    let response = route(&conn, &request, state, now);
    match response {
        Ok((status, value)) => write_json(&mut stream, status, &value),
        Err(error) => {
            let (status, code) = error_status(&error);
            write_json(
                &mut stream,
                status,
                &json!({"error": {"code": code, "message": error.to_string()}}),
            )
        }
    }
}

fn error_status(error: &IssueError) -> (u16, &'static str) {
    match error {
        IssueError::Unauthorized => (401, "unauthorized"),
        IssueError::Forbidden(_) => (403, "forbidden"),
        IssueError::NotFound(_) => (404, "not_found"),
        IssueError::StaleVersion => (409, "stale_version"),
        IssueError::InvalidTransition { .. } => (409, "invalid_transition"),
        IssueError::VerificationRequired(_) => (409, "verification_required"),
        IssueError::BadRequest(_) => (400, "bad_request"),
        IssueError::Db(_) => (500, "db"),
    }
}

fn route(
    conn: &rusqlite::Connection,
    request: &Request,
    state: &ServerState,
    now: i64,
) -> Result<(u16, Value), IssueError> {
    let path = request.path.as_str();
    if request.method == "GET" && path == "/healthz" {
        return Ok((200, json!({"ok": true})));
    }

    let principal = authenticate(conn, bearer_token(request).as_deref().unwrap_or(""), now)?;

    if request.method == "POST" && path == "/v1/tokens" {
        require_scope(&principal, SCOPE_ADMIN)?;
        let body: Value = parse_body(request)?;
        let principal_id = body["principal_id"]
            .as_str()
            .ok_or_else(|| IssueError::BadRequest("principal_id is required".to_owned()))?;
        let name = body["name"].as_str().unwrap_or("token");
        let scopes: Vec<String> = body["scopes"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let days = body["expires_days"].as_i64().unwrap_or(90);
        let token = create_token(
            conn,
            principal_id,
            name,
            &scopes,
            now + days * 24 * 3600 * 1000,
            now,
            None,
        )?;
        return Ok((
            200,
            json!({
                "token": token.plaintext,
                "token_id": token.token_id,
                "principal_id": token.principal_id,
                "scopes": token.scopes,
                "expires_at": token.expires_at,
            }),
        ));
    }
    if request.method == "POST" && path.starts_with("/v1/tokens/") {
        require_scope(&principal, SCOPE_ADMIN)?;
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        // /v1/tokens/{id}/rotate|revoke
        if parts.len() == 4 {
            let id = parts[2];
            return match parts[3] {
                "rotate" => {
                    let token = rotate_token(conn, id, now)?;
                    Ok((
                        200,
                        json!({
                                            "token": token.plaintext,
                                            "token_id": token.token_id,
                                            "expires_at": token.expires_at,
                        "rotated_from": id,
                                        }),
                    ))
                }
                "revoke" => {
                    revoke_token(conn, id, now)?;
                    Ok((200, json!({"ok": true, "revoked": id})))
                }
                _ => Err(IssueError::NotFound(path.to_owned())),
            };
        }
    }

    if request.method == "POST" && path == "/v1/issues" {
        require_scope(&principal, SCOPE_WRITE)?;
        let mut input: CreateIssue = parse_body(request)?;
        let actor = principal.actor_for(input.on_behalf_of.as_deref());
        if input.reporter.is_none() {
            input.reporter = Some(actor.clone());
        }
        let issue = create_issue(conn, input, &actor, now)?;
        return Ok((200, serde_json::to_value(issue).unwrap_or(json!({}))));
    }
    if request.method == "GET" && path == "/v1/issues" {
        require_scope(&principal, SCOPE_READ)?;
        let query = search_query(&request.query);
        let page = search_issues(conn, &query)?;
        return Ok((200, serde_json::to_value(page).unwrap_or(json!({}))));
    }
    if request.method == "GET" && path.starts_with("/v1/issues/") {
        require_scope(&principal, SCOPE_READ)?;
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 3 {
            let issue = crate::get_issue(conn, parts[2])?;
            return Ok((200, serde_json::to_value(issue).unwrap_or(json!({}))));
        }
        if parts.len() == 4 && parts[3] == "events" {
            let events = events_for_issue(conn, parts[2])?;
            return Ok((200, serde_json::to_value(events).unwrap_or(json!({}))));
        }
        if parts.len() == 4 && parts[3] == "work_items" {
            let items = list_work_items_public(conn, parts[2])?;
            return Ok((200, serde_json::to_value(items).unwrap_or(json!({}))));
        }
    }
    if request.method == "PATCH" && path.starts_with("/v1/issues/") {
        require_scope(&principal, SCOPE_WRITE)?;
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 3 {
            let input: UpdateIssue = parse_body(request)?;
            let actor = principal.actor_for(input.on_behalf_of.as_deref());
            let is_admin = principal.has(SCOPE_ADMIN);
            let issue = update_issue(conn, parts[2], input, &actor, is_admin, &state.admin, now)?;
            return Ok((200, serde_json::to_value(issue).unwrap_or(json!({}))));
        }
    }
    if request.method == "POST" && path.starts_with("/v1/issues/") && path.ends_with("/work_items")
    {
        require_scope(&principal, SCOPE_WRITE)?;
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 4 {
            let input: CreateWorkItem = parse_body(request)?;
            let actor = principal.actor_for(input.on_behalf_of.as_deref());
            let item = create_work_item(conn, parts[2], input, &actor, now)?;
            return Ok((200, serde_json::to_value(item).unwrap_or(json!({}))));
        }
    }
    if request.method == "GET" && path.starts_with("/v1/work_items/") {
        require_scope(&principal, SCOPE_READ)?;
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 3 {
            let item = crate::get_work_item(conn, parts[2])?;
            return Ok((200, serde_json::to_value(item).unwrap_or(json!({}))));
        }
    }
    if request.method == "POST" && path.starts_with("/v1/work_items/") {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 4 && parts[3] == "report" {
            require_scope(&principal, SCOPE_REPORT)?;
            let input: ReportResult = parse_body(request)?;
            let actor = principal.actor_for(input.on_behalf_of.as_deref());
            let value = report_result(conn, parts[2], input, &actor, now)?;
            return Ok((200, value));
        }
        if parts.len() == 4 && parts[3] == "verify" {
            require_scope(&principal, SCOPE_VERIFY)?;
            let input: VerifyWorkItem = parse_body(request)?;
            let actor = principal.actor_for(input.on_behalf_of.as_deref());
            let is_admin = principal.has(SCOPE_ADMIN);
            let item =
                verify_work_item(conn, parts[2], input, &actor, is_admin, &state.admin, now)?;
            return Ok((200, serde_json::to_value(item).unwrap_or(json!({}))));
        }
    }

    if request.method == "POST" && path == "/v1/subscriptions" {
        require_scope(&principal, SCOPE_SUBSCRIBE)?;
        let input: CreateSubscription = parse_body(request)?;
        let sub = create_subscription(conn, input, now)?;
        return Ok((200, serde_json::to_value(sub).unwrap_or(json!({}))));
    }
    if request.method == "GET" && path == "/v1/subscriptions" {
        require_scope(&principal, SCOPE_READ)?;
        let subscriber = query_value(&request.query, "subscriber");
        let subs = list_subscriptions(conn, subscriber.as_deref())?;
        return Ok((200, serde_json::to_value(subs).unwrap_or(json!({}))));
    }
    if request.method == "DELETE" && path.starts_with("/v1/subscriptions/") {
        require_scope(&principal, SCOPE_SUBSCRIBE)?;
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        if parts.len() == 3 {
            deactivate_subscription(conn, parts[2], now)?;
            return Ok((200, json!({"ok": true})));
        }
    }

    if request.method == "GET" && path == "/v1/my_work" {
        require_scope(&principal, SCOPE_READ)?;
        let query = MyWorkQuery {
            assignee: query_value(&request.query, "assignee"),
            subscriber: query_value(&request.query, "subscriber"),
            channel: query_value(&request.query, "channel"),
            status: query_value(&request.query, "status"),
            ack_event_ids: query_value(&request.query, "ack_event_ids")
                .map(|value| {
                    value
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        };
        let work = my_work(conn, &principal.id, &query, now)?;
        return Ok((200, serde_json::to_value(work).unwrap_or(json!({}))));
    }
    if request.method == "POST" && path == "/v1/events/ack" {
        require_scope(&principal, SCOPE_REPORT)?;
        let body: Value = parse_body(request)?;
        let subscriber = body["subscriber_id"]
            .as_str()
            .ok_or_else(|| IssueError::BadRequest("subscriber_id is required".to_owned()))?;
        let channel = body["channel"].as_str().unwrap_or("acb_session");
        let ids: Vec<String> = body["event_ids"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let value = ack_events(conn, subscriber, channel, &ids, now)?;
        return Ok((200, value));
    }
    if request.method == "POST" && path == "/v1/maintenance/run_due" {
        require_scope(&principal, SCOPE_ADMIN)?;
        let body: Value = parse_body(request).unwrap_or_else(|_| json!({}));
        let tick_now = body["now_ms"].as_i64().unwrap_or(now);
        let summary = run_due(conn, tick_now, &state.admin)?;
        return Ok((200, serde_json::to_value(summary).unwrap_or(json!({}))));
    }

    Err(IssueError::NotFound(path.to_owned()))
}

fn bearer_token(request: &Request) -> Option<String> {
    request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_body<T: serde::de::DeserializeOwned>(request: &Request) -> Result<T, IssueError> {
    serde_json::from_slice(&request.body)
        .map_err(|error| IssueError::BadRequest(format!("invalid JSON body: {error}")))
}

fn search_query(query: &str) -> SearchQuery {
    SearchQuery {
        status: query_value(query, "status"),
        tags: query_value(query, "tags")
            .map(|value| {
                value
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        assignee: query_value(query, "assignee"),
        since: query_value(query, "since").and_then(|value| value.parse().ok()),
        until: query_value(query, "until").and_then(|value| value.parse().ok()),
        q: query_value(query, "q"),
        cursor: query_value(query, "cursor"),
        limit: query_value(query, "limit").and_then(|value| value.parse().ok()),
    }
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            Some(percent_decode(value))
        } else {
            None
        }
    })
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
            {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > 64 * 1024 {
            return Ok(None);
        }
    };
    let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let mut headers = std::collections::HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Some(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        headers,
        body,
    }))
}

fn write_json(stream: &mut TcpStream, status: u16, value: &Value) -> std::io::Result<()> {
    let body = value.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let challenge = if status == 401 {
        "WWW-Authenticate: Bearer\r\n"
    } else {
        ""
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{challenge}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
