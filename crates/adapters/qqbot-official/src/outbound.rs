//! 出站：桥给的内容元素 → 官方机器人的被动回复。
//!
//! 限制（平台规则，见 crate 文档）：只能回复**最近收到**的消息（带收到的
//! `msg_id` + 递增的 `msg_seq`）；没有「主动推送」能力。内容只支持纯文本，
//! 富媒体元素明确报错（不降级不丢弃——`api_v2` 的富媒体走另一套接口，接了再说）。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::json;

use acb_kernel::{Element, Outbound, OutboundError, RoomId, Sent};

use crate::token::TokenManager;

/// 每个房间最近收到的 `msg_id` 与该 `msg_id` 下已用的 `msg_seq`。
///
/// 官方限制：同一 `msg_id` 最多回 5 次，且只在收到后的被动窗口内有效。这里
/// 按 `msg_seq` 单调递增保证不重号；窗口过期由平台侧报错，适配器原样透传。
///
/// 持久化：绑定自有 SQLite（[`ReplyState::bind`]）后写入
/// `passive_windows(room_key, msg_id, seq, observed_at)`——桥重启后被动窗口
/// 仍然存活。未绑定时退化为内存（仅测试/极早窗口有意义）。
#[derive(Debug, Clone)]
pub struct ReplyState {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// 已绑定的自有库；`None` = 未绑定（observe/next_seq 走内存兜底）。
    store: Mutex<Option<rusqlite::Connection>>,
    /// 绑定前的内存锚点；bind 时并入库。
    fallback: Mutex<HashMap<String, (String, u32)>>,
}

impl ReplyState {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::default()),
        }
    }

    /// 绑定自有库（launch 线程拿到 `state_dir` 后第一时间调用；出站线程的
    /// 锚点只能来自入站记录，而入站记录晚于 bind，因此不存在竞态窗口）。
    /// 绑定前的内存锚点并入库，不覆盖已持久化的行。
    pub fn bind(&self, path: &Path) {
        let mut store = self
            .inner
            .store
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if store.is_some() {
            return;
        }
        match open_store(path) {
            Ok(mutex) => {
                let connection = mutex.into_inner()
                    .unwrap_or_else(|poison| poison.into_inner());
                // 内存兜底里攒下的锚点并入；已持久化的行优先。
                let fallback: Vec<(String, String, u32)> = self
                    .inner
                    .fallback
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .iter()
                    .map(|(key, (msg_id, seq))| (key.clone(), msg_id.clone(), *seq))
                    .collect();
                {
                    for (key, msg_id, seq) in &fallback {
                        let _ = connection.execute(
                            "INSERT INTO passive_windows (room_key, msg_id, seq, observed_at)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(room_key) DO NOTHING",
                            rusqlite::params![key, msg_id, seq, now_millis()],
                        );
                    }
                }
                *store = Some(connection);
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!("adapter.qqbot.state_open_failed", error = error)
                );
            }
        }
    }

    /// 入站记录：这个房间刚收到一条 `msg_id`（被动回复的窗口锚点，seq 归零）。
    pub fn observe(&self, room: &RoomId, msg_id: &str) {
        let key = room_key(room);
        let mut fallback = self
            .inner
            .fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(store) = self.inner.store.lock().unwrap().as_ref() {
            let _ = store.execute(
                "INSERT INTO passive_windows (room_key, msg_id, seq, observed_at)
                 VALUES (?1, ?2, 0, ?3)
                 ON CONFLICT(room_key) DO UPDATE SET msg_id = excluded.msg_id, seq = 0, observed_at = excluded.observed_at",
                rusqlite::params![key, msg_id, now_millis()],
            );
            return;
        }
        fallback.insert(key, (msg_id.to_owned(), 0));
    }

    /// 取这个房间的回复锚点并占用下一个 `msg_seq`。没有锚点 = 不能主动发。
    pub fn next_seq(&self, room: &RoomId) -> Option<(String, u32)> {
        let key = room_key(room);
        let mut fallback = self
            .inner
            .fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match self.inner.store.lock().unwrap().as_ref() {
            Some(store) => {
                let (msg_id, seq): (String, u32) = store
                    .query_row(
                        "SELECT msg_id, seq FROM passive_windows WHERE room_key = ?1",
                        rusqlite::params![key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .ok()?;
                store
                    .execute(
                        "UPDATE passive_windows SET seq = seq + 1 WHERE room_key = ?1",
                        rusqlite::params![key],
                    )
                    .ok()?;
                Some((msg_id, seq + 1))
            }
            None => {
                let entry = fallback.get_mut(&key)?;
                entry.1 += 1;
                Some((entry.0.clone(), entry.1))
            }
        }
    }
}

fn open_store(path: &Path) -> Result<Mutex<rusqlite::Connection>, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = rusqlite::Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute(
            "CREATE TABLE IF NOT EXISTS passive_windows (
                room_key    TEXT PRIMARY KEY,
                msg_id      TEXT NOT NULL,
                seq         INTEGER NOT NULL DEFAULT 0,
                observed_at INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|error| error.to_string())?;
    Ok(Mutex::new(connection))
}

fn room_key(room: &RoomId) -> String {
    room.path.join("/")
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
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
            .send(
                &room,
                &[Element::Image {
                    url: "https://x/y.png".into(),
                    alt: None,
                }],
            )
            .expect_err("富媒体应明确报错");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }

    #[test]
    fn bound_store_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("qqbot-bind-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("adapter.db");
        let state = ReplyState::new();
        state.observe(&room("group", "R1"), "msgid-old");
        state.bind(&path);
        // 重开一个新实例（模拟桥重启）：持久化的窗口仍在，新消息照常覆盖。
        let revived = ReplyState::new();
        revived.bind(&path);
        revived.observe(&room("group", "R1"), "msgid-new");
        let (msg_id, _) = revived
            .next_seq(&room("group", "R1"))
            .expect("重启后窗口仍在");
        assert_eq!(msg_id, "msgid-new");
    }
}
