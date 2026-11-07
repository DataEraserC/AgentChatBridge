//! 与 OneBot 11 实现之间的 WebSocket 长连接。
//!
//! 一个线程同时管读和写：读带 20ms 超时，超时不是错，只是「这会儿没消息」，回头把排队的
//! 动作写出去再读。这样做的好处是不需要拆 socket，也不会出现「读线程阻塞、回复发不出去」
//! 的僵局。
//!
//! 两处特意设计的地方：
//! - **动作走队列**：`send` 只是把动作塞进队列，由这个线程统一写 socket 并按 `echo` 配对
//!   回包，写读永远只有一个人，不会串字节。
//! - **事件走队列**：适配器只把 `RecordDraft` 丢进 core 的 `EventQueue`，不拿桥的锁，
//!   也不直接调 `ingest`。真正入库由 core 的 drain 线程统一做。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acb_kernel::{AdapterStatus, Connection, EventQueue, Outbound, RoomId};
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tungstenite::error::Error as WsError;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

use crate::event::{self, Parsed};
use crate::outbound::OneBotSnowLumaOutbound;

/// 适配器声明的 SnowLuma 兼容下限。上游没有兼容承诺，只做「低于下限告警」，不断连。
pub const DEFAULT_SNOWLUMA_MIN: &str = "2.4.0";

/// 一条 WebSocket 连接。
pub type Ws = WebSocket<TcpStream>;

/// 每次读最多等这么久，然后回头看看有没有动作要写。
pub const READ_TICK: Duration = Duration::from_millis(20);
/// 握手最多等这么久。握手期间不能用读超时，否则正常网络也会被判超时。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 断线之后隔多久重连。
pub const RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// 适配器错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("{}", acb_i18n::t!("adapter.onebot11.bad_url", url = .0))]
    BadUrl(String),
    #[error("{}", acb_i18n::t!("adapter.onebot11.connect_failed", address = address, detail = detail))]
    Connect { address: String, detail: String },
    #[error("{}", acb_i18n::t!("adapter.onebot11.handshake_failed", detail = .0))]
    Handshake(String),
    #[error("{}", acb_i18n::t!("adapter.onebot11.transport_error", detail = .0))]
    Transport(String),
    #[error("{}", acb_i18n::t!("adapter.onebot11.platform_timeout"))]
    Timeout,
}

/// 一个待发出的动作，外加等结果的地方。
pub struct Action {
    pub body: Value,
    pub reply: Sender<Result<Value, LinkError>>,
}

/// 当前这条连接的发送端。断线时清空、重连时换新的，出站那边只管往里塞。
#[derive(Clone, Default)]
pub struct LinkSlot {
    inner: Arc<Mutex<Option<Sender<Action>>>>,
}

impl LinkSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 这条连接现在通不通。
    pub fn connected(&self) -> bool {
        self.lock().is_some()
    }

    fn install(&self, sender: Sender<Action>) {
        *self.lock() = Some(sender);
    }

    fn clear(&self) {
        *self.lock() = None;
    }

    pub(crate) fn current(&self) -> Option<Sender<Action>> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Sender<Action>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// 连上 OneBot 的 WebSocket 服务端（正向 WS）。
///
/// `token` 是 OneBot 那边开的访问令牌，填了就放在 `Authorization: Bearer` 头里——
/// NapCat / Lagrange / go-cqhttp 都认这个头；地址里带 `?access_token=` 也行，那样令牌
/// 会跟着请求路径送过去。
///
/// 只支持 `ws://`：`wss://` 要额外拉 TLS 依赖，等真有需要再加。
pub fn connect(url: &str, token: Option<&str>, read_timeout: Duration) -> Result<Ws, LinkError> {
    let address = endpoint_of(url)?;
    let request = build_request(url, token)?;
    let stream = TcpStream::connect(&address).map_err(|error| LinkError::Connect {
        address: address.clone(),
        detail: error.to_string(),
    })?;
    let _ = stream.set_nodelay(true);
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .map_err(|error| LinkError::Connect {
            address: address.clone(),
            detail: error.to_string(),
        })?;

    let (ws, _response) = tungstenite::client(request, stream)
        .map_err(|error| LinkError::Handshake(error.to_string()))?;

    // 握手之后换成短超时：读一下没消息就回去看有没有动作要写。
    let _ = ws.get_ref().set_read_timeout(Some(read_timeout));
    Ok(ws)
}

/// 组一个带令牌的握手请求。
fn build_request(
    url: &str,
    token: Option<&str>,
) -> Result<tungstenite::http::Request<()>, LinkError> {
    use tungstenite::client::IntoClientRequest;

    let mut request = url
        .into_client_request()
        .map_err(|error| LinkError::BadUrl(format!("{url} ({error})")))?;

    if let Some(token) = token {
        let token = token.trim();
        if !token.is_empty() {
            let value = tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| {
                    LinkError::BadUrl(acb_i18n::t!("adapter.onebot11.token_header_invalid"))
                })?;
            request
                .headers_mut()
                .insert(tungstenite::http::header::AUTHORIZATION, value);
        }
    }

    Ok(request)
}

/// 从地址里取 `主机:端口`。
///
/// 认三种常见写法：`ws://h:1`、`ws://h:1/path`、`ws://h:1/path?access_token=x`。
/// 查询串是给人看的（很多 OneBot 实现用 `access_token` 参数鉴权），不属于地址。
fn endpoint_of(url: &str) -> Result<String, LinkError> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| LinkError::BadUrl(url.to_owned()))?;
    let authority = rest
        .split(['/', '?'])
        .next()
        .ok_or_else(|| LinkError::BadUrl(url.to_owned()))?;
    if authority.is_empty() {
        return Err(LinkError::BadUrl(url.to_owned()));
    }
    if authority.contains(':') {
        Ok(authority.to_owned())
    } else {
        Ok(format!("{authority}:80"))
    }
}

/// 反向 WS 的监听地址从哪里来。
///
/// 优先 `options.listen`（如 `0.0.0.0:3001`）；也接受 `url = "ws://0.0.0.0:3001"`，
/// 这样配置里不用多一个字段。两种都没有就返回 `None`，由工厂报明确错误。
pub(crate) fn server_listen(connection: &Connection) -> Option<String> {
    let option = connection
        .options
        .get("listen")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(listen) = option {
        return Some(listen.to_owned());
    }
    connection.url.as_deref().and_then(|url| {
        endpoint_of(url).ok().or_else(|| {
            let address = url.split(['/', '?']).next()?.trim();
            (!address.is_empty()).then(|| address.to_owned())
        })
    })
}

/// 服务端握手时验 token。SnowLuma 的反向 WS 常见写法是 `?access_token=`；
/// 也接受 `Authorization: Bearer`，两种都只做常量时间语义上「直接比较」，不记录 token。
fn server_token_ok(request: &Request, token: Option<&str>) -> bool {
    let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) else {
        return true;
    };

    let bearer = request
        .headers()
        .get(tungstenite::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    if bearer.is_some_and(|value| value == token) {
        return true;
    }

    request
        .uri()
        .query()
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "access_token").then(|| percent_decode(value))
            })
        })
        .is_some_and(|value| value == token)
}

