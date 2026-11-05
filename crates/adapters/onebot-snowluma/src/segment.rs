//! 内容元素 ↔ OneBot 11 消息段。
//!
//! 两个方向都按「最大程度适配、降不了就明确报错、绝不悄悄丢」办（ADR-0004）：
//! - 出去：标准元素翻成 OneBot 段，翻不出来报 `Unsupported`；
//! - 进来：不认识的段装进 [`Element::Opaque`] 原样留着，编回 QQ 时照搬。

use acb_proto::Element;
use serde_json::{Value, json};

/// 平台名。房间写成 `qq:group/123`。
pub const PLATFORM: &str = "qq";

/// 翻译失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// 这个平台表达不了这段内容。
    #[error("{0}")]
    Unsupported(String),
}

/// 内容元素 → OneBot 消息段数组。
pub fn to_onebot(elements: &[Element]) -> Result<Vec<Value>, SegmentError> {
    elements.iter().map(one_to_onebot).collect()
}

fn one_to_onebot(element: &Element) -> Result<Value, SegmentError> {
    Ok(match element {
        Element::Text { text } => json!({"type": "text", "data": {"text": text}}),
        Element::At { target, all } => {
            let qq = if *all { "all" } else { target.as_str() };
            json!({"type": "at", "data": {"qq": qq}})
        }
        Element::Quote { message_id } => json!({"type": "reply", "data": {"id": message_id}}),
        Element::Image { url, .. } => json!({"type": "image", "data": {"file": url}}),
        Element::Audio { url } => json!({"type": "record", "data": {"file": url}}),
        Element::Video { url } => json!({"type": "video", "data": {"file": url}}),
        Element::Face { id, .. } => json!({"type": "face", "data": {"id": id}}),
        Element::File { .. } => {
            return Err(SegmentError::Unsupported(acb_i18n::t!(
                "adapter.onebot11.file_unsupported"
            )));
        }
        // QQ 自己的段，原样送回去。
        Element::Opaque { platform, payload } if platform == PLATFORM => payload.clone(),
        Element::Opaque { platform, .. } => {
            return Err(SegmentError::Unsupported(acb_i18n::t!(
                "adapter.onebot11.foreign_platform_content",
                platform = platform
            )));
        }
    })
}

/// OneBot 消息段数组 → 内容元素，顺带判定「有没有 @ 到我」。
///
/// `message` 不是数组时按 CQ 码字符串解析（SnowLuma 的 `messageFormat: string`
/// 会把 `message` 换成 CQ 串；老实现也直接发 CQ 串）。解析不出来的片段保留为文本，
/// 不静默丢弃。
pub fn from_onebot(message: &Value, self_id: &str) -> (Vec<Element>, bool) {
    let Some(items) = message.as_array() else {
        let text = message.as_str().unwrap_or_default();
        if text.is_empty() {
            return (Vec::new(), false);
        }
        return parse_cq(text, self_id);
    };

    let mut elements = Vec::new();
    let mut at_self = false;

    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        let data = item.get("data").cloned().unwrap_or(Value::Null);
        let (element, aimed) = segment_element(kind, &data, self_id);
        at_self |= aimed;
        elements.push(element);
    }

    (elements, at_self)
}

