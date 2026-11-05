//! `acb-issue-tools`：process-tools 载体的 stdio provider。
//!
//! ACB 只启动这个子进程；issue 服务必须独立运行。调用全部走 HTTP，不读 SQLite。

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;

use serde_json::{Value, json};

fn main() {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc":"2.0","id":Value::Null,"error":{"code":-32700,"message":format!("invalid JSON: {error}")}})
                );
                continue;
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "shutdown" {
            let _ = writeln!(
                out,
                "{}",
                json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}})
            );
            break;
        }
        let result = match method {
            "initialize" => Ok(json!({"name":"acb-issue-tools","version":"0.1.0"})),
            "list_tools" => Ok(json!({"tools": tools()})),
            "call_tool" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                call_tool(name, arguments)
            }
            other => Err(format!("unknown method {other}")),
        };
        let response = match result {
            Ok(value) => {
                // 只有 call_tool 需要 `{"result": ...}` 包装；list_tools/initialize
                // 的 result 就是协议约定的载荷。
                let payload = if method == "call_tool" {
                    json!({"result": value})
                } else {
                    value
                };
                json!({"jsonrpc":"2.0","id":id,"result":payload})
            }
            Err(message) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":message}})
            }
        };
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "issue_create",
            "创建 issue（标题 + 正文 + 可选标签/指派人）",
            json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "priority":{"type":"string","enum":["P0","P1","P2","P3"]},
                    "tags":{"type":"array","items":{"type":"string"}},
                    "assignee":{"type":"string"},
                    "source":{"type":"object","additionalProperties":true},
                    "related":{"type":"object","additionalProperties":true},
                    "on_behalf_of":{"type":"string","description":"ACB service token 专用；其它 token 忽略"}
                },
                "required":["title"],
                "additionalProperties":true
            }),
        ),
        tool(
            "issue_get",
            "按 id 读 issue（含 work items）",
            json!({
                "type":"object",
                "properties":{"id":{"type":"string"}},
                "required":["id"],
                "additionalProperties":false
            }),
        ),
        tool(
            "issue_search",
            "按状态/标签/指派人/时间/全文搜索 issue",
            json!({
                "type":"object",
                "properties":{
                    "q":{"type":"string"},
                    "status":{"type":"string"},
                    "tags":{"type":"array","items":{"type":"string"}},
                    "assignee":{"type":"string"},
                    "since":{"type":"integer"},
                    "until":{"type":"integer"},
                    "cursor":{"type":"string"},
                    "limit":{"type":"integer"}
                },
                "additionalProperties":false
            }),
        ),
        tool(
            "issue_update",
            "更新 issue 状态/指派/正文（必须带 expected_version）",
            json!({
                "type":"object",
                "properties":{
                    "id":{"type":"string"},
                    "expected_version":{"type":"integer"},
                    "status":{"type":"string"},
                    "assignee":{"type":"string"},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "priority":{"type":"string"},
                    "tags":{"type":"array","items":{"type":"string"}},
                    "resolution":{"type":"string"},
                    "comment":{"type":"string"},
                    "on_behalf_of":{"type":"string"}
                },
                "required":["id","expected_version"],
                "additionalProperties":true
            }),
        ),
        tool(
            "issue_subscribe",
            "订阅 issue / 标签 / 查询条件",
            json!({
                "type":"object",
                "properties":{
                    "subscriber_id":{"type":"string","description":"session:<id> / principal:<id> / webhook:<id>"},
                    "channel":{"type":"string","enum":["acb_session","console","webhook"]},
                    "filter":{"type":"object","additionalProperties":true}
                },
                "required":["subscriber_id","filter"],
                "additionalProperties":false
            }),
        ),
        tool(
            "issue_unsubscribe",
            "取消订阅",
            json!({
                "type":"object",
                "properties":{"id":{"type":"string"}},
                "required":["id"],
                "additionalProperties":false
            }),
        ),
        tool(
            "issue_report_result",
            "回报 WorkItem 执行结果（带幂等键）",
            json!({
                "type":"object",
                "properties":{
                    "issue_id":{"type":"string"},
                    "work_item_id":{"type":"string"},
                    "status":{"type":"string"},
                    "result":{"type":"string"},
                    "idempotency_key":{"type":"string"},
                    "on_behalf_of":{"type":"string"}
                },
                "required":["work_item_id","result","idempotency_key"],
                "additionalProperties":true
            }),
        ),
        tool(
            "issue_my_work",
            "查当前待办 + 未读事件；带 ack_event_ids 时显式确认",
            json!({
                "type":"object",
                "properties":{
                    "assignee":{"type":"string"},
                    "subscriber":{"type":"string"},
                    "channel":{"type":"string","enum":["acb_session","console","webhook"]},
                    "status":{"type":"string"},
                    "ack_event_ids":{"type":"array","items":{"type":"string"}}
                },
                "additionalProperties":false
            }),
        ),
    ]
}