/// 只解码 `%XX`；token 一般用不到，但地址里出现编码时不能直接比较原文。
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &value[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 一条连接喂事件时需要的一组只读上下文。
struct PumpContext<'a> {
    events: &'a EventQueue,
    connection: &'a str,
    admin_qqs: &'a [String],
    status: &'a AdapterStatus,
    dedup: &'a Arc<Mutex<EventDedup>>,
}

/// 昵称补查线程要带着走的一组 owned 上下文。
struct PokeContext {
    events: EventQueue,
    connection: String,
    admin_qqs: Vec<String>,
    status: AdapterStatus,
    dedup: Arc<Mutex<EventDedup>>,
}

/// 一条下载流的累计状态。只记元信息与分块数量，不把 chunk 的 base64 攒进内存。
#[derive(Debug, Default)]
struct StreamAcc {
    info: Option<Value>,
    chunks: usize,
    bytes: u64,
}

/// SnowLuma 的 HTTP 上报没有事件 ID、不重试、不去重；WS 与 HTTP 是并列通道，
/// 同一条事件可能两边都到。这里按原始事件 JSON 的哈希做短期去重。
const DEDUP_CAPACITY: usize = 4096;
const DEDUP_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Default)]
struct EventDedup {
    seen: HashMap<String, Instant>,
}

impl EventDedup {
    fn duplicate(&mut self, value: &Value) -> bool {
        let Some(key) = event_dedup_key(value) else {
            return false;
        };
        let now = Instant::now();
        self.seen
            .retain(|_, at| now.duration_since(*at) <= DEDUP_TTL);
        if let Some(at) = self.seen.get_mut(&key) {
            *at = now;
            return true;
        }
        if self.seen.len() >= DEDUP_CAPACITY {
            // 到上限时按时间淘汰最老的一批，别让高事件量把表撑爆，也别一次清空
            // 导致刚去重过的事件又被收一遍。
            let evict = DEDUP_CAPACITY / 4;
            let mut entries: Vec<(String, Instant)> = self
                .seen
                .iter()
                .map(|(key, at)| (key.clone(), *at))
                .collect();
            entries.sort_unstable_by_key(|(_, at)| *at);
            for (key, _) in entries.into_iter().take(evict) {
                self.seen.remove(&key);
            }
        }
        self.seen.insert(key, now);
        false
    }
}

/// 心跳每次都该刷新健康状态，不参与去重。
fn event_dedup_key(value: &Value) -> Option<String> {
    let post = value.get("post_type").and_then(Value::as_str)?;
    if post == "meta_event" {
        return None;
    }
    let raw = serde_json::to_string(value).ok()?;
    let mut hasher = Sha1::new();
    hasher.update(raw.as_bytes());
    let digest = hasher.finalize();
    Some(format!("{post}-{}", hex(digest.as_slice())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 一条连接的主循环：写动作、喂事件、读回包。连不上或断了就返回。
fn pump(
    mut ws: Ws,
    actions: Receiver<Action>,
    ctx: PumpContext<'_>,
    slot: &LinkSlot,
    shutdown: &AtomicBool,
) -> Result<(), LinkError> {
    let mut waiting: HashMap<String, Sender<Result<Value, LinkError>>> = HashMap::new();
    let mut streams: HashMap<String, StreamAcc> = HashMap::new();

    while !shutdown.load(Ordering::Relaxed) {
        // 1. 把排队的动作写出去。写和读在同一个线程，必须严格串行。
        loop {
            match actions.try_recv() {
                Ok(action) => {
                    if let Some(echo) = action.body.get("echo").and_then(Value::as_str) {
                        waiting.insert(echo.to_owned(), action.reply);
                    }
                    if let Err(error) = ws.send(Message::text(action.body.to_string())) {
                        fail_all(
                            &mut waiting,
                            &acb_i18n::t!("adapter.onebot11.connection_lost_sending"),
                        );
                        return Err(LinkError::Transport(error.to_string()));
                    }
                }
                Err(TryRecvError::Empty) => break,
                // 出站那边没了（进程要退），当成正常收工。
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // 2. 读一帧。事件由 handle_text 丢进 core 的入站队列。
        match ws.read() {
            Ok(Message::Text(text)) => {
                handle_text(text.as_str(), &ctx, slot, &mut waiting, &mut streams);
            }
            // tungstenite 收到 ping 会自动回 pong，这里只兜一手。
            Ok(Message::Ping(payload)) => {
                let _ = ws.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(WsError::Io(error)) if is_timeout(&error) => continue,
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => return Ok(()),
            Err(error) => {
                fail_all(
                    &mut waiting,
                    &acb_i18n::t!("adapter.onebot11.connection_lost_reading"),
                );
                return Err(LinkError::Transport(error.to_string()));
            }
        }
    }

    fail_all(
        &mut waiting,
        &acb_i18n::t!("adapter.onebot11.bridge_shutdown"),
    );
    streams.clear();
    Ok(())
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

/// 下载流帧的数据类型。上传的单请求确认不带 `data_type`，会走普通 echo 路径。
fn stream_frame_kind(value: &Value) -> Option<&str> {
    let kind = value
        .get("data")
        .and_then(|data| data.get("data_type"))
        .and_then(Value::as_str)?;
    matches!(
        kind,
        "file_info" | "file_chunk" | "data_chunk" | "file_complete" | "data_complete" | "error"
    )
    .then_some(kind)
}

fn handle_text(
    text: &str,
    ctx: &PumpContext<'_>,
    slot: &LinkSlot,
    waiting: &mut HashMap<String, Sender<Result<Value, LinkError>>>,
    streams: &mut HashMap<String, StreamAcc>,
) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.non_json_content",
                connection = ctx.connection
            )
        );
        return;
    };

    // 下载流是多帧：file_info → file_chunk* → file_complete，全部带同一个 echo。
    // 先把这些帧累计起来，终帧才把结果交回等动作的人；chunk 的 base64 不进内存。
    if let Some(kind) = stream_frame_kind(&value).map(str::to_owned) {
        let Some(echo) = value.get("echo").and_then(Value::as_str).map(str::to_owned) else {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.stream_frame_no_echo",
                    connection = ctx.connection
                )
            );
            return;
        };
        let data = value.get("data").cloned().unwrap_or(Value::Null);
        let acc = streams.entry(echo.clone()).or_default();
        match kind.as_str() {
            "file_info" => acc.info = Some(data.clone()),
            "file_chunk" | "data_chunk" => {
                acc.chunks = acc.chunks.saturating_add(1);
                let size = data
                    .get("size")
                    .and_then(Value::as_u64)
                    .or_else(|| {
                        data.get("data")
                            .and_then(Value::as_str)
                            .map(|chunk| chunk.len() as u64)
                    })
                    .unwrap_or_default();
                acc.bytes = acc.bytes.saturating_add(size);
            }
            _ => {}
        }
        let terminal = matches!(kind.as_str(), "file_complete" | "data_complete" | "error");
        if !terminal {
            return;
        }
        let Some(reply) = waiting.remove(&echo) else {
            streams.remove(&echo);
            return;
        };
        let response = if kind == "error" {
            value
        } else {
            json!({
                "status": "ok",
                "retcode": 0,
                "stream": "stream-action",
                "echo": echo,
                "data": {
                    "type": "stream_summary",
                    "file_info": acc.info,
                    "chunks": acc.chunks,
                    "bytes": acc.bytes,
                    "complete": data,
                }
            })
        };
        streams.remove(&echo);
        let _ = reply.send(Ok(response));
        return;
    }

    // 有 echo 就是动作的回包，交给等它的那个人。
    if let Some(echo) = value.get("echo").and_then(Value::as_str) {
        if let Some(reply) = waiting.remove(echo) {
            let _ = reply.send(Ok(value));
        }
        return;
    }

    // WS 与 HTTP 上报可能把同一条事件各送一次；短期哈希去重挡住重复入库。
    if ctx
        .dedup
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .duplicate(&value)
    {
        return;
    }

    // 拍一拍常不带 sender：先异步查群成员资料补昵称，再入库。查询线程通过
    // LinkSlot 发动作，由本线程继续读写回包，不会把 pump 卡死。
    if let Some((group, user)) = event::poke_missing_sender(&value) {
        spawn_poke_lookup(
            value,
            slot.clone(),
            PokeContext {
                events: ctx.events.clone(),
                connection: ctx.connection.to_owned(),
                admin_qqs: ctx.admin_qqs.to_vec(),
                status: ctx.status.clone(),
                dedup: ctx.dedup.clone(),
            },
            group,
            user,
        );
        return;
    }

    submit_event(ctx, &value);
}

/// 解析并提交一条事件。正常路径和昵称补查线程都走这里。
fn submit_event(ctx: &PumpContext<'_>, value: &Value) {
    match event::parse(ctx.connection, value, ctx.admin_qqs) {
        Ok(Parsed::AccountStatus { online, detail }) => {
            ctx.status.account(online, None, &detail);
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.account_status",
                    connection = ctx.connection,
                    status = detail
                )
            );
        }
        Ok(Parsed::Heartbeat) => ctx.status.heartbeat(),
        Ok(Parsed::Record(draft)) => {
            if let Err(error) = ctx.events.submit(draft) {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.onebot11.inbound_queue_rejected",
                        connection = ctx.connection,
                        reason = error
                    )
                );
            }
        }
        Ok(Parsed::Ignored(reason)) => {
            // 心跳之类，不值得每次都打日志。
            if reason.starts_with(acb_i18n::tr("adapter.onebot11.unrecognized_prefix").as_str()) {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.onebot11.ignored_event",
                        connection = ctx.connection,
                        reason = reason
                    )
                );
            }
        }
        Err(error) => eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.event_parse_failed",
                connection = ctx.connection,
                error = error
            )
        ),
    }
}

