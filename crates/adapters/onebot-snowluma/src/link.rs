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
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acb_kernel::{AdapterStatus, Connection, EventQueue, Outbound, RoomId};
use serde_json::{Value, json};
use tungstenite::error::Error as WsError;
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

/// 一条连接喂事件时需要的一组只读上下文。
struct PumpContext<'a> {
    events: &'a EventQueue,
    connection: &'a str,
    admin_qqs: &'a [String],
    status: &'a AdapterStatus,
}

/// 昵称补查线程要带着走的一组 owned 上下文。
struct PokeContext {
    events: EventQueue,
    connection: String,
    admin_qqs: Vec<String>,
    status: AdapterStatus,
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
                handle_text(text.as_str(), &ctx, slot, &mut waiting);
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

fn handle_text(
    text: &str,
    ctx: &PumpContext<'_>,
    slot: &LinkSlot,
    waiting: &mut HashMap<String, Sender<Result<Value, LinkError>>>,
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

    // 有 echo 就是动作的回包，交给等它的那个人。
    if let Some(echo) = value.get("echo").and_then(Value::as_str) {
        if let Some(reply) = waiting.remove(echo) {
            let _ = reply.send(Ok(value));
        }
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

/// 守护一条平台连接：连不上就等着重试，断了就重连，直到要退出。
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
                let (sender, actions) = mpsc::channel();
                slot.install(sender.clone());

                // 顺手问一下对面登录的是哪个号：订阅里的 send_account 要是跟它对不上，
                // 自己说的话就会被当成别人说的，严重时会自己回自己。这种事必须当场喊出来。
                {
                    let sender = sender.clone();
                    let account = account.clone();
                    let id = id.clone();
                    let status = status.clone();
                    let min_version = min_version.clone();
                    std::thread::spawn(move || {
                        probe_identity(&sender, &account, &id, &status, &min_version)
                    });
                }

                let ctx = PumpContext {
                    events: &events,
                    connection: &id,
                    admin_qqs: &admin_qqs,
                    status: &status,
                };
                let outcome = pump(ws, actions, ctx, &slot, &shutdown);
                slot.clear();
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
        let config = Config::parse(
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
        let config = Config::parse(
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
            token = "t"

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
}
