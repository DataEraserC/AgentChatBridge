//! 网关连接：桥作 WS 客户端，连 QQ 开放平台的 wss 网关。
//!
//! 生命周期（官方 v2 协议）：
//! 1. 连上 `wss://api.sgroup.qq.com/websocket`，收到 Hello（op 10，`d.heartbeat_interval`）；
//! 2. 发 Identify（op 2，`d.token = "QQBot <access_token>"`，intents，shard）；
//!    断线重连且手里有 session 时改发 Resume（op 6，`d.{token, session_id, seq}`）；
//! 3. 读循环：Dispatch（op 0，带 `s` 序号）解成记录；定时发 Heartbeat（op 1，`d` = 最后序号）；
//!    Reconnect（op 7）/ InvalidSession（op 9）按协议重连。
//!
//! 断线退避 1s→2s→…→60s；退出信号随时打断重连循环。

use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use acb_kernel::{AdapterStatus, Connection, EventQueue};

use crate::event::{self, Parsed};
use crate::outbound::ReplyState;
use crate::token::TokenManager;

const DEFAULT_WS_URL: &str = "wss://api.sgroup.qq.com/websocket";
/// 群 @ / 好友单聊（1 << 25）+ 频道私信（1 << 12）。
const INTENTS: u64 = (1 << 25) | (1 << 12);
/// 读循环的轮询粒度：没消息就回去看看心跳到点没有、退出信号亮没亮。
const READ_TICK: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// Hello 不给 interval 时的保底值（官方通常 30 秒级别）。
const FALLBACK_HEARTBEAT: Duration = Duration::from_secs(30);

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("token: {0}")]
    Token(String),
    #[error("io: {0}")]
    Io(String),
}

/// 网关连接循环。断线按指数退避重连；退出信号随时打断。
pub fn supervise(
    connection: Connection,
    shutdown: Arc<AtomicBool>,
    events: EventQueue,
    status: AdapterStatus,
    token_manager: TokenManager,
    reply_state: ReplyState,
) {
    let id = connection.id.clone();
    let url = connection
        .url
        .clone()
        .unwrap_or_else(|| DEFAULT_WS_URL.to_owned());
    let mut backoff = Duration::from_secs(1);
    // 会话状态跨重连保留：Resume 失败（InvalidSession）时清掉重新 Identify。
    let session = Arc::new(Mutex::new(SessionState::default()));
    while !shutdown.load(Ordering::Relaxed) {
        status.attempt();
        match run_connection(
            &connection,
            &url,
            &token_manager,
            &reply_state,
            &shutdown,
            &events,
            &status,
            &session,
            &id,
        ) {
            Ok(()) => {
                // 退出信号亮了：正常收工。
                return;
            }
            Err(error) => {
                status.failed("connect", &error.to_string());
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.qqbot.disconnected",
                        id = id,
                        detail = error.to_string()
                    )
                );
            }
        }
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        status.disconnected("backoff", &format!("{backoff:?}"));
        eprintln!(
            "{}",
            acb_i18n::t!("adapter.qqbot.backoff", id = id, secs = backoff.as_secs())
        );
        let deadline = Instant::now() + backoff;
        while Instant::now() < deadline && !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// 一次完整的连接生命周期（连接 → 鉴权 → 读循环）。返回 Ok 表示正常退出（退出信号）。
#[allow(clippy::too_many_arguments)]
fn run_connection(
    connection: &Connection,
    url: &str,
    token_manager: &TokenManager,
    reply_state: &ReplyState,
    shutdown: &Arc<AtomicBool>,
    events: &EventQueue,
    status: &AdapterStatus,
    session: &Arc<Mutex<SessionState>>,
    id: &str,
) -> Result<(), LinkError> {
    let ws = connect(url)?;
    status.connected();
    eprintln!(
        "{}",
        acb_i18n::t!("adapter.qqbot.connected", id = id, url = url)
    );
    let last_seq = AtomicU64::new(0);
    let result = handshake_and_read(
        connection,
        ws,
        token_manager,
        reply_state,
        shutdown,
        events,
        &last_seq,
        session,
    );
    let _ = status.disconnected("gateway_closed", "read loop ended");
    result
}