/// 查一次群成员资料，把 `sender` 注入原事件后再入库；查不到就沿用号码。
fn spawn_poke_lookup(raw: Value, slot: LinkSlot, ctx: PokeContext, group: String, user: String) {
    std::thread::spawn(move || {
        let mut enriched = raw.clone();
        if let Ok(room) = RoomId::new(&ctx.connection, format!("group/{group}")) {
            let mut outbound = OneBotSnowLumaOutbound::new(slot, &ctx.connection);
            let params = json!({
                "group_id": number_value(&group),
                "user_id": number_value(&user),
            });
            match outbound.call(&room, "member.info", &params) {
                Ok(outcome) => {
                    let data = &outcome.value;
                    let nickname = data.get("nickname").and_then(Value::as_str).unwrap_or("");
                    let card = data.get("card").and_then(Value::as_str).unwrap_or("");
                    let role = data.get("role").and_then(Value::as_str).unwrap_or("");
                    if let Value::Object(map) = &mut enriched {
                        map.insert(
                            "sender".to_owned(),
                            json!({
                                "user_id": user,
                                "nickname": nickname,
                                "card": card,
                                "role": role,
                            }),
                        );
                    }
                }
                Err(error) => eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.onebot11.nickname_lookup_failed",
                        connection = ctx.connection,
                        user = user,
                        error = error
                    )
                ),
            }
        }
        let pump_ctx = PumpContext {
            events: &ctx.events,
            connection: &ctx.connection,
            admin_qqs: &ctx.admin_qqs,
            status: &ctx.status,
            dedup: &ctx.dedup,
        };
        submit_event(&pump_ctx, &enriched);
    });
}

/// 数字形式的号传数字，非数字（少见）原样传字符串。
fn number_value(text: &str) -> Value {
    text.parse::<u64>()
        .map(Value::from)
        .unwrap_or_else(|_| Value::from(text))
}

fn fail_all(waiting: &mut HashMap<String, Sender<Result<Value, LinkError>>>, reason: &str) {
    for (_, reply) in waiting.drain() {
        let _ = reply.send(Err(LinkError::Transport(reason.to_owned())));
    }
}

/// 守护一条平台连接：正向连平台，或者反向等平台连进来。
///
/// 只拿连接配置快照，不拿 `Bridge`：适配器在 core 外，收发的记录走 `events`
/// 队列，账号核对用配置里的 `account`。
pub fn supervise(
    slot: LinkSlot,
    connection: Connection,
    shutdown: Arc<AtomicBool>,
    events: EventQueue,
    status: AdapterStatus,
) {
    let dedup = Arc::new(Mutex::new(EventDedup::default()));
    spawn_http_events(&connection, &shutdown, &events, &status, &dedup);
    if connection.transport.as_str() == "ws-server" {
        supervise_server(slot, connection, shutdown, events, status, dedup);
    } else {
        supervise_client(slot, connection, shutdown, events, status, dedup);
    }
}

