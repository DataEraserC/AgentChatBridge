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
    Missing(&'static str),
    #[error("bad room: {0}")]
    BadRoom(String),
}

/// 解析结果：要么一条记录，要么「这类型不产生记录」。
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
            false,
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
    let openid = text_of(payload.get(openid_field)).ok_or(EventError::Missing(openid_field))?;
    if openid.is_empty() {
        return Err(EventError::Missing(openid_field));
    }
    let room = RoomId::new(connection, format!("{scope}/{openid}"))
        .map_err(|error| EventError::BadRoom(error.to_string()))?;
    let content = text_of(payload.get("content")).unwrap_or_default();
    let msg_id = text_of(payload.get("id")).unwrap_or_default();
    // 被动回复的窗口按 msg_id 计：收到的每条消息都记下，出站才有的回复。
    if !msg_id.is_empty() {
        reply_state.observe(&room, &msg_id);
    }
    let sender_id = text_of(payload.get("author"))
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
