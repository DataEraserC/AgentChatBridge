//! 真进程验收：ACB（plugin provider）不在时用 HTTP 建 issue/回报；
//! ACB 启动后 `acb-issue-tools` 能看到未读并 ack。
//!
//! 这里启动的是真实的 `acb-issue-service` 和 `acb-issue-tools` 二进制，
//! 不经过库函数直连 SQLite。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_issue_service::{
    CreateSubscription, SCOPE_READ, SCOPE_REPORT, SCOPE_SUBSCRIBE, SCOPE_WRITE,
    create_subscription, create_token, open_db,
};
use serde_json::{Value, json};

const ADMIN: &str = "admin";
const SERVICE_PRINCIPAL: &str = "acb-bridge";

fn temp_path(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("acb-issue-provider-{tag}-{stamp}.db"))
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn http(port: u16, method: &str, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
    let body = body.map(|value| value.to_string()).unwrap_or_default();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("连接 issue 服务");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response).to_string();
    let (head, body) = text.split_once("\r\n\r\n").expect("响应要有 body");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(500);
    let value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
    (status, value)
}

fn wait_ready(port: u16) {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ = stream.write_all(
                b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let mut response = Vec::new();
            let _ = stream.read_to_end(&mut response);
            if response.starts_with(b"HTTP/1.1 200") {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("issue 服务没有在端口 {port} 就绪");
}

fn spawn_service(db: &PathBuf, port: u16) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_acb-issue-service"))
        .arg("serve")
        .env("ACB_ISSUE_DB", db)
        .env("ACB_ISSUE_LISTEN", format!("127.0.0.1:{port}"))
        .env("ACB_ISSUE_ADMIN_PRINCIPAL", ADMIN)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 acb-issue-service");
    wait_ready(port);
    child
}

fn rpc(stdin: &mut impl Write, stdout: &mut BufReader<impl Read>, request: Value) -> Value {
    writeln!(stdin, "{request}").unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    serde_json::from_str(&line).expect("provider 要回合法 JSON")
}

#[test]
fn acb_provider_sees_unread_after_offline_http_writes() {
    let db = temp_path("flow");
    let (port, service_token) = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let conn = open_db(&db).unwrap();
        let now = acb_issue_service::now_ms();
        let service = create_token(
            &conn,
            SERVICE_PRINCIPAL,
            "acb",
            &[
                SCOPE_READ.to_owned(),
                SCOPE_WRITE.to_owned(),
                SCOPE_REPORT.to_owned(),
                SCOPE_SUBSCRIBE.to_owned(),
            ],
            now + 90 * 24 * 3600 * 1000,
            now,
            None,
        )
        .unwrap()
        .plaintext;
        create_subscription(
            &conn,
            CreateSubscription {
                subscriber_id: "session:s1".to_owned(),
                channel: "acb_session".to_owned(),
                filter: json!({}),
            },
            now,
        )
        .unwrap();
        (port, service)
    };
    let mut service = spawn_service(&db, port);

    // ACB（provider）还没启动：只用 HTTP 建 issue、拆 workitem、回报结果。
    let (status, issue) = http(
        port,
        "POST",
        "/v1/issues",
        &service_token,
        Some(json!({
            "title": "ACB 停机时创建",
            "body": "停机期间回报",
            "on_behalf_of": "session:s1",
        })),
    );
    assert_eq!(status, 200);
    let issue_id = issue["id"].as_str().expect("issue id");
    let (status, item) = http(
        port,
        "POST",
        &format!("/v1/issues/{issue_id}/work_items"),
        &service_token,
        Some(json!({
            "title": "执行切片",
            "assignee": "agent:exec",
            "acceptance": "",
            "on_behalf_of": "session:s1",
        })),
    );
    assert_eq!(status, 200);
    let item_id = item["id"].as_str().expect("work item id");
    let (status, report) = http(
        port,
        "POST",
        &format!("/v1/work_items/{item_id}/report"),
        &service_token,
        Some(json!({
            "result": "停机期间完成",
            "idempotency_key": "provider-flow-1",
            "on_behalf_of": "agent:exec",
        })),
    );
    assert_eq!(status, 200, "report: {report}");

    // ACB 启动：真实的 process-tools provider 子进程。
    let mut tools = Command::new(env!("CARGO_BIN_EXE_acb-issue-tools"))
        .env("ACB_ISSUE_URL", format!("http://127.0.0.1:{port}"))
        .env("ACB_ISSUE_TOKEN", &service_token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("启动 acb-issue-tools");
    let mut stdin = tools.stdin.take().unwrap();
    let mut stdout = BufReader::new(tools.stdout.take().unwrap());

    let init = rpc(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    );
    assert_eq!(init["result"]["name"], "acb-issue-tools");

    let listed = rpc(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc":"2.0","id":2,"method":"list_tools","params":{}}),
    );
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names.len(), 8, "provider 必须暴露 8 个工具");
    for expected in [
        "issue_create",
        "issue_get",
        "issue_search",
        "issue_update",
        "issue_subscribe",
        "issue_unsubscribe",
        "issue_report_result",
        "issue_my_work",
    ] {
        assert!(names.contains(&expected), "{expected} 不在 provider 工具表");
    }

    let unread = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc":"2.0","id":3,"method":"call_tool",
            "params":{
                "name":"issue_my_work",
                "arguments":{"subscriber":"session:s1","channel":"acb_session"}
            }
        }),
    );
    let result = &unread["result"]["result"];
    assert_eq!(
        result["unread_events"]["count"], 3,
        "created + workitem + result"
    );
    let ids: Vec<String> = result["unread_events"]["event_ids"]
        .as_array()
        .expect("event_ids")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        result["unread_events"]["events"][0]["title"],
        "ACB 停机时创建"
    );

    let acked = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc":"2.0","id":4,"method":"call_tool",
            "params":{
                "name":"issue_my_work",
                "arguments":{
                    "subscriber":"session:s1",
                    "channel":"acb_session",
                    "ack_event_ids": ids,
                }
            }
        }),
    );
    assert_eq!(
        acked["result"]["result"]["unread_events"]["count"], 0,
        "带 ack_event_ids 的调用先确认，再返回剩余未读"
    );

    let after = rpc(
        &mut stdin,
        &mut stdout,
        json!({
            "jsonrpc":"2.0","id":5,"method":"call_tool",
            "params":{
                "name":"issue_my_work",
                "arguments":{"subscriber":"session:s1","channel":"acb_session"}
            }
        }),
    );
    assert_eq!(
        after["result"]["result"]["unread_events"]["count"], 0,
        "ack 后不再重复返回"
    );

    let _ = rpc(
        &mut stdin,
        &mut stdout,
        json!({"jsonrpc":"2.0","id":6,"method":"shutdown"}),
    );
    let _ = tools.wait();
    let _ = service.kill();
    let _ = service.wait();
    cleanup(&db);
}

#[test]
fn http_rejects_missing_and_insufficient_tokens() {
    let db = temp_path("auth");
    let (port, reader) = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let conn = open_db(&db).unwrap();
        let now = acb_issue_service::now_ms();
        let reader = create_token(
            &conn,
            "reader",
            "reader",
            &[SCOPE_READ.to_owned()],
            now + 90 * 24 * 3600 * 1000,
            now,
            None,
        )
        .unwrap()
        .plaintext;
        (port, reader)
    };
    let mut service = spawn_service(&db, port);

    let (status, _) = http(port, "GET", "/v1/issues", "", None);
    assert_eq!(status, 401);
    let (status, _) = http(
        port,
        "POST",
        "/v1/issues",
        &reader,
        Some(json!({"title": "越权"})),
    );
    assert_eq!(status, 403);
    let (status, _) = http(port, "GET", "/v1/issues", &reader, None);
    assert_eq!(status, 200);

    let _ = service.kill();
    let _ = service.wait();
    cleanup(&db);
}