/// 正向 WS：桥主动连 OneBot 的服务端，断了重连。
fn supervise_client(
    slot: LinkSlot,
    connection: Connection,
    shutdown: Arc<AtomicBool>,
    events: EventQueue,
    status: AdapterStatus,
    dedup: Arc<Mutex<EventDedup>>,
) {
    let url = connection.url.clone().unwrap_or_default();
    let token = connection.token.clone();
    let id = connection.id.clone();
    let admin_qqs = connection.admin_qqs.clone();
    let account = connection.account.clone();
    let min_version = version_floor(&connection.options);

    while !shutdown.load(Ordering::Relaxed) {
        status.attempt();
        match connect(&url, token.as_deref(), READ_TICK) {
            Ok(ws) => {
                status.connected();
                eprintln!(
                    "{}",
                    acb_i18n::t!("adapter.onebot11.connected", id = id, url = url)
                );
                let outcome = drive_ws(
                    ws,
                    &slot,
                    DriveContext {
                        connection: &id,
                        account: &account,
                        admin_qqs: &admin_qqs,
                        min_version: &min_version,
                        events: &events,
                        status: &status,
                        dedup: &dedup,
                    },
                    &shutdown,
                );
                if shutdown.load(Ordering::Relaxed) {
                    status.disconnected("closed", "adapter shutdown");
                    break;
                }
                match outcome {
                    Ok(()) => {
                        status.disconnected("closed", "connection closed");
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.onebot11.reconnect_closed",
                                id = id,
                                delay = format!("{RECONNECT_DELAY:?}")
                            )
                        )
                    }
                    Err(error) => {
                        status.disconnected("unreachable", &error.to_string());
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.onebot11.reconnect_error",
                                id = id,
                                error = error,
                                delay = format!("{RECONNECT_DELAY:?}")
                            )
                        )
                    }
                }
            }
            Err(error) => {
                status.disconnected("unreachable", &error.to_string());
                eprintln!(
                    "{}",
                    acb_i18n::t!("adapter.onebot11.adapter_error", id = id, error = error)
                );
            }
        }

        // 分小段睡，退出时不用等满一整个间隔。
        let mut waited = Duration::ZERO;
        while waited < RECONNECT_DELAY && !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
            waited += Duration::from_millis(100);
        }
    }
}

/// HTTP 事件上报的请求体上限。事件是一小段 JSON，1 MiB 已经非常宽。
const HTTP_MAX_BODY: usize = 1024 * 1024;
const HTTP_MAX_HEADERS: usize = 16 * 1024;
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// `httpClients` 上报线程的上下文。事件走同一个 `EventQueue`，与 WS 共用去重表。
struct HttpEventContext {
    events: EventQueue,
    connection: String,
    admin_qqs: Vec<String>,
    status: AdapterStatus,
    dedup: Arc<Mutex<EventDedup>>,
    token: String,
}

/// 可选 HTTP 事件上报（SnowLuma 的 `httpClients`）。
///
/// 只负责收事件，动作仍走 WS/HTTP API 的既有通道。签名按 OneBot 11 规范：
/// `X-Signature: sha1=HMAC-SHA1(accessToken, raw body)`。
fn spawn_http_events(
    connection: &Connection,
    shutdown: &Arc<AtomicBool>,
    events: &EventQueue,
    status: &AdapterStatus,
    dedup: &Arc<Mutex<EventDedup>>,
) {
    let Some(listen) = connection
        .options
        .get("http_listen")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    else {
        return;
    };
    let Some(token) = connection
        .token
        .clone()
        .filter(|token| !token.trim().is_empty())
    else {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.http_listen_requires_token",
                id = connection.id
            )
        );
        return;
    };
    let ctx = HttpEventContext {
        events: events.clone(),
        connection: connection.id.clone(),
        admin_qqs: connection.admin_qqs.clone(),
        status: status.clone(),
        dedup: dedup.clone(),
        token,
    };
    let shutdown = shutdown.clone();
    std::thread::spawn(move || run_http_listener(&listen, &ctx, &shutdown));
}

fn run_http_listener(listen: &str, ctx: &HttpEventContext, shutdown: &AtomicBool) {
    let listener = match TcpListener::bind(listen) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.http_events_listen_failed",
                    connection = ctx.connection,
                    listen = listen,
                    error = error
                )
            );
            return;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.http_events_listen_failed",
                connection = ctx.connection,
                listen = listen,
                error = error
            )
        );
        return;
    }
    eprintln!(
        "{}",
        acb_i18n::t!(
            "adapter.onebot11.http_events_listening",
            connection = ctx.connection,
            listen = listen
        )
    );
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => handle_http_event(stream, ctx),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.onebot11.http_events_listen_failed",
                        connection = ctx.connection,
                        listen = listen,
                        error = error
                    )
                );
                break;
            }
        }
    }
}

fn handle_http_event(mut stream: TcpStream, ctx: &HttpEventContext) {
    let _ = stream.set_read_timeout(Some(HTTP_READ_TIMEOUT));
    let (method, headers, body) = match read_http_request(&mut stream) {
        Ok(request) => request,
        Err(status) => {
            respond(&mut stream, status);
            return;
        }
    };
    if method != "POST" {
        respond(&mut stream, 405);
        return;
    }
    let signature = headers
        .get("x-signature")
        .and_then(|value| strip_signature_prefix(value));
    let Some(signature) = signature.and_then(hex_decode) else {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.http_events_auth_failed",
                connection = ctx.connection,
                reason = "missing X-Signature"
            )
        );
        respond(&mut stream, 401);
        return;
    };
    let expected = hmac_sha1(ctx.token.as_bytes(), &body);
    if !constant_time_eq(&expected, &signature) {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.http_events_auth_failed",
                connection = ctx.connection,
                reason = "bad HMAC-SHA1"
            )
        );
        respond(&mut stream, 401);
        return;
    }
    let Ok(value) = serde_json::from_slice::<Value>(&body) else {
        respond(&mut stream, 400);
        return;
    };
    if ctx
        .dedup
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .duplicate(&value)
    {
        respond(&mut stream, 204);
        return;
    }
    let pump_ctx = PumpContext {
        events: &ctx.events,
        connection: &ctx.connection,
        admin_qqs: &ctx.admin_qqs,
        status: &ctx.status,
        dedup: &ctx.dedup,
    };
    submit_event(&pump_ctx, &value);
    respond(&mut stream, 204);
}

/// 解析后的 HTTP 请求：方法、头、原始 body。
type HttpRequest = (String, HashMap<String, String>, Vec<u8>);

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, u16> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).map_err(|_| 400u16)?;
        if read == 0 {
            return Err(400);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if buffer.len() > HTTP_MAX_HEADERS {
            return Err(431);
        }
    }
    let split = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or(400u16)?;
    let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let method = request_line
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if length > HTTP_MAX_BODY {
        return Err(413);
    }
    let mut body = buffer[split + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).map_err(|_| 400u16)?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
        if body.len() > HTTP_MAX_BODY {
            return Err(413);
        }
    }
    body.truncate(length.min(body.len()));
    Ok((method, headers, body))
}