/// 一段 OneBot 消息段（数组项或 CQ 码解析结果）→ 内容元素。
fn segment_element(kind: &str, data: &Value, self_id: &str) -> (Element, bool) {
    let field = |key: &str| {
        data.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    match kind {
        "text" => (Element::text(field("text")), false),
        "at" => {
            let qq = field("qq");
            let all = qq == "all";
            let aimed = !all && !self_id.is_empty() && qq == self_id;
            (
                Element::At {
                    target: if all { String::new() } else { qq },
                    all,
                },
                aimed,
            )
        }
        "reply" => (
            Element::Quote {
                message_id: field("id"),
            },
            false,
        ),
        "image" => (
            Element::Image {
                url: url_of(data),
                alt: data
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            false,
        ),
        "record" => (Element::Audio { url: url_of(data) }, false),
        "video" => (Element::Video { url: url_of(data) }, false),
        "face" => (
            Element::Face {
                platform: PLATFORM.to_owned(),
                id: field("id"),
            },
            false,
        ),
        // 卡片、合并转发、markdown……核心不认识，原样留着。
        _ => (
            Element::Opaque {
                platform: PLATFORM.to_owned(),
                payload: json!({"type": kind, "data": data}),
            },
            false,
        ),
    }
}

/// CQ 码字符串 → 内容元素。语法 `[CQ:type,key=value,...]`；`[` / `]` / `,` / `&`
/// 用 `&#91;` / `&#93;` / `&#44;` / `&amp;` 转义。未知段保留成 Opaque。
fn parse_cq(text: &str, self_id: &str) -> (Vec<Element>, bool) {
    let mut elements = Vec::new();
    let mut at_self = false;
    let mut rest = text;

    while let Some(start) = rest.find("[CQ:") {
        let (before, marked) = rest.split_at(start);
        if !before.is_empty() {
            elements.push(Element::text(unescape_cq(before)));
        }
        let body_and_rest = &marked["[CQ:".len()..];
        // 只认第一个 `]`；CQ 参数值里的 `]` 必须转义成 `&#93;`。
        let Some(end) = body_and_rest.find(']') else {
            // 没有闭合括号：从 `[CQ:` 到结尾都按文本保留，不猜。
            elements.push(Element::text(unescape_cq(marked)));
            return (elements, at_self);
        };
        let (kind, params) = match body_and_rest[..end].split_once(',') {
            Some((kind, params)) => (kind, Some(params)),
            None => (&body_and_rest[..end], None),
        };
        let kind = kind.trim();
        let mut data = serde_json::Map::new();
        if let Some(params) = params {
            for pair in params.split(',') {
                let Some((key, value)) = pair.split_once('=') else {
                    continue;
                };
                let key = key.trim();
                if key.is_empty() {
                    continue;
                }
                data.insert(key.to_owned(), json!(unescape_cq(value)));
            }
        }
        let data = Value::Object(data);
        let (element, aimed) = segment_element(kind, &data, self_id);
        at_self |= aimed;
        elements.push(element);
        rest = &body_and_rest[end + 1..];
    }

    if !rest.is_empty() {
        elements.push(Element::text(unescape_cq(rest)));
    }
    (elements, at_self)
}

/// CQ 转义还原。先还原转义字符本身，最后还原 `&`，避免 `&amp;#91;` 被多解一层。
fn unescape_cq(text: &str) -> String {
    text.replace("&#91;", "[")
        .replace("&#93;", "]")
        .replace("&#44;", ",")
        .replace("&amp;", "&")
}

/// 媒体段优先取直链，取不到退回平台内部文件名。
fn url_of(data: &Value) -> String {
    for key in ["url", "file"] {
        if let Some(value) = data.get(key).and_then(Value::as_str) {
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_mention_go_out_as_onebot_segments() {
        let segments = to_onebot(&[
            Element::text("在吗 "),
            Element::At {
                target: "10001".into(),
                all: false,
            },
        ])
        .expect("能翻");
        assert_eq!(
            segments[0],
            json!({"type": "text", "data": {"text": "在吗 "}})
        );
        assert_eq!(segments[1], json!({"type": "at", "data": {"qq": "10001"}}));
    }

    #[test]
    fn mention_all_uses_the_magic_value() {
        let segments = to_onebot(&[Element::At {
            target: String::new(),
            all: true,
        }])
        .expect("能翻");
        assert_eq!(segments[0]["data"]["qq"], "all");
    }

    #[test]
    fn quote_becomes_reply_segment() {
        let segments = to_onebot(&[Element::Quote {
            message_id: "77".into(),
        }])
        .expect("能翻");
        assert_eq!(segments[0]["type"], "reply");
        assert_eq!(segments[0]["data"]["id"], "77");
    }

    #[test]
    fn file_is_refused_not_silently_dropped() {
        let error = to_onebot(&[Element::File {
            url: "u".into(),
            name: "n".into(),
        }])
        .expect_err("发不了就该报错");
        assert!(matches!(error, SegmentError::Unsupported(_)));
    }

    #[test]
    fn foreign_platform_payload_is_refused() {
        let error = to_onebot(&[Element::Opaque {
            platform: "kook".into(),
            payload: json!({"card": 1}),
        }])
        .expect_err("别的平台的内容发不到 QQ");
        assert!(matches!(error, SegmentError::Unsupported(_)));
    }

    #[test]
    fn qq_payload_passes_through_unchanged() {
        let card = json!({"type": "json", "data": {"data": "{\"app\":\"x\"}"}});
        let segments = to_onebot(&[Element::Opaque {
            platform: PLATFORM.into(),
            payload: card.clone(),
        }])
        .expect("原样送回去");
        assert_eq!(segments[0], card);
    }

    #[test]
    fn incoming_message_is_decoded_and_mention_detected() {
        let message = json!([
            {"type": "at", "data": {"qq": "999"}},
            {"type": "text", "data": {"text": " 看这个"}},
            {"type": "image", "data": {"url": "https://e/a.png", "summary": "图"}},
        ]);
        let (elements, at_self) = from_onebot(&message, "999");
        assert!(at_self, "@ 到我就是冲我来的");
        assert_eq!(
            elements,
            vec![
                Element::At {
                    target: "999".into(),
                    all: false
                },
                Element::text(" 看这个"),
                Element::Image {
                    url: "https://e/a.png".into(),
                    alt: Some("图".into())
                },
            ]
        );
    }

    #[test]
    fn at_all_is_not_at_self() {
        let message = json!([{"type": "at", "data": {"qq": "all"}}]);
        let (elements, at_self) = from_onebot(&message, "999");
        assert!(!at_self);
        assert_eq!(
            elements,
            vec![Element::At {
                target: String::new(),
                all: true
            }]
        );
    }

    #[test]
    fn unknown_segment_is_kept_as_opaque() {
        let message = json!([{"type": "mface", "data": {"emoji_id": "5"}}]);
        let (elements, _) = from_onebot(&message, "999");
        assert_eq!(
            elements,
            vec![Element::Opaque {
                platform: PLATFORM.into(),
                payload: json!({"type": "mface", "data": {"emoji_id": "5"}}),
            }]
        );
    }

    #[test]
    fn cq_string_is_parsed_into_segments() {
        let (elements, at_self) = from_onebot(&json!("[CQ:at,qq=999] 你好"), "999");
        assert_eq!(
            elements,
            vec![
                Element::At {
                    target: "999".to_owned(),
                    all: false,
                },
                Element::text(" 你好"),
            ]
        );
        assert!(at_self, "string 模式也要能认出 @ 到自己");
    }

    #[test]
    fn cq_escapes_are_decoded_after_segment_parsing() {
        let (elements, _) = from_onebot(&json!("[CQ:face,id=14]&#91;x&#93;&#44;&amp;"), "999");
        assert_eq!(
            elements,
            vec![
                Element::Face {
                    platform: PLATFORM.to_owned(),
                    id: "14".to_owned(),
                },
                Element::text("[x],&"),
            ]
        );
    }

    #[test]
    fn unknown_cq_segment_is_kept_as_opaque() {
        let (elements, _) = from_onebot(&json!("[CQ:mface,emoji_id=5]"), "999");
        assert_eq!(
            elements,
            vec![Element::Opaque {
                platform: PLATFORM.to_owned(),
                payload: json!({"type": "mface", "data": {"emoji_id": "5"}}),
            }]
        );
    }

    #[test]
    fn unclosed_cq_marker_is_kept_as_text() {
        let (elements, at_self) = from_onebot(&json!("[CQ:at,qq=999"), "999");
        assert_eq!(elements, vec![Element::text("[CQ:at,qq=999")]);
        assert!(!at_self, "不闭合的 CQ 码不猜");
    }

    #[test]
    fn cq_reply_and_image_are_mapped() {
        let (elements, _) = from_onebot(
            &json!("[CQ:reply,id=42][CQ:image,file=https://x/a.png]"),
            "999",
        );
        assert_eq!(
            elements,
            vec![
                Element::Quote {
                    message_id: "42".to_owned(),
                },
                Element::Image {
                    url: "https://x/a.png".to_owned(),
                    alt: None,
                },
            ]
        );
    }
}