fn tool(name: &str, summary: &str, schema: Value) -> Value {
    json!({"name":name,"summary":summary,"inputSchema":schema})
}

fn call_tool(name: &str, args: Value) -> Result<Value, String> {
    match name {
        "issue_create" => post("/v1/issues", &args),
        "issue_get" => {
            let id = str_arg(&args, "id")?;
            get(&format!("/v1/issues/{id}"))
        }
        "issue_search" => {
            let mut query = Vec::new();
            for key in [
                "q", "status", "assignee", "since", "until", "cursor", "limit",
            ] {
                if let Some(value) = args.get(key) {
                    query.push(format!(
                        "{key}={}",
                        url_encode(value.to_string().trim_matches('"'))
                    ));
                }
            }
            if let Some(tags) = args.get("tags").and_then(Value::as_array) {
                let joined = tags
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined.is_empty() {
                    query.push(format!("tags={}", url_encode(&joined)));
                }
            }
            let suffix = if query.is_empty() {
                String::new()
            } else {
                format!("?{}", query.join("&"))
            };
            get(&format!("/v1/issues{suffix}"))
        }
        "issue_update" => {
            let id = str_arg(&args, "id")?;
            patch(&format!("/v1/issues/{id}"), &args)
        }
        "issue_subscribe" => post("/v1/subscriptions", &args),
        "issue_unsubscribe" => {
            let id = str_arg(&args, "id")?;
            delete(&format!("/v1/subscriptions/{id}"))
        }
        "issue_report_result" => {
            let id = str_arg(&args, "work_item_id")?;
            post(&format!("/v1/work_items/{id}/report"), &args)
        }
        "issue_my_work" => {
            let mut query = Vec::new();
            for key in ["assignee", "subscriber", "channel", "status"] {
                if let Some(value) = args.get(key).and_then(Value::as_str) {
                    query.push(format!("{key}={}", url_encode(value)));
                }
            }
            if let Some(ids) = args.get("ack_event_ids").and_then(Value::as_array) {
                let joined = ids
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined.is_empty() {
                    query.push(format!("ack_event_ids={}", url_encode(&joined)));
                }
            }
            let suffix = if query.is_empty() {
                String::new()
            } else {
                format!("?{}", query.join("&"))
            };
            get(&format!("/v1/my_work{suffix}"))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing {key}"))
}

fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn service_url() -> String {
    std::env::var("ACB_ISSUE_URL").unwrap_or_else(|_| "http://127.0.0.1:7800".to_owned())
}

fn token() -> String {
    std::env::var("ACB_ISSUE_TOKEN").unwrap_or_default()
}

fn get(path: &str) -> Result<Value, String> {
    request("GET", path, None)
}

fn post(path: &str, body: &Value) -> Result<Value, String> {
    request("POST", path, Some(body))
}

fn patch(path: &str, body: &Value) -> Result<Value, String> {
    request("PATCH", path, Some(body))
}

fn delete(path: &str) -> Result<Value, String> {
    request("DELETE", path, None)
}

fn request(method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
    let url = service_url();
    let Some(rest) = url.strip_prefix("http://") else {
        return Err(format!("only http:// is supported, got {url}"));
    };
    let (host, port) = rest
        .split_once(':')
        .map(|(host, port)| (host, port.parse::<u16>().unwrap_or(80)))
        .ok_or_else(|| format!("bad ACB_ISSUE_URL {url}"))?;
    let token = token();
    if token.is_empty() {
        return Err("ACB_ISSUE_TOKEN is empty".to_owned());
    }
    let body_text = body.map(|value| value.to_string()).unwrap_or_default();
    let mut stream = TcpStream::connect((host, port))
        .map_err(|error| format!("connect {host}:{port}: {error}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .ok();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_text}",
        body_text.len()
    )
    .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let text = String::from_utf8_lossy(&response).to_string();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "response has no body separator".to_owned())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or(500);
    let value: Value = serde_json::from_str(body.trim())
        .map_err(|error| format!("invalid response JSON: {error}; body={body}"))?;
    if (200..300).contains(&status) {
        Ok(value)
    } else {
        Err(format!(
            "issue service returned {status}: {}",
            value["error"]["message"].as_str().unwrap_or("")
        ))
    }
}