/// 连上网关。TLS 由 tungstenite 的 rustls 特性处理（wss://）。
fn connect(url: &str) -> Result<Ws, LinkError> {
    let (ws, _response) =
        tungstenite::connect(url).map_err(|error| LinkError::Connect(error.to_string()))?;
    set_stream_timeout(&ws, READ_TICK);
    Ok(ws)
}

fn set_stream_timeout(ws: &Ws, timeout: Duration) {
    let apply = |stream: &TcpStream| {
        let _ = stream.set_read_timeout(Some(timeout));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    };
    match ws.get_ref() {
        MaybeTlsStream::Plain(stream) => apply(stream),
        MaybeTlsStream::Rustls(stream) => apply(stream.get_ref()),
        _ => {}
    }
}

/// Hello → Identify/Resume。成功返回 Heartbeat 间隔。
fn handshake(
    ws: &mut Ws,
    token_manager: &TokenManager,
    session: &Mutex<SessionState>,
) -> Result<Duration, LinkError> {
    let heartbeat;
    loop {
        let message = next_text(ws)?;
        let payload: Value = match parse_frame(&message) {
            Ok(payload) => payload,
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!("adapter.qqbot.frame_parse_failed", error = error)
                );
                continue;
            }
        };
        match payload.get("op").and_then(|value| value.as_u64()) {
            Some(10) => {
                // Hello：interval 毫秒。
                heartbeat = Duration::from_millis(
                    payload
                        .get("d")
                        .and_then(|d| d.get("heartbeat_interval"))
                        .and_then(|value| value.as_u64())
                        .unwrap_or(FALLBACK_HEARTBEAT.as_millis() as u64),
                );
            }
            Some(2) | Some(9) => {
                // 不该在这个阶段出现的 op（服务端直接 InvalidSession）：重握手。
                continue;
            }
            other => {
                return Err(LinkError::Handshake(format!(
                    "expected Hello (op 10), got {other:?}"
                )));
            }
        }
        let state = session
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let frame = match (&state.session_id, state.last_seq > 0) {
            (Some(session_id), true) => {
                // Resume：token 里不能带等级后缀，原样 QQBot <token>。
                json!({
                    "op": 6,
                    "d": {
                        "token": token_manager
                            .auth_header()
                            .map_err(|error| LinkError::Token(error.to_string()))?,
                        "session_id": session_id,
                        "seq": state.last_seq,
                    },
                })
            }
            _ => json!({
                "op": 2,
                "d": {
                    "token": token_manager
                        .auth_header()
                        .map_err(|error| LinkError::Token(error.to_string()))?,
                    "intents": INTENTS,
                    "shard": [0u64, 1u64],
                },
            }),
        };
        ws.send(Message::Text(frame.to_string().into()))
            .map_err(|error| LinkError::Handshake(error.to_string()))?;
        return Ok(heartbeat);
    }
}