fn respond(stream: &mut TcpStream, status: u16) {
    let reason = match status {
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
}

fn strip_signature_prefix(value: &str) -> Option<&str> {
    let (prefix, rest) = value.split_once('=')?;
    if prefix.eq_ignore_ascii_case("sha1") {
        Some(rest.trim())
    } else {
        None
    }
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    for pair in text.as_bytes().chunks(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        bytes.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(bytes)
}

/// HMAC-SHA1（RFC 2104）。SnowLuma 的 `accessToken` 在这里是 HMAC 密钥，不是 Bearer。
fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        let digest = Sha1::digest(key);
        block[..20].copy_from_slice(digest.as_slice());
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for index in 0..64 {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner = Sha1::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner = inner.finalize();
    let mut outer = Sha1::new();
    outer.update(outer_pad);
    outer.update(inner);
    let digest = outer.finalize();
    let mut out = [0u8; 20];
    out.copy_from_slice(digest.as_slice());
    out
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 反向 WS：桥监听一个地址，SnowLuma 作为客户端连进来。一次只服务一条活动连接。
///
/// 握手时校验 token（`?access_token=` 或 `Authorization: Bearer`）；不匹配直接 401。
/// 监听失败是配置/环境问题，标记 `failed` 后退出，不在原地无限重试。
#[allow(clippy::result_large_err)] // tungstenite 的 ErrorResponse 是 http::Response，体量固定。
fn supervise_server(
    slot: LinkSlot,
    connection: Connection,
    shutdown: Arc<AtomicBool>,
    events: EventQueue,
    status: AdapterStatus,
    dedup: Arc<Mutex<EventDedup>>,
) {
    let Some(listen) = server_listen(&connection) else {
        status.failed("config", "ws-server requires options.listen");
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.adapter_error",
                id = connection.id,
                error = acb_i18n::t!("adapter.onebot11.ws_server_requires_listen")
            )
        );
        return;
    };
    let listener = match TcpListener::bind(&listen) {
        Ok(listener) => listener,
        Err(error) => {
            status.failed("listen", &error.to_string());
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.ws_server_bind_failed",
                    id = connection.id,
                    listen = listen,
                    error = error
                )
            );
            return;
        }
    };
    let _ = listener.set_nonblocking(true);
    let id = connection.id.clone();
    let admin_qqs = connection.admin_qqs.clone();
    let account = connection.account.clone();
    let min_version = version_floor(&connection.options);
    eprintln!(
        "{}",
        acb_i18n::t!(
            "adapter.onebot11.ws_server_listening",
            id = id,
            listen = listen
        )
    );

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, remote)) => {
                status.attempt();
                let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
                let token = connection.token.clone();
                let authorize = move |request: &Request,
                                      response: Response|
                      -> Result<Response, ErrorResponse> {
                    if server_token_ok(request, token.as_deref()) {
                        Ok(response)
                    } else {
                        let mut denied =
                            tungstenite::http::Response::new(Some("unauthorized".to_owned()));
                        *denied.status_mut() = tungstenite::http::StatusCode::UNAUTHORIZED;
                        Err(denied)
                    }
                };
                match tungstenite::accept_hdr(stream, authorize) {
                    Ok(ws) => {
                        let _ = ws.get_ref().set_read_timeout(Some(READ_TICK));
                        status.connected();
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.onebot11.ws_server_connected",
                                id = id,
                                remote = remote
                            )
                        );
                        let outcome = drive_ws(
                            ws,
                            &slot,
                            DriveContext {
                                connection: &id,
                                account: &account,
                                admin_qqs: &admin_qqs,
                                min_version: &min_version,
                                events: &events,
                                status: &status,
                                dedup: &dedup,
                            },
                            &shutdown,
                        );
                        if shutdown.load(Ordering::Relaxed) {
                            status.disconnected("closed", "adapter shutdown");
                            break;
                        }
                        match outcome {
                            Ok(()) => status.disconnected("closed", "connection closed"),
                            Err(error) => {
                                status.disconnected("unreachable", &error.to_string());
                                eprintln!(
                                    "{}",
                                    acb_i18n::t!(
                                        "adapter.onebot11.ws_server_connection_error",
                                        id = id,
                                        error = error
                                    )
                                );
                            }
                        }
                    }
                    Err(error) => {
                        status.disconnected("auth", &error.to_string());
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.onebot11.ws_server_rejected",
                                id = id,
                                remote = remote,
                                error = error
                            )
                        );
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                status.failed("listen", &error.to_string());
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.onebot11.ws_server_accept_failed",
                        id = id,
                        error = error
                    )
                );
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// 连接建立后要带着走的一组 borrowed 上下文。
struct DriveContext<'a> {
    connection: &'a str,
    account: &'a str,
    admin_qqs: &'a [String],
    min_version: &'a str,
    events: &'a EventQueue,
    status: &'a AdapterStatus,
    dedup: &'a Arc<Mutex<EventDedup>>,
}

/// 连接建立后的公共部分：安装发送槽、探测身份、跑 pump。
fn drive_ws(
    ws: Ws,
    slot: &LinkSlot,
    ctx: DriveContext<'_>,
    shutdown: &AtomicBool,
) -> Result<(), LinkError> {
    let (sender, actions) = mpsc::channel();
    slot.install(sender.clone());

    // 顺手问一下对面登录的是哪个号：订阅里的 send_account 要是跟它对不上，
    // 自己说的话就会被当成别人说的，严重时会自己回自己。这种事必须当场喊出来。
    {
        let sender = sender.clone();
        let account = ctx.account.to_owned();
        let connection = ctx.connection.to_owned();
        let probe_status = (*ctx.status).clone();
        let min_version = ctx.min_version.to_owned();
        std::thread::spawn(move || {
            probe_identity(&sender, &account, &connection, &probe_status, &min_version)
        });
    }

    let pump_ctx = PumpContext {
        events: ctx.events,
        connection: ctx.connection,
        admin_qqs: ctx.admin_qqs,
        status: ctx.status,
        dedup: ctx.dedup,
    };
    let outcome = pump(ws, actions, pump_ctx, slot, shutdown);
    slot.clear();
    outcome
}

/// 连上之后问三件事：登录账号、协议/实现版本、账号在线状态。
///
/// 三件事都是「取不到不影响收发」：拿不到就记 `unknown`，不阻塞连接、不猜。
fn probe_identity(
    sender: &Sender<Action>,
    configured: &str,
    connection: &str,
    status: &AdapterStatus,
    min_version: &str,
) {
    if let Some(value) = ask(sender, "get_login_info") {
        check_login(&value, configured, connection);
    } else {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.login_probe_timeout",
                connection = connection
            )
        );
    }

    let version = match ask(sender, "get_version_info") {
        Some(value) => parse_version_info(&value, min_version),
        None => {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.version_probe_timeout",
                    connection = connection
                )
            );
            empty_version_info(min_version)
        }
    };
    eprintln!(
        "{}",
        acb_i18n::t!(
            "adapter.onebot11.version_info",
            connection = connection,
            protocol = version.protocol_version.as_deref().unwrap_or("unknown"),
            implementation = version.implementation_name.as_deref().unwrap_or("unknown"),
            implementation_version = version
                .implementation_version
                .as_deref()
                .unwrap_or("unknown"),
            adapter = version.adapter_version,
            floor = version.compat_floor.as_deref().unwrap_or("unknown"),
            compat = version.compat_state.as_deref().unwrap_or("unknown")
        )
    );
    if version.compat_state.as_deref() == Some("below_declared_floor") {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.snowluma_below_min",
                connection = connection,
                version = version
                    .implementation_version
                    .as_deref()
                    .unwrap_or("unknown"),
                floor = version.compat_floor.as_deref().unwrap_or("unknown")
            )
        );
    }
    status.version(version);

    if let Some(value) = ask(sender, "get_status") {
        if let Some((online, good)) = parse_status(&value) {
            status.account(online, good, "get_status");
        }
    } else {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.status_probe_timeout",
                connection = connection
            )
        );
    }
}

