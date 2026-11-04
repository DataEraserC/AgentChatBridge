//! stdio 上的行 JSON-RPC 2.0。只实现载体需要的四个方法。
//!
//! 请求固定带 `jsonrpc`、数字 `id`、`method`、`params`；响应按 `id` 配对。
//! 不是通知、不带批量、不做双向请求——协议面越小，越不容易在子进程里出错。

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'static str,
    pub params: Value,
}

#[derive(Debug, Deserialize)]
pub struct Response {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
pub struct RpcError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
}

/// 一行响应的解析结果。
#[derive(Debug, PartialEq)]
pub enum Decoded {
    Result(Value),
    RpcError {
        code: i64,
        message: String,
    },
    /// 不是给这个请求的响应（id 不匹配），丢弃继续等。
    Ignore,
    /// 连 JSON 都不是，或形状不对。
    Malformed(String),
}

pub fn encode_request(id: u64, method: &'static str, params: Value) -> String {
    serde_json::to_string(&Request {
        jsonrpc: "2.0",
        id,
        method,
        params,
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

pub fn decode_response(line: &str, expected_id: u64) -> Decoded {
    let response: Response = match serde_json::from_str(line) {
        Ok(response) => response,
        Err(error) => return Decoded::Malformed(format!("invalid JSON-RPC response: {error}")),
    };
    if response.id != Some(expected_id) {
        return Decoded::Ignore;
    }
    if let Some(error) = response.error {
        return Decoded::RpcError {
            code: error.code,
            message: error.message,
        };
    }
    match response.result {
        Some(result) => Decoded::Result(result),
        None => Decoded::Malformed("JSON-RPC response has neither result nor error".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_request_with_jsonrpc_and_id() {
        let text = encode_request(7, "initialize", json!({}));
        let value: Value = serde_json::from_str(&text).expect("合法 JSON");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], 7);
        assert_eq!(value["method"], "initialize");
    }

    #[test]
    fn decodes_result_only_for_matching_id() {
        let line = r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#;
        assert!(matches!(decode_response(line, 3), Decoded::Result(_)));
        assert_eq!(decode_response(line, 4), Decoded::Ignore);
    }

    #[test]
    fn decodes_rpc_error() {
        let line = r#"{"jsonrpc":"2.0","id":9,"error":{"code":-32601,"message":"unknown"}}"#;
        assert_eq!(
            decode_response(line, 9),
            Decoded::RpcError {
                code: -32601,
                message: "unknown".to_owned()
            }
        );
    }

    #[test]
    fn malformed_line_is_reported() {
        assert!(matches!(
            decode_response("not json", 1),
            Decoded::Malformed(_)
        ));
    }
}
