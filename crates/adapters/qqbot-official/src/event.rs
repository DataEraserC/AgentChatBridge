//! 网关 dispatch 事件 → [`acb_kernel::RecordDraft`]。
//!
//! 范围（与「官方机器人功能有限」对齐）：群 @ 消息、好友单聊消息是正文；其它
//! dispatch 类型记一条 suppressed 事件（不投递、不唤醒），流水仍有迹可循。
//! 频道（DIRECT_MESSAGE_CREATE）后续要接时在这里加分支。

use serde_json::Value;

use acb_kernel::{Element, RecordDraft, RecordKind, RoomId, RoomScope, Sender};

use crate::outbound::ReplyState;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("missing field: {0}")]
    Missing(String),
    #[error("bad room: {0}")]
    BadRoom(String),
}

/// 解析结果：要么一条记录，要么「这类型不产生记录」。
#[derive(Debug)]
pub enum Parsed {
    Record(RecordDraft),
    /// 心跳/确认类：直接丢弃。
    Ignored,
}

/// 网关 dispatch（`op == 0`）的入口。`event_type` 是 `t`，`payload` 是 `d`。
pub fn parse(
    connection: &str,
    event_type: &str,
    payload: &Value,
    reply_state: &ReplyState,
    at: i64,
) -> Result<Parsed, EventError> {
    match event_type {
        "GROUP_AT_MESSAGE_CREATE" => message(
            connection,
            payload,
            reply_state,
            at,
            "group",
            "group_openid",
            true,
        ),
        "C2C_MESSAGE_CREATE" => message(
            connection,
            payload,
            reply_state,
            at,
            "private",
            "user_openid",
            // 私聊消息天然「冲我来」：唤醒策略的 at_mention 才能在单聊里工作。
            true,
        ),
        // 其余类型（FRIEND_ADD、interaction、audit…）记 suppressed 事件流水。
        _ => Ok(Parsed::Record(notice(connection, event_type, payload, at)?)),
    }
}

fn message(
    connection: &str,
    payload: &Value,
    reply_state: &ReplyState,
    at: i64,
    scope: &str,
    openid_field: &'static str,
    at_self: bool,
) -> Result<Parsed, EventError> {
    // OpenID 的位置文档内外的形态都有：GROUP 在顶层（group_openid），C2C 在
    // author.user_openid / author.id。三种都试，读不到就把真实键名留在错误里
    // （2026-11-15 实测：C2C 载荷顶层没有 user_openid，一直 Missing 被丢弃）。
    let author = payload.get("author");
    let openid = text_of(payload.get(openid_field))
        .or_else(|| author.and_then(|value| text_of(value.get(openid_field))))
        .or_else(|| author.and_then(|value| text_of(value.get("id"))))
        .filter(|openid| !openid.is_empty())
        .ok_or_else(|| {
            let mut keys: Vec<String> = payload
                .as_object()
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            if let Some(map) = author.and_then(Value::as_object) {
                keys.extend(map.keys().map(|key| format!("author.{key}")));
            }
            EventError::Missing(format!("{openid_field} (payload keys: {keys:?})"))
        })?;
    let room = RoomId::new(connection, format!("{scope}/{openid}"))
        .map_err(|error| EventError::BadRoom(error.to_string()))?;
    let content = text_of(payload.get("content")).unwrap_or_default();
    let msg_id = text_of(payload.get("id")).unwrap_or_default();
    // 被动回复的窗口按 msg_id 计：收到的每条消息都记下，出站才有的回复。
    if !msg_id.is_empty() {
        reply_state.observe(&room, &msg_id);
    }
    // author 是对象时 id 即发送者 openid；是字符串时直接当发送者。
    let sender_id = author
        .and_then(|value| text_of(value.get("id")))
        .unwrap_or_else(|| openid.clone());
    let draft = RecordDraft::message(
        room.clone(),
        Sender {
            id: sender_id.clone(),
            name: sender_id,
            role: None,
        },
        at,
        vec![Element::Text { text: content }],
        at_self,
    )
    .with_room_scope(if scope == "private" {
        RoomScope::Direct
    } else {
        RoomScope::MultiParty
    })
    .with_platform_id(msg_id.clone());
    Ok(Parsed::Record(draft))
}