fn empty_version_info(min_version: &str) -> acb_kernel::PlatformVersionInfo {
    acb_kernel::PlatformVersionInfo {
        protocol_kind: "onebot11".to_owned(),
        protocol_name: "OneBot".to_owned(),
        protocol_version: None,
        implementation_name: None,
        implementation_version: None,
        adapter_version: env!("CARGO_PKG_VERSION").to_owned(),
        compat_floor: Some(min_version.to_owned()),
        compat_state: Some("unknown".to_owned()),
    }
}

/// `get_version_info` 回包 → 三层版本。取不到或形状不对都落 `unknown`，不 panic。
fn parse_version_info(value: &Value, min_version: &str) -> acb_kernel::PlatformVersionInfo {
    let mut version = empty_version_info(min_version);
    if !retcode_ok(value) {
        return version;
    }
    let data = value.get("data").unwrap_or(&Value::Null);
    version.protocol_version =
        normalize_protocol_version(text_field(data, "protocol_version").as_deref());
    version.implementation_name = text_field(data, "app_name");
    version.implementation_version = text_field(data, "app_version");
    version.compat_state = Some(
        compat_state(
            version.implementation_name.as_deref(),
            version.implementation_version.as_deref(),
            min_version,
        )
        .to_owned(),
    );
    version
}

/// `get_status` 回包 → `(online, good)`。缺字段或 retcode != 0 都返回 None。
fn parse_status(value: &Value) -> Option<(bool, Option<bool>)> {
    if !retcode_ok(value) {
        return None;
    }
    let data = value.get("data").unwrap_or(&Value::Null);
    let online = data.get("online").and_then(Value::as_bool)?;
    let good = data.get("good").and_then(Value::as_bool);
    Some((online, good))
}

fn retcode_ok(value: &Value) -> bool {
    value
        .get("retcode")
        .and_then(Value::as_i64)
        .is_none_or(|code| code == 0)
}

/// 只认 OneBot 11 的两种写法；识别不了就记 `unknown`，不切协议解析器。
fn normalize_protocol_version(value: Option<&str>) -> Option<String> {
    let value = value?.trim().to_ascii_lowercase();
    matches!(value.as_str(), "11" | "v11").then(|| "v11".to_owned())
}

/// SnowLuma 兼容下限：只在实现名能确认是 SnowLuma 时比较，其它实现记 unknown。
fn compat_state(name: Option<&str>, version: Option<&str>, floor: &str) -> &'static str {
    let Some(name) = name else {
        return "unknown";
    };
    if !name.to_ascii_lowercase().contains("snowluma") {
        return "unknown";
    }
    let Some(actual) = version.and_then(version_triplet) else {
        return "unknown";
    };
    let Some(floor) = version_triplet(floor) else {
        return "unknown";
    };
    if actual < floor {
        "below_declared_floor"
    } else {
        "ok"
    }
}

/// 宽松的 `major.minor.patch` 解析：允许 `v` 前缀、缺 minor/patch 按 0；
/// 带预发布/构建后缀的版本按 unknown 处理，不猜大小。
fn version_triplet(text: &str) -> Option<(u64, u64, u64)> {
    let text = text.trim().trim_start_matches(['v', 'V']);
    if text.is_empty() {
        return None;
    }
    let mut parts = text.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts
        .next()
        .map_or(Some(0), |part| part.parse::<u64>().ok())?;
    let patch = parts
        .next()
        .map_or(Some(0), |part| part.parse::<u64>().ok())?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn version_floor(options: &Value) -> String {
    options
        .get("snowluma_min_version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_SNOWLUMA_MIN)
        .to_owned()
}

/// 发一个动作并等回包。超时/连接断开都算「没问到」。
fn ask(sender: &Sender<Action>, action: &str) -> Option<Value> {
    let (reply_tx, reply_rx) = mpsc::channel();
    let request = Action {
        body: json!({
            "action": action,
            "params": {},
            "echo": format!("acb-probe-{action}"),
        }),
        reply: reply_tx,
    };
    if sender.send(request).is_err() {
        return None;
    }
    match reply_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(value)) => Some(value),
        _ => None,
    }
}