/// 读循环：Dispatch 解记录、定时心跳、Reconnect/InvalidSession 触发重连。
#[allow(clippy::too_many_arguments)]
fn handshake_and_read(
    connection: &Connection,
    mut ws: Ws,
    token_manager: &TokenManager,
    reply_state: &ReplyState,
    shutdown: &Arc<AtomicBool>,
    events: &EventQueue,
    last_seq: &AtomicU64,
    session: &Arc<Mutex<SessionState>>,
) -> Result<(), LinkError> {
    let heartbeat = handshake(&mut ws, token_manager, session)?;
    let mut next_heartbeat = Instant::now() + heartbeat;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            let _ = ws.close(None);
            return Ok(());
        }
        // 心跳到点（读超时唤醒轮询）。
        if Instant::now() >= next_heartbeat {
            let seq = last_seq.load(Ordering::Relaxed);
            ws.send(Message::Text(json!({"op": 1, "d": seq}).to_string().into()))
                .map_err(|error| LinkError::Io(error.to_string()))?;
            next_heartbeat = Instant::now() + heartbeat;
        }
        let message = match ws.read() {
            Ok(message) => message,
            Err(tungstenite::Error::Io(error))
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(error) => return Err(LinkError::Io(error.to_string())),
        };
        match message {
            Message::Text(text) => {
                let payload: Value = match parse_frame(&text) {
                    Ok(payload) => payload,
                    Err(error) => {
                        eprintln!(
                            "{}",
                            acb_i18n::t!("adapter.qqbot.frame_parse_failed", error = error)
                        );
                        continue;
                    }
                };
                match payload.get("op").and_then(|value| value.as_u64()) {
                    Some(0) => {
                        // Dispatch：记序号、解事件、进队列。
                        let seq = payload
                            .get("s")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(0);
                        if seq > 0 {
                            last_seq.store(seq, Ordering::Relaxed);
                        }
                        let event_type = payload
                            .get("t")
                            .and_then(|value| value.as_str())
                            .unwrap_or_default()
                            .to_owned();
                        if event_type.is_empty() {
                            continue;
                        }
                        let payload_data = payload.get("d").cloned().unwrap_or(Value::Null);
                        // READY 是控制帧：抓会话 id（Resume 要用）后直接吃掉，
                        // 不能落到 event::parse——否则它会被 unknown 分支包成
                        // notice 记录（room group/READY），造成伪事件噪音。
                        if event_type == "READY" {
                            let session_id = payload_data
                                .get("session_id")
                                .and_then(|value| value.as_str())
                                .map(str::to_owned);
                            session
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .session_id = session_id;
                            continue;
                        }
                        // 每条 dispatch 都留一行：官方平台的场景开关（群/单聊）
                        // 没开时消息根本不推，这里是最便宜的判定点。
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.qqbot.dispatch",
                                connection = connection.id,
                                event = event_type
                            )
                        );
                        match event::parse(
                            &connection.id,
                            &event_type,
                            &payload_data,
                            reply_state,
                            now_millis(),
                        ) {
                            Ok(Parsed::Record(draft)) => {
                                if let Err(error) = events.submit(draft) {
                                    eprintln!(
                                        "{}",
                                        acb_i18n::t!(
                                            "adapter.qqbot.submit_failed",
                                            error = error.to_string()
                                        )
                                    );
                                }
                            }
                            Ok(Parsed::Ignored) => {}
                            Err(error) => eprintln!(
                                "{}",
                                acb_i18n::t!(
                                    "adapter.qqbot.event_parse_failed",
                                    detail = error.to_string()
                                )
                            ),
                        }
                    }
                    Some(11) => {} // HeartbeatAck
                    Some(7) => {
                        // 服务端要求重连：立刻断开走重连流程。
                        let _ = ws.close(None);
                        return Err(LinkError::Io(
                            acb_i18n::t!("adapter.qqbot.server_reconnect").to_owned(),
                        ));
                    }
                    Some(9) => {
                        // InvalidSession：丢弃会话状态，重连后重新 Identify。
                        session
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .clear();
                        let _ = ws.close(None);
                        return Err(LinkError::Io(
                            acb_i18n::t!("adapter.qqbot.invalid_session").to_owned(),
                        ));
                    }
                    _ => {}
                }
            }
            Message::Ping(data) => {
                let _ = ws.send(Message::Pong(data));
            }
            Message::Close(frame) => {
                let _ = ws.close(frame);
                return Err(LinkError::Io(
                    acb_i18n::t!("adapter.qqbot.server_closed").to_owned(),
                ));
            }
            _ => {}
        }
    }
}

/// 会话恢复状态：session_id 来自 READY dispatch；Resume 被拒就清掉重新 Identify。
#[derive(Debug, Clone, Default)]
struct SessionState {
    session_id: Option<String>,
    last_seq: u64,
}

impl SessionState {
    fn clear(&mut self) {
        self.session_id = None;
        self.last_seq = 0;
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or(0)
}

fn parse_frame(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|error| error.to_string())
}

fn next_text(ws: &mut Ws) -> Result<String, LinkError> {
    loop {
        match ws.read() {
            Ok(Message::Text(text)) => return Ok(text.to_string()),
            Ok(Message::Ping(data)) => {
                let _ = ws.send(Message::Pong(data));
            }
            Ok(_) => continue,
            Err(tungstenite::Error::Io(error)) => return Err(LinkError::Io(error.to_string())),
            Err(error) => return Err(LinkError::Handshake(error.to_string())),
        }
    }
}
