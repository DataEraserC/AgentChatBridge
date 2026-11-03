//! 协议层自己的错误：构造参数不合法、编码失败、帧解不开。
//!
//! 工具的失败不在这里——那是 [`crate::frame::ToolError`]，走正常的帧返回，不断连接。

use thiserror::Error;

/// 构造协议类型时的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProtoError {
    #[error("{}", acb_i18n::t!("proto.connection.empty_name"))]
    EmptyConnection,
    #[error("{}", acb_i18n::t!("proto.room.empty_path"))]
    EmptyPath,
    #[error("{}", acb_i18n::t!("proto.connection.separator", part = .0))]
    SeparatorInPart(String),
    #[error("{}", acb_i18n::t!("proto.ticket.empty"))]
    EmptyTicket,
    #[error("{}", acb_i18n::t!("proto.frame.encode_failed", detail = .0))]
    Encode(String),
}

/// 解帧失败。除了「太大」，其余都不是断连理由之外的错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("{}", acb_i18n::t!("proto.frame.too_large", size = size, max = max))]
    TooLarge { size: usize, max: usize },
    #[error("{}", acb_i18n::t!("proto.json.invalid", detail = detail))]
    NotJson { detail: String },
    #[error("{}", acb_i18n::t!("proto.frame.unknown", frame = frame))]
    UnknownFrame { frame: String },
    #[error("{}", acb_i18n::t!("proto.frame.missing_field", detail = detail))]
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