fn text_field(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// 账号核对：对面登录的号必须和配置一致，否则自己的话会被当成别人说的。
fn check_login(value: &Value, configured: &str, connection: &str) {
    let self_id = match value.get("data").and_then(|data| data.get("user_id")) {
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) => text.clone(),
        _ => {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.login_probe_missing",
                    connection = connection
                )
            );
            return;
        }
    };
    eprintln!(
        "{}",
        acb_i18n::t!(
            "adapter.onebot11.login_account",
            connection = connection,
            self_id = self_id
        )
    );

    if !configured.is_empty() && configured != self_id {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.onebot11.account_mismatch",
                connection = connection,
                self_id = self_id,
                configured = configured
            )
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::Config;

    #[test]
    fn endpoint_is_taken_from_the_url() {
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001").expect("合法"),
            "127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001/onebot/v11/ws").expect("合法"),
            "127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_of("ws://example.com").expect("合法"),
            "example.com:80"
        );
    }

    #[test]
    fn query_string_is_not_part_of_the_address() {
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001?access_token=abc").expect("合法"),
            "127.0.0.1:3001"
        );
        assert_eq!(
            endpoint_of("ws://127.0.0.1:3001/ws?access_token=abc").expect("合法"),
            "127.0.0.1:3001"
        );
    }

    #[test]
    fn token_goes_into_the_authorization_header() {
        let request = build_request("ws://127.0.0.1:3001", Some("abc")).expect("能组请求");
        assert_eq!(
            request
                .headers()
                .get(tungstenite::http::header::AUTHORIZATION)
                .expect("应当有令牌头")
                .to_str()
                .expect("可见字符"),
            "Bearer abc"
        );
        assert_eq!(request.uri().path(), "/");
    }

    #[test]
    fn no_token_means_no_header_and_the_path_is_kept() {
        let request =
            build_request("ws://127.0.0.1:3001/onebot/v11/ws?x=1", None).expect("能组请求");
        assert!(
            request
                .headers()
                .get(tungstenite::http::header::AUTHORIZATION)
                .is_none()
        );
        assert_eq!(request.uri().path(), "/onebot/v11/ws");
        assert_eq!(request.uri().query(), Some("x=1"));
    }

    #[test]
    fn tls_urls_are_refused_with_a_clear_message() {
        assert!(matches!(
            endpoint_of("wss://example.com/ws"),
            Err(LinkError::BadUrl(_))
        ));
        assert!(matches!(endpoint_of("http://x"), Err(LinkError::BadUrl(_))));
    }

    #[test]
    fn version_fields_are_read_only_when_non_empty() {
        let data = json!({
            "protocol_version": "v11",
            "app_name": "snowluma",
            "app_version": "  ",
        });
        assert_eq!(
            text_field(&data, "protocol_version").as_deref(),
            Some("v11")
        );
        assert_eq!(text_field(&data, "app_name").as_deref(), Some("snowluma"));
        assert_eq!(text_field(&data, "app_version"), None);
        assert_eq!(text_field(&data, "missing"), None);
    }

    #[test]
    fn version_probe_normalizes_protocol_and_records_compat() {
        let value = json!({
            "retcode": 0,
            "data": {
                "protocol_version": "11",
                "app_name": "SnowLuma",
                "app_version": "2.4.0",
            }
        });
        let info = parse_version_info(&value, DEFAULT_SNOWLUMA_MIN);
        assert_eq!(info.protocol_kind, "onebot11");
        assert_eq!(info.protocol_name, "OneBot");
        assert_eq!(info.protocol_version.as_deref(), Some("v11"));
        assert_eq!(info.implementation_name.as_deref(), Some("SnowLuma"));
        assert_eq!(info.implementation_version.as_deref(), Some("2.4.0"));
        assert_eq!(info.compat_floor.as_deref(), Some(DEFAULT_SNOWLUMA_MIN));
        assert_eq!(info.compat_state.as_deref(), Some("ok"));
        assert_eq!(info.adapter_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn version_probe_falls_back_to_unknown_without_panicking() {
        let empty = json!({"retcode": 0, "data": {}});
        let info = parse_version_info(&empty, DEFAULT_SNOWLUMA_MIN);
        assert_eq!(info.protocol_version, None);
        assert_eq!(info.implementation_name, None);
        assert_eq!(info.implementation_version, None);
        assert_eq!(info.compat_state.as_deref(), Some("unknown"));

        let failed = json!({
            "retcode": 1,
            "data": {"protocol_version": "v11", "app_name": "snowluma", "app_version": "2.4.0"}
        });
        let info = parse_version_info(&failed, DEFAULT_SNOWLUMA_MIN);
        assert_eq!(info.protocol_version, None, "retcode != 0 不采信");
        assert_eq!(info.implementation_name, None);
        assert_eq!(info.compat_state.as_deref(), Some("unknown"));

        let wrong_protocol = json!({
            "retcode": 0,
            "data": {"protocol_version": "12", "app_name": "snowluma", "app_version": "2.4.0"}
        });
        let info = parse_version_info(&wrong_protocol, DEFAULT_SNOWLUMA_MIN);
        assert_eq!(info.protocol_version, None, "识别不了的协议版本记 unknown");
        assert_eq!(info.implementation_name.as_deref(), Some("snowluma"));
    }

    #[test]
    fn status_probe_requires_retcode_and_online() {
        let online = json!({"retcode": 0, "data": {"online": true, "good": true}});
        assert_eq!(parse_status(&online), Some((true, Some(true))));
        let offline = json!({"retcode": 0, "data": {"online": false}});
        assert_eq!(parse_status(&offline), Some((false, None)));
        let missing = json!({"retcode": 0, "data": {"good": true}});
        assert_eq!(parse_status(&missing), None);
        let failed = json!({"retcode": 1, "data": {"online": true}});
        assert_eq!(parse_status(&failed), None);
    }

    #[test]
    fn compat_floor_marks_below_and_ignores_other_implementations() {
        assert_eq!(
            compat_state(Some("snowluma"), Some("2.3.9"), "2.4.0"),
            "below_declared_floor"
        );
        assert_eq!(compat_state(Some("SnowLuma"), Some("2.4"), "2.4.0"), "ok");
        assert_eq!(
            compat_state(Some("snowluma"), Some("2.4.0-beta"), "2.4.0"),
            "unknown",
            "带后缀的版本不猜大小"
        );
        assert_eq!(
            compat_state(Some("napcat"), Some("2.3.9"), "2.4.0"),
            "unknown",
            "其它实现不套 SnowLuma 下限"
        );
        assert_eq!(compat_state(None, None, "2.4.0"), "unknown");
    }

    #[test]
    fn compat_floor_comes_from_options_with_default() {
        assert_eq!(version_floor(&json!({})), DEFAULT_SNOWLUMA_MIN);
        assert_eq!(
            version_floor(&json!({"snowluma_min_version": " 2.6.1 "})),
            "2.6.1"
        );
        assert_eq!(
            version_floor(&json!({"snowluma_min_version": "  "})),
            DEFAULT_SNOWLUMA_MIN
        );
    }

    #[test]
    fn ask_returns_none_when_the_action_queue_is_closed() {
        let (sender, receiver) = mpsc::channel();
        drop(receiver);
        assert!(ask(&sender, "get_version_info").is_none());
    }

    #[test]
    fn probe_identity_writes_version_and_status_to_dependencies() {
        let config = Config::parse_fixture(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"
            "#,
        )
        .expect("配置合法");
        let deps = acb_kernel::DependencyRegistry::from_config(&config);
        let status = AdapterStatus::new("qq", deps.clone());
        let (sender, receiver) = mpsc::channel::<Action>();

        std::thread::spawn(move || {
            while let Ok(action) = receiver.recv() {
                let name = action.body["action"].as_str().unwrap_or_default();
                let response = match name {
                    "get_login_info" => json!({"retcode": 0, "data": {"user_id": 10001}}),
                    "get_version_info" => json!({
                        "retcode": 0,
                        "data": {
                            "protocol_version": "11",
                            "app_name": "SnowLuma",
                            "app_version": "2.4.1",
                        }
                    }),
                    "get_status" => json!({"retcode": 0, "data": {"online": true, "good": true}}),
                    _ => json!({"retcode": 1404}),
                };
                let _ = action.reply.send(Ok(response));
            }
        });

        probe_identity(&sender, "10001", "qq", &status, DEFAULT_SNOWLUMA_MIN);

        let qq = deps
            .platform_items()
            .into_iter()
            .find(|item| item["id"] == "qq")
            .expect("有 qq 平台条目");
        assert_eq!(qq["version"]["protocol"]["version"], "v11");
        assert_eq!(qq["version"]["impl"]["name"], "SnowLuma");
        assert_eq!(qq["version"]["impl"]["version"], "2.4.1");
        assert_eq!(
            qq["version"]["adapter"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(qq["version"]["adapter"]["compat"]["state"], "ok");
        assert_eq!(qq["accountOnline"], true);
        assert_eq!(qq["accountGood"], true);
    }

    #[test]
    fn the_configured_account_is_read_back_for_the_right_connection() {
        let config = Config::parse_fixture(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "999"

            [[connections]]
            id = "qq2"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3002"
            account = "888"

            [[devices]]
            id = "main"
            persona = "p"
            memory = "m"
            token = { secret = "t" }

            [[devices.subscriptions]]
            connection = "qq"
            room = "group/1"
            "#,
        )
        .expect("配置合法");

        assert_eq!(config.connection("qq").expect("有").account, "999");
        assert_eq!(config.connection("qq2").expect("有").account, "888");
        assert!(config.connection("kook").is_none());
    }

    fn request(uri: &str, bearer: Option<&str>) -> Request {
        let mut request = Request::new(());
        *request.uri_mut() = uri.parse().expect("合法 URI");
        if let Some(token) = bearer {
            request.headers_mut().insert(
                tungstenite::http::header::AUTHORIZATION,
                tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
                    .expect("合法 header"),
            );
        }
        request
    }

    #[test]
    fn ws_server_listen_prefers_options_and_falls_back_to_url() {
        let config = Config::parse_fixture(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            transport = "ws-server"
            options = { listen = "127.0.0.1:3999" }
            account = "10001"

            [[connections]]
            id = "qq2"
            platform = "qq"
            kind = "onebot11"
            transport = "ws-server"
            url = "ws://127.0.0.1:3998"
            account = "10002"
            "#,
        )
        .expect("配置合法");

        assert_eq!(
            server_listen(config.connection("qq").expect("有")),
            Some("127.0.0.1:3999".to_owned())
        );
        assert_eq!(
            server_listen(config.connection("qq2").expect("有")),
            Some("127.0.0.1:3998".to_owned())
        );
    }

    #[test]
    fn reverse_ws_token_accepts_query_and_bearer() {
        let token = Some("s3cret");
        assert!(server_token_ok(
            &request("ws://127.0.0.1:3001/ws?access_token=s3cret", None),
            token
        ));
        assert!(server_token_ok(
            &request("ws://127.0.0.1:3001/ws", Some("s3cret")),
            token
        ));
        assert!(server_token_ok(
            &request("ws://127.0.0.1:3001/ws?access_token=s3%63ret", None),
            token
        ));
        assert!(!server_token_ok(
            &request("ws://127.0.0.1:3001/ws?access_token=wrong", None),
            token
        ));
        assert!(!server_token_ok(
            &request("ws://127.0.0.1:3001/ws", None),
            token
        ));

        // 没配 token 时不要求鉴权。
        assert!(server_token_ok(
            &request("ws://127.0.0.1:3001/ws", None),
            None
        ));
    }

    #[test]
    fn hmac_sha1_matches_the_rfc2202_vector() {
        // RFC 2202 test case 1.
        let mac = hmac_sha1(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(hex(&mac), "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79");
        assert!(constant_time_eq(&mac, &mac));
        assert!(!constant_time_eq(&mac, &[0u8; 20]));
        assert_eq!(hex_decode("00FF").expect("合法 hex"), vec![0, 255]);
        assert!(hex_decode("xyz").is_none());
        assert_eq!(strip_signature_prefix("sha1=abc"), Some("abc"));
        assert_eq!(strip_signature_prefix("SHA1=abc"), Some("abc"));
        assert_eq!(strip_signature_prefix("Bearer abc"), None);
    }

    #[test]
    fn event_dedup_drops_repeats_but_not_heartbeats() {
        let mut dedup = EventDedup::default();
        let event = json!({
            "post_type": "message",
            "message_type": "group",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "message_id": 1,
            "time": 1_793_000_000,
            "message": [{"type": "text", "data": {"text": "hi"}}]
        });
        assert!(!dedup.duplicate(&event), "第一次不算重复");
        assert!(dedup.duplicate(&event), "完全相同的第二次要丢掉");

        let heartbeat = json!({
            "post_type": "meta_event",
            "meta_event_type": "heartbeat",
            "self_id": 999,
            "time": 1_793_000_000,
        });
        assert!(!dedup.duplicate(&heartbeat));
        assert!(!dedup.duplicate(&heartbeat), "心跳永远不该被去重");
    }

    #[test]
    fn event_dedup_evicts_the_oldest_entries_at_capacity() {
        let mut dedup = EventDedup::default();
        for index in 0..DEDUP_CAPACITY + 64 {
            let event = json!({
                "post_type": "notice",
                "notice_type": "group_upload",
                "self_id": 999,
                "group_id": 456,
                "user_id": 10001,
                "file": {"id": format!("f{index}")},
                "time": 1_793_000_000,
            });
            assert!(!dedup.duplicate(&event), "每条新事件都该入库");
        }
        assert!(
            dedup.seen.len() <= DEDUP_CAPACITY,
            "去重表必须保持有界：{}",
            dedup.seen.len()
        );
        // 最老的一批已被按时间淘汰；刚插入的仍能命中重复。
        let newest = json!({
            "post_type": "notice",
            "notice_type": "group_upload",
            "self_id": 999,
            "group_id": 456,
            "user_id": 10001,
            "file": {"id": format!("f{}", DEDUP_CAPACITY + 63)},
            "time": 1_793_000_000,
        });
        assert!(dedup.duplicate(&newest), "最新条目应仍在表里");
    }

    #[test]
    fn download_stream_frames_are_accumulated_until_complete() {
        let config = Config::parse_fixture(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"
            "#,
        )
        .expect("配置合法");
        let status = AdapterStatus::new("qq", acb_kernel::DependencyRegistry::from_config(&config));
        let (events, _receiver) = EventQueue::channel();
        let dedup = Arc::new(Mutex::new(EventDedup::default()));
        let admin_qqs: Vec<String> = Vec::new();
        let ctx = PumpContext {
            events: &events,
            connection: "qq",
            admin_qqs: &admin_qqs,
            status: &status,
            dedup: &dedup,
        };
        let slot = LinkSlot::new();
        let mut waiting = HashMap::new();
        let (sender, receiver) = mpsc::channel();
        waiting.insert("e1".to_owned(), sender);
        let mut streams = HashMap::new();

        handle_text(
            r#"{"echo":"e1","data":{"data_type":"file_info","file_name":"a.bin","file_size":30}}"#,
            &ctx,
            &slot,
            &mut waiting,
            &mut streams,
        );
        assert!(receiver.try_recv().is_err(), "首帧不能提前交结果");
        handle_text(
            r#"{"echo":"e1","data":{"data_type":"file_chunk","size":10}}"#,
            &ctx,
            &slot,
            &mut waiting,
            &mut streams,
        );
        handle_text(
            r#"{"echo":"e1","data":{"data_type":"file_chunk","size":20}}"#,
            &ctx,
            &slot,
            &mut waiting,
            &mut streams,
        );
        assert!(receiver.try_recv().is_err(), "chunk 阶段不能提前交结果");
        handle_text(
            r#"{"echo":"e1","data":{"data_type":"file_complete","total_bytes":30}}"#,
            &ctx,
            &slot,
            &mut waiting,
            &mut streams,
        );
        let response = receiver
            .try_recv()
            .expect("终帧要回结果")
            .expect("不是连接错误");
        assert_eq!(response["data"]["type"], "stream_summary");
        assert_eq!(response["data"]["file_info"]["file_name"], "a.bin");
        assert_eq!(response["data"]["chunks"], 2);
        assert_eq!(response["data"]["bytes"], 30);
        assert!(waiting.is_empty(), "终帧之后不该继续挂着");
        assert!(streams.is_empty(), "终帧之后流状态要清掉");
    }
}
