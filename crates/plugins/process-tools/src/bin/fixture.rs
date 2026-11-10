//! 测试夹具：按命令行参数模拟正常/挂起/崩溃的第三方工具。
//!
//! 模式：
//! - `echo`（默认）：正常响应；
//! - `hang`：收到 `call_tool` 后永不响应；
//! - `crash`：第一次 `call_tool` 直接退出；
//! - `crash-once <marker>`：第一次 `call_tool` 退出并写 marker，第二次起正常。

use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;

use serde_json::{Value, json};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("echo");
    let marker = args.get(1).cloned();
    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock()).lines();
    let mut stdout = std::io::stdout();
    loop {
        let line = match lines.next() {
            Some(Ok(line)) => line,
            _ => return ExitCode::SUCCESS,
        };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = request.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => reply(
                &mut stdout,
                id,
                json!({"id": "process.tools", "version": "0.1.0"}),
            ),
            "list_tools" => reply(
                &mut stdout,
                id,
                json!({"tools": [{
                    "name": "echo",
                    "summary": "echo the arguments back",
                    "inputSchema": {"type": "object", "additionalProperties": true}
                }]}),
            ),
            "call_tool" => match mode {
                "env" => {
                    let vars: std::collections::BTreeMap<String, String> = std::env::vars()
                        .filter(|(key, _)| key.starts_with("ACB_PLUGIN_"))
                        .collect();
                    reply(&mut stdout, id, json!({"result": {"env": vars}}));
                }
                "hang" => continue,
                "crash" => return ExitCode::FAILURE,
                "crash-once"
                    if marker
                        .as_ref()
                        .map(|path| !std::path::Path::new(path).exists())
                        .unwrap_or(true) =>
                {
                    if let Some(path) = &marker {
                        let _ = std::fs::write(path, "crashed");
                    }
                    return ExitCode::FAILURE;
                }
                _ => {
                    let result = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({"echo": params}));
                    reply(&mut stdout, id, json!({"result": result}));
                }
            },
            "shutdown" => {
                reply(&mut stdout, id, json!({}));
                return ExitCode::SUCCESS;
            }
            _ => reply_error(&mut stdout, id, -32601, "unknown method"),
        }
    }
}

fn reply(stdout: &mut std::io::Stdout, id: Value, result: Value) {
    let line = json!({"jsonrpc": "2.0", "id": id, "result": result});
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

fn reply_error(stdout: &mut std::io::Stdout, id: Value, code: i64, message: &str) {
    let line = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}
