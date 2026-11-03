//! 协议层自己的错误：构造参数不合法、编码失败、帧解不开。
//!
//! 工具的失败不在这里——那是 [`crate::frame::ToolError`]，走正常的帧返回，不断连接。

use thiserror::Error;

/// 构造协议类型时的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtoError {
    #[error("connection name must not be empty")]
    EmptyConnection,
    #[error("room path must not be empty (write it as segment/segment, e.g. group/123)")]
    EmptyPath,
    #[error("connection name or path segment must not contain a separator (: or /): {0}")]
    SeparatorInPart(String),
    #[error("turn ticket must not be empty")]
    EmptyTicket,
    #[error("frame encode failed: {0}")]
    Encode(String),
}

/// 解帧失败。除了「太大」，其余都不是断连理由之外的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("frame too large: {size} bytes (limit {max})")]
    TooLarge { size: usize, max: usize },
    #[error("not valid JSON: {detail}")]
    NotJson { detail: String },
    #[error("unknown frame: {frame}")]
    UnknownFrame { frame: String },
    #[error("frame is missing a required field: {detail}")]
    MissingField { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_render_readable_text() {
        let error = ProtoError::EmptyPath;
        assert_eq!(
            error.to_string(),
            "room path must not be empty (write it as segment/segment, e.g. group/123)"
        );

        let decode = DecodeError::TooLarge { size: 9, max: 4 };
        assert!(decode.to_string().contains("9"));
    }
}
