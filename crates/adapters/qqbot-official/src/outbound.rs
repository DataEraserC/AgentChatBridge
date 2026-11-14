//! 出站：桥给的内容元素 → 官方机器人的被动回复。
//!
//! 限制（平台规则，见 crate 文档）：只能回复**最近收到**的消息（带收到的
//! `msg_id` + 递增的 `msg_seq`）；没有「主动推送」能力。内容只支持纯文本，
//! 富媒体元素明确报错（不降级不丢弃——`api_v2` 的富媒体走另一套接口，接了再说）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use acb_kernel::{Element, Outbound, OutboundError, RoomId, Sent};

use crate::token::TokenManager;

/// 每个房间最近收到的 `msg_id` 与该 `msg_id` 下已用的 `msg_seq`。
///
/// 官方限制：同一 `msg_id` 最多回 5 次，且只在收到后的被动窗口内有效。这里
/// 按 `msg_seq` 单调递增保证不重号；窗口过期由平台侧报错，适配器原样透传。
#[derive(Debug, Default, Clone)]
pub struct ReplyState {
    /// Arc 共享：工厂克隆给出站与入站两侧，msg_id 锚点必须互通。
    inner: Arc<Mutex<HashMap<String, (String, u32)>>>,
}

impl ReplyState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 入站记录：这个房间刚收到一条 `msg_id`（被动回复的窗口锚点）。
    pub fn observe(&self, room: &RoomId, msg_id: &str) {
        let key = room_key(room);
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(key, (msg_id.to_owned(), 0));
    }

    /// 取这个房间的回复锚点并占用下一个 `msg_seq`。没有锚点 = 不能主动发。
    fn next_seq(&self, room: &RoomId) -> Option<(String, u32)> {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = map.get_mut(&room_key(room))?;
        entry.1 += 1;
        Some((entry.0.clone(), entry.1))
    }
}

fn room_key(room: &RoomId) -> String {
    room.path.join("/")
}

/// QQ 官方机器人出站。一个连接一个实例。
#[derive(Debug, Clone)]
pub struct QqBotOfficialOutbound {
    connection: String,
    token_manager: TokenManager,
    reply_state: ReplyState,
    /// REST 基地址（默认官方线上；沙箱用 sandbox.api.sgroup.qq.com）。
    api_base: String,
}

impl QqBotOfficialOutbound {
    pub fn new(
        connection: impl Into<String>,
        token_manager: TokenManager,
        reply_state: ReplyState,
        api_base: impl Into<String>,
    ) -> Self {
        Self {
            connection: connection.into(),
            token_manager,
            reply_state,
            api_base: api_base.into(),
        }
    }
}

const DEFAULT_API_BASE: &str = "https://api.sgroup.qq.com";

/// REST 基地址默认值；沙箱连接通过 options.api_base 覆盖。
pub fn default_api_base() -> &'static str {
    DEFAULT_API_BASE
}

impl Outbound for QqBotOfficialOutbound {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unsupported(format!(
                "{}",
                acb_i18n::t!("adapter.qqbot.room_mismatch", room = room)
            )));
        }
        // 内容：只支持纯文本拼接。富媒体元素明确报错。
        let mut text = String::new();
        for element in elements {
            match element.as_text() {
                Some(piece) => text.push_str(piece),
                None => {
                    return Err(OutboundError::Unsupported(
                        acb_i18n::t!("adapter.qqbot.rich_media_unsupported").to_owned(),
                    ));
                }
            }
        }
        if text.trim().is_empty() {
            return Err(OutboundError::Unsupported(
                acb_i18n::t!("adapter.qqbot.empty_text").to_owned(),
            ));
        }
        let (msg_id, msg_seq) = self.reply_state.next_seq(room).ok_or_else(|| {
            OutboundError::Unavailable(
                acb_i18n::t!("adapter.qqbot.no_passive_window").to_owned(),
            )
        })?;
        let openid = room
            .path
            .get(1)
            .cloned()
            .ok_or_else(|| OutboundError::Unsupported(acb_i18n::t!("adapter.qqbot.room_mismatch", room = room).to_owned()))?;

        let body = json!({
            "content": text,
            "msg_id": msg_id,
            "msg_seq": msg_seq,
            "msg_type": 0,
        });
        let url = format!(
            "{}/v2/users/{}/messages?msg_seq={}",
            self.api_base.trim_end_matches('/'),
            openid,
            msg_seq
        );
        let auth = self
            .token_manager
            .auth_header()
            .map_err(|error| OutboundError::Unavailable(error.to_string()))?;
        let response = ureq::post(&url)
            .set("Authorization", &auth)
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(|error| {
                // ureq 的错误里带 HTTP 状态码；平台的业务错误码在响应体里，
                // 这里只透传状态与文本，够运维定位。
                OutboundError::Rejected(error.to_string())
            })?;
        let payload: serde_json::Value = response
            .into_json()
            .map_err(|error| OutboundError::Rejected(error.to_string()))?;
        let platform_id = payload
            .get("id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        Ok(Sent {
            platform_id,
            at: now_millis(),
        })
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::Element;

    fn room(scoped: &str, openid: &str) -> RoomId {
        RoomId::new("qqbot-main", format!("{scoped}/{openid}")).expect("房间合法")
    }

    #[test]
    fn passive_window_requires_observed_msg_id() {
        let state = ReplyState::new();
        let room = room("group", "ABC123");
        assert!(state.next_seq(&room).is_none(), "没收到消息就不能回复");
        state.observe(&room, "msgid-1");
        let (msg_id, seq) = state.next_seq(&room).expect("锚点已建立");
        assert_eq!(msg_id, "msgid-1");
        assert_eq!(seq, 1);
        let (msg_id, seq) = state.next_seq(&room).expect("第二次回复");
        assert_eq!(msg_id, "msgid-1");
        assert_eq!(seq, 2, "msg_seq 单调递增");
    }

    #[test]
    fn separate_rooms_keep_separate_windows() {
        let state = ReplyState::new();
        let a = room("group", "AAA");
        let b = room("private", "BBB");
        state.observe(&a, "msgid-a");
        assert!(state.next_seq(&b).is_none());
        state.observe(&b, "msgid-b");
        assert_eq!(state.next_seq(&a).expect("A 独立").0, "msgid-a");
        assert_eq!(state.next_seq(&b).expect("B 独立").0, "msgid-b");
    }

    #[test]
    fn non_text_elements_are_rejected_upfront() {
        let state = ReplyState::new();
        let room = room("group", "ABC123");
        state.observe(&room, "msgid-1");
        let mut outbound = QqBotOfficialOutbound::new(
            "qqbot-main",
            TokenManager::new("appid", "secret"),
            state,
            DEFAULT_API_BASE,
        );
        let error = outbound
            .send(&room, &[Element::Image { url: "https://x/y.png".into(), alt: None }])
            .expect_err("富媒体应明确报错");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }
}