fn notice(
    connection: &str,
    event_type: &str,
    payload: &Value,
    at: i64,
) -> Result<RecordDraft, EventError> {
    let room = RoomId::new(connection, format!("group/{event_type}"))
        .map_err(|error| EventError::BadRoom(error.to_string()))?;
    Ok(RecordDraft {
        room,
        kind: RecordKind::Event {
            event: format!("qq.{event_type}"),
            tags: vec!["notice".to_owned()],
            payload: Some(payload.clone()),
            summary: None,
            body: false,
        },
        sender: Sender {
            id: "qq".to_owned(),
            name: "QQ 官方".to_owned(),
            role: None,
        },
        at,
        elements: Vec::new(),
        at_self: false,
        platform_id: None,
        reply_to_self: false,
        target_platform_id: None,
        target_labels: Vec::new(),
        suppressed: true,
        room_scope: Some(RoomScope::MultiParty),
        room_labels: Vec::new(),
    })
}

fn text_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        // author 是对象：openid 字段名按事件类型不同（user_openid）。
        Value::Object(map) => map
            .get("user_openid")
            .and_then(|inner| inner.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_c2c(payload: Value) -> Result<Parsed, EventError> {
        let reply = ReplyState::new();
        parse("qqbot-official", "C2C_MESSAGE_CREATE", &payload, &reply, 1)
    }

    fn parse_group(payload: Value) -> Result<Parsed, EventError> {
        let reply = ReplyState::new();
        parse(
            "qqbot-official",
            "GROUP_AT_MESSAGE_CREATE",
            &payload,
            &reply,
            1,
        )
    }

    #[test]
    fn c2c_openid_nested_in_author_is_found() {
        // 线上实测形态：顶层没有 user_openid，它在 author 里。
        let parsed = parse_c2c(json!({
            "author": {"id": "author-id", "user_openid": "OPENID_1"},
            "content": "在吗",
            "id": "msg-1"
        }))
        .expect("author 形态应能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("应产生记录");
        };
        assert_eq!(
            draft.room.to_string(),
            "qqbot-official:private/OPENID_1",
            "房间串拿去配订阅"
        );
        assert_eq!(draft.sender.id, "author-id");
        assert_eq!(draft.platform_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn c2c_openid_top_level_still_works() {
        let parsed = parse_c2c(json!({
            "user_openid": "OPENID_2",
            "content": "hi",
            "id": "msg-2"
        }))
        .expect("顶层形态应能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("应产生记录");
        };
        assert_eq!(draft.room.to_string(), "qqbot-official:private/OPENID_2");
    }

    #[test]
    fn group_openid_stays_top_level() {
        let parsed = parse_group(json!({
            "group_openid": "GROUP_OPENID",
            "author": {"id": "member-openid"},
            "content": "@bot 你好",
            "id": "msg-3"
        }))
        .expect("群形态应能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("应产生记录");
        };
        assert_eq!(draft.room.to_string(), "qqbot-official:group/GROUP_OPENID");
        assert_eq!(draft.sender.id, "member-openid");
        assert!(draft.at_self, "群消息标 at_self");
    }

    #[test]
    fn missing_openid_error_lists_payload_keys() {
        let error = parse_c2c(json!({"content": "没有 openid 的怪载荷"}))
            .expect_err("读不到 openid 必须报错");
        let message = error.to_string();
        assert!(message.contains("user_openid"), "{message}");
        assert!(message.contains("payload keys"), "{message}");
        assert!(message.contains("content"), "{message}");
    }
}
