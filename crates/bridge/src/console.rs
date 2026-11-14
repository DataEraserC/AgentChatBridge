//! 桥侧统一控制台：一个极简 HTTP 服务（ADR-0018）。
//!
//! webapi + webui（用户 2026-10-31 裁定：配置页面要有，做成 webapi + webui 且带鉴权；
//! 参考旧 Node 版 bridge.js 控制台的安全做法）。用标准库手写 HTTP（不引依赖）：
//! - `GET /`：单文件管理页（`console.html`，include 进二进制）。浏览器没法随手发
//!   Authorization 头，所以页面用 `/?token=<控制台token>` 进门，JS 把 token 存进
//!   localStorage，之后所有 `/api/*` 请求都带 `Authorization: Bearer`。
//! - `/api/*`：状态、会话（含桥↔harness 映射与重置）、反馈、配置只读视图。
//! - 鉴权：每条请求都要 token（Bearer 头或 `?token=`，常量时间比较）。
//! - 安全响应头（X-Frame-Options / CSP / nosniff / no-store / no-referrer）照抄旧版。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, CONSOLE_CONFIG_FILENAME, ConfigSource, ControlRegistry, Cursor, DependencyRegistry,
    QueueStats, RouteWorker, Store, StuckUnread,
};
use acb_proto::Frame;
use serde_json::json;

/// 投过多少次还没确认就算「卡住」。与 D7 的提示阈值保持一致（`unread_redelivered` 从 2 起算，
/// 这是警告线）。
const STUCK_UNREAD_THRESHOLD: u32 = 3;

/// `/api/status` 的队列视图。入站与路由共用同一套字段，路由额外带 busy。
fn queue_status(
    inbound: &acb_kernel::QueueSnapshot,
    route: &acb_kernel::QueueSnapshot,
    busy: usize,
) -> serde_json::Value {
    json!({
        "inbound": {
            "depth": inbound.depth,
            "capacity": inbound.capacity,
            "highWatermark": inbound.high_watermark,
            "warnPercent": inbound.warn_percent,
            "rejected": inbound.rejected,
            "warned": inbound.warned,
        },
        "route": {
            "depth": route.depth,
            "capacity": route.capacity,
            "busy": busy,
            "highWatermark": route.high_watermark,
            "warnPercent": route.warn_percent,
            "rejected": route.rejected,
            "warned": route.warned,
        },
    })
}

/// 管理页。单文件、零外部依赖：改页面就是改这个 HTML，编译时嵌进二进制。
const CONSOLE_HTML: &str = include_str!("console.html");

/// 概览页「最近失败」的统计窗口（秒）。比 in-band 去重窗口长，
/// 因为要看的是「这段故障持续了多久」，不是「刚才那条通知发没发」。
const RECENT_FAILURE_WINDOW_SECS: u64 = 900;

/// 没带 token 打开 `/` 时给的一小块登录页：弹窗要 token，拼进 URL 重进。
/// 跟旧 Node 版同一招——不让完整页面在未授权时先渲染出来。
/// 文案全部来自 locale 表，按请求 locale 渲染；登录前拿不到 `/api/i18n`。
fn login_html(locale: &str) -> String {
    let title = acb_i18n::tr_in(locale, "bridge.console.login_title");
    let prompt = acb_i18n::tr_in(locale, "bridge.console.login_prompt");
    let placeholder = acb_i18n::tr_in(locale, "bridge.console.login_placeholder");
    let enter = acb_i18n::tr_in(locale, "bridge.console.login_enter");
    format!(
        r#"<!doctype html><html lang="{locale}"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<style>
  :root {{ color-scheme:dark; --bg:#14161a; --surface:#1b1e24; --border:#2f343d; --text:#dfe3ea; --muted:#939aa6; --accent:#4f9cf9; --space-2:8px; --space-3:12px; --space-4:16px; --space-6:24px; --radius:8px; --control:32px; }}
  * {{ box-sizing:border-box; }}
  body {{ margin:0; min-height:100vh; display:grid; place-items:center; background:var(--bg); color:var(--text); font:14px/1.5 system-ui,"PingFang SC","Microsoft YaHei",sans-serif; }}
  form {{ width:min(360px, calc(100vw - 32px)); background:var(--surface); border:1px solid var(--border); border-radius:var(--radius); padding:var(--space-6); }}
  h1 {{ font-size:16px; line-height:1.4; margin:0 0 var(--space-1,4px); font-weight:600; }}
  p {{ margin:0 0 var(--space-4); color:var(--muted); font-size:12px; line-height:1.5; }}
  input {{ width:100%; height:var(--control); padding:0 var(--space-2); background:var(--bg); color:var(--text); border:1px solid var(--border); border-radius:6px; font:inherit; }}
  input:focus-visible {{ outline:2px solid var(--accent); outline-offset:2px; }}
  button {{ width:100%; height:var(--control); margin-top:var(--space-3); background:var(--accent); color:#fff; border:1px solid var(--accent); border-radius:6px; font:inherit; cursor:pointer; }}
  button:focus-visible {{ outline:2px solid var(--accent); outline-offset:2px; }}
  @media (max-width:480px) {{ button, input {{ min-height:44px; }} }}
</style>
<body>
<form onsubmit="event.preventDefault();go()">
<h1>{title}</h1>
<p>{prompt}</p>
<input id="t" type="password" placeholder="{placeholder}" aria-label="{placeholder}" autocomplete="current-password" autofocus>
<button type="submit">{enter}</button>
</form>
<script>function go(){{var t=document.getElementById('t').value;if(t)location.href='/?token='+encodeURIComponent(t)}}</script>
</body></html>"#
    )
}

/// 大小写不敏感的请求头查找。
fn header_value<'a>(header_text: &'a str, name: &str) -> Option<&'a str> {
    header_text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// 请求 locale：`?locale=` 优先，其次 `Accept-Language`，最后进程默认。
fn request_locale(header_text: &str, query: &str) -> &'static str {
    query_param(query, "locale")
        .filter(|value| !value.is_empty())
        .map(acb_i18n::normalize_locale)
        .or_else(|| {
            header_value(header_text, "accept-language")
                .and_then(acb_i18n::locale_from_accept_language)
        })
        .unwrap_or_else(acb_i18n::locale)
}

/// 渲染一个 key + 参数错误；`detail` 只放无法 key 化的底层原文。
fn error_json(message: acb_i18n::Message, detail: Option<String>) -> String {
    let locale = acb_i18n::request_locale();
    match detail {
        Some(detail) => json!({"error": message.render(locale), "detail": detail}).to_string(),
        None => json!({"error": message.render(locale)}).to_string(),
    }
}

fn error_response(
    status: u16,
    message: acb_i18n::Message,
    detail: Option<String>,
) -> (u16, String) {
    (status, error_json(message, detail))
}

/// 配置保存失败：除了 error/detail 还带 `saved: false`（页面据此留在编辑器）。
/// 新建会话时临时补出来的令牌来源。`Drop` 里回滚，除非显式 `commit`。
#[derive(Default)]
struct SessionSecretMint {
    fragment_path: Option<PathBuf>,
    /// `Some(None)` = 原来没有这个文件；`Some(Some(text))` = 原内容。
    fragment_previous: Option<Option<String>>,
    token_paths: Vec<PathBuf>,
    names: Vec<String>,
    /// 明文令牌只在本进程内存里待一会儿：落库要加密，落文件要 0600。
    tokens: Vec<(String, String)>,
    committed: bool,
}

impl SessionSecretMint {
    fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|listed| listed == name)
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SessionSecretMint {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Some(path) = &self.fragment_path {
            match &self.fragment_previous {
                Some(Some(text)) => {
                    let _ = std::fs::write(path, text);
                }
                Some(None) => {
                    let _ = std::fs::remove_file(path);
                }
                None => {}
            }
        }
        for path in &self.token_paths {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 补丁后凭空消失的对象：没被显式 `delete`，却不在新集合里。
///
/// 返回第一条，够用——发现问题就整单拒绝，不需要列全。
fn lost_object(
    before: &acb_kernel::ConfigObjects,
    after: &acb_kernel::ConfigObjects,
    ops: &[serde_json::Value],
) -> Option<(&'static str, String)> {
    let deleted: Vec<(String, String)> = ops
        .iter()
        .filter(|op| op.get("op").and_then(serde_json::Value::as_str) == Some("delete"))
        .map(|op| {
            (
                op.get("section")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                op.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect();
    for kind in acb_kernel::ConfigObjectKind::ALL {
        let section = match kind {
            acb_kernel::ConfigObjectKind::Connection => "connection",
            acb_kernel::ConfigObjectKind::Harness => "harness",
            acb_kernel::ConfigObjectKind::Persona => "persona",
            acb_kernel::ConfigObjectKind::Memory => "memory",
            acb_kernel::ConfigObjectKind::Session => "session",
        };
        for object in before.get(kind) {
            if after.get(kind).iter().any(|next| next.id == object.id) {
                continue;
            }
            if deleted
                .iter()
                .any(|(dot, id)| dot == section && *id == object.id)
            {
                continue;
            }
            return Some((section, object.id.clone()));
        }
    }
    None
}

fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'))
}

fn write_private_file(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// base 与 `config.d/*.toml` 里声明过的来源名。只读名字，不碰值。
fn declared_secret_names(config_path: &Path, config_dir: &Path) -> Vec<String> {
    fn collect(path: &Path, out: &mut Vec<String>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(value) = text.parse::<toml::Value>() else {
            return;
        };
        let Some(sources) = value
            .get("secrets")
            .and_then(|secrets| secrets.get("sources"))
            .and_then(toml::Value::as_array)
        else {
            return;
        };
        for source in sources {
            if let Some(name) = source.get("name").and_then(toml::Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    out.push(name.to_owned());
                }
            }
        }
    }

    let mut names = Vec::new();
    collect(config_path, &mut names);
    if let Ok(entries) = std::fs::read_dir(config_dir) {
        let mut fragments: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .collect();
        fragments.sort();
        for fragment in fragments {
            collect(&fragment, &mut names);
        }
    }
    names
}

fn config_error(status: u16, message: acb_i18n::Message, detail: Option<String>) -> (u16, String) {
    let locale = acb_i18n::request_locale();
    let mut body = json!({"error": message.render(locale), "saved": false});
    if let Some(detail) = detail {
        body["detail"] = json!(detail);
    }
    (status, body.to_string())
}

/// 控制台服务。桥本体之外的一条 HTTP 服务，跟接入点（TCP 7700）互不相干。
pub struct Console<S: Store> {
    bridge: Arc<Mutex<Bridge<S>>>,
    controls: ControlRegistry,
    token: String,
    /// base 配置文件路径（`--config`）；控制台保存不再写它。
    config_path: String,
    /// 分片目录；控制台保存写到 `<config_dir>/90-console.toml`。
    config_dir: String,
    /// 本次启动实际加载的配置文件与贡献计数（只读展示）。
    config_sources: Vec<ConfigSource>,
    /// 入站队列统计。
    inbound_stats: QueueStats,
    /// 路由动作 worker（拿队列统计与 busy 数）。
    route_worker: RouteWorker,
    /// 依赖状态表（只读缓存）。
    deps: DependencyRegistry,
    /// 记忆管家状态文件；未启用时 `None`。
    memory_steward: Option<PathBuf>,
    /// 插件动作执行器（按 plugin id 找）。没有动作的插件不会有条目。
    plugin_actions: Vec<Arc<dyn acb_kernel::PluginActionRunner>>,
}

impl<S: Store + Send + 'static> Console<S> {
    // 控制台把桥的各个只读视图都拼在一个服务里；拆成结构体只会让 main 更难读。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bridge: Arc<Mutex<Bridge<S>>>,
        controls: ControlRegistry,
        token: String,
        config_path: String,
        config_dir: String,
        config_sources: Vec<ConfigSource>,
        inbound_stats: QueueStats,
        route_worker: RouteWorker,
        deps: DependencyRegistry,
        memory_steward: Option<PathBuf>,
        plugin_actions: Vec<Arc<dyn acb_kernel::PluginActionRunner>>,
    ) -> Self {
        Self {
            bridge,
            controls,
            token,
            config_path,
            config_dir,
            config_sources,
            inbound_stats,
            route_worker,
            deps,
            memory_steward,
            plugin_actions,
        }
    }

    /// 控制台保存目标：分片目录里的最高优先级独立文件。
    fn console_config_path(&self) -> PathBuf {
        Path::new(&self.config_dir).join(CONSOLE_CONFIG_FILENAME)
    }

    /// 编辑器当前展示的文本：有控制台覆盖层就展示它，否则展示 base。
    /// 控制台可编辑的那份文档：**只**是覆盖层 `90-console.toml`。
    ///
    /// 以前没有覆盖层时退回 `config.toml` 全文，导致表单补丁把 base（含 `[secrets]`、
    /// `[storage]` 这些启动参数）当成覆盖层回灌，解析期直接报
    /// 「90-console.toml must not declare [secrets]」——2026-11-10 现场复现：
    /// 页面上任何一次保存都失败。覆盖层不存在时给空文档，合并由 base 负责。
    fn console_document(&self) -> Result<(String, String), (u16, String)> {
        let overlay = self.console_config_path();
        let text = if overlay.exists() {
            std::fs::read_to_string(&overlay).map_err(|error| {
                error_response(
                    500,
                    acb_i18n::msg!(
                        "bridge.console.read_config_failed",
                        path = overlay.display().to_string()
                    ),
                    Some(error.to_string()),
                )
            })?
        } else {
            String::new()
        };
        Ok((overlay.display().to_string(), text))
    }

    /// 守着监听端口，来一个请求处理一个。
    pub fn serve(listener: TcpListener, console: Arc<Console<S>>, shutdown: Arc<AtomicBool>) {
        listener.set_nonblocking(true).ok();
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let console = Arc::clone(&console);
                    std::thread::spawn(move || {
                        if let Err(error) = console.handle(stream) {
                            eprintln!(
                                "{}",
                                acb_i18n::t!("bridge.console.handle_failed", error = error)
                            );
                        }
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("bridge.console.accept_failed", error = error)
                    );
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }

    fn handle(&self, mut stream: TcpStream) -> std::io::Result<()> {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let mut buffer = Vec::new();
        // 读请求头 + body（简版：读到空行结束头，再按 Content-Length 读 body）。
        let mut header_end = None;
        let mut chunk = [0_u8; 8192];
        loop {
            let read = stream.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(pos) = find_header_end(&buffer) {
                header_end = Some(pos);
                break;
            }
            if buffer.len() > 64 * 1024 {
                break;
            }
        }
        let Some(header_end) = header_end else {
            let body = error_json(acb_i18n::msg!("bridge.console.incomplete_headers"), None);
            return write_response(&mut stream, 400, "application/json", &body, &[]);
        };
        let header_text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let peer = stream.peer_addr().ok().map(|addr| addr.ip());
        let (method, path, auth) = parse_request_line(&header_text);
        let cookie = header_text
            .lines()
            .find_map(|l| l.strip_prefix("Cookie:").map(str::trim))
            .unwrap_or_default()
            .to_owned();
        let locale = request_locale(&header_text, split_query(&path).1);
        acb_i18n::with_locale(locale, || {
            // 读 body（反馈改状态要带 {"status": "...", "note": "..."}）。
            let body = read_body(&mut stream, &buffer[header_end + 4..], &header_text);

            // 浏览器自动请求 favicon 且不带 token；放鉴权前 204 收掉，别给 DevTools 留 401 噪音。
            if method == "GET" && (path == "/favicon.ico" || path == "/apple-touch-icon.png") {
                return write_response(&mut stream, 204, "text/plain", "", &[]);
            }

            let (path, query) = split_query(&path);
            // 三条门路：Authorization: Bearer 头、浏览器 cookie（进门时种下的）、
            // ?token=（登录表单跳转用）。cookie 是刷新后不掉登录的关键：localStorage
            // 在某些嵌入环境（沙箱 iframe）里会被禁/清掉，cookie 由浏览器自己管。
            let query_ok = query_token_matches(query, &self.token);
            let authorized =
                self.authorized(&auth) || query_ok || cookie_token_matches(&cookie, &self.token);

            if !authorized {
                // 页面本身：没 token 时给登录页（不是 JSON），人才能输 token 进来。
                if method == "GET" && path == "/" {
                    let html = login_html(acb_i18n::request_locale());
                    return write_response(
                        &mut stream,
                        401,
                        "text/html; charset=utf-8",
                        &html,
                        &[],
                    );
                }
                let body = error_json(acb_i18n::msg!("bridge.console.unauthorized"), None);
                return write_response(&mut stream, 401, "application/json", &body, &[]);
            }

            // 管理页。route 只管 JSON API。进门带 ?token= 时顺手种个 cookie：
            // 之后刷新/关掉重开都靠它，不用再输一遍令牌。
            if method == "GET" && path == "/" {
                let set_cookie = if query_ok {
                    format!(
                        "Set-Cookie: acb_console={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000\r\n",
                        self.token
                    )
                } else {
                    String::new()
                };
                return write_response(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    CONSOLE_HTML,
                    &[&set_cookie],
                );
            }

            let (status, body) = self.route(&method, path, &body, peer);
            write_response(&mut stream, status, "application/json", &body, &[])
        })
    }

    fn authorized(&self, auth: &str) -> bool {
        // 常量时间比较，避免时序侧信道（token 是敏感凭据）。
        constant_time_eq(auth, &format!("Bearer {}", self.token))
    }

    fn route(
        &self,
        method: &str,
        path: &str,
        body: &str,
        peer: Option<std::net::IpAddr>,
    ) -> (u16, String) {
        // 路径可能带查询串（/api/feedback?status=open&limit=20）。
        let (path, query) = split_query(path);
        match (method, path) {
            // 健康/排空探针：只给本机。仍走控制台 token 鉴权（在 handle 里）。
            ("GET", "/healthz") => {
                if !peer.is_some_and(|ip| ip.is_loopback()) {
                    return error_response(
                        403,
                        acb_i18n::msg!("bridge.console.health_loopback_only"),
                        None,
                    );
                }
                let bridge = lock(&self.bridge);
                let mut snapshot = bridge.drain_snapshot();
                if let serde_json::Value::Object(fields) = &mut snapshot {
                    fields.insert("dependencies".into(), self.deps.summary());
                }
                (200, snapshot.to_string())
            }
            ("GET", "/api/dependencies") => {
                let since = query_param(query, "since").and_then(|value| value.parse::<u64>().ok());
                (200, self.deps.snapshot(since).to_string())
            }
            ("GET", "/api/status") => (200, self.status()),
            ("GET", "/api/connections") => (200, self.connections_view()),
            ("GET", "/api/secrets") => (200, self.secrets_view()),
            ("POST", "/api/secrets") => self.create_secret(body),
            ("GET", "/api/sessions") => (200, self.sessions()),
            // 记忆管家：最近一次同步时间、结果、冲突文件数与失败原因。
            // 管家线程写 status.json，这里只读，不阻塞任何同步动作。
            ("GET", "/api/memory-steward") => (200, self.memory_steward_status()),
            // 插件目录：只读，描述由插件作者声明；不暴露实际 options / 路径 / token。
            // `connections` 是逐平台的运行时连接状态，补上工厂级 health 看不到的部分。
            ("GET", "/api/plugins") => {
                let bridge = lock(&self.bridge);
                let plugins = bridge.plugin_catalog();
                let wake_audit = bridge.wake_audit();
                let registry = match bridge.registry_plugins() {
                    Ok(list) => list,
                    Err(error) => {
                        eprintln!("console: failed to read plugin registry: {error}");
                        Vec::new()
                    }
                };
                let registry_view: Vec<serde_json::Value> = registry
                    .iter()
                    .map(|plugin| {
                        let status =
                            acb_kernel::plugin_registry::check_command(&plugin.command, plugin.command_kind, None);
                        json!({
                            "id": plugin.id,
                            "kind": plugin.kind.as_str(),
                            "trustTier": plugin.trust_tier,
                            "command": plugin.command,
                            "args": plugin.args,
                            "timeoutMs": plugin.timeout_ms,
                            "commandKind": plugin.command_kind.as_str(),
                            "fingerprint": plugin.command_fingerprint,
                            "fingerprintOk": plugin.fingerprint_matches(),
                            "manifest": serde_json::from_str::<serde_json::Value>(&plugin.manifest_json)
                                .unwrap_or(serde_json::Value::Null),
                            "enabled": plugin.enabled,
                            "degraded": if plugin.enabled {
                                Some(status.describe()).filter(|_| !status.is_ok())
                            } else {
                                None
                            },
                            "policy": {
                                "enabled": plugin.policy.enabled,
                                "scopeKind": plugin.policy.scope_kind.as_str(),
                                "scopeId": plugin.policy.scope_id,
                                "capabilities": plugin.policy.capabilities,
                                "lifecycle": plugin.policy.lifecycle.as_str(),
                                "allowUnknown": plugin.policy.allow_unknown,
                            },
                            "storage": {
                                "kind": plugin.storage.kind.as_str(),
                                "locationKind": plugin.storage.location_kind.as_str(),
                                "location": plugin.storage.location,
                                "dataClass": plugin.storage.data_class.as_str(),
                                "backup": plugin.storage.backup,
                                "retention": plugin.storage.retention,
                            },
                            "capabilities": plugin.capabilities.iter().map(|decl| json!({
                                "id": decl.capability_id,
                                "risk": decl.risk,
                                "requiresTicket": decl.requires_ticket,
                                "scopes": decl.scopes,
                                "description": decl.description,
                            })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                (
                    200,
                    json!({
                        "count": plugins.len(),
                        "plugins": plugins,
                        "registry": registry_view,
                        "connections": self.deps.platform_items(),
                        "wakeAudit": wake_audit,
                    })
                    .to_string(),
                )
            }
            // 动态插件卸载/重载：只入队，真正的 dlclose/dlopen 在轮次边界执行。
            ("POST", "/api/plugins/unload") => self.plugin_action(body, false),
            ("POST", "/api/plugins/reload") => self.plugin_action(body, true),
            // 插件配置页（schema 17）：按 manifest 渲染表单，读写校验都在 kernel。
            ("GET", p) if p.starts_with("/api/plugins/") && p.ends_with("/admin") => {
                self.plugin_admin(p)
            }
            ("POST", p) if p.starts_with("/api/plugins/") && p.ends_with("/config") => {
                self.plugin_config_update(p, body)
            }
            ("POST", p) if p.starts_with("/api/plugins/") && p.ends_with("/secret-bind") => {
                self.plugin_secret_bind(p, body)
            }
            ("POST", p) if p.starts_with("/api/plugins/") && p.ends_with("/action") => {
                self.plugin_run_action(p, body)
            }
            ("POST", p)
                if p.starts_with("/api/plugins/")
                    && p.contains("/records/")
                    && p.ends_with("/delete") =>
            {
                self.plugin_record_delete(p, body)
            }
            ("GET", p) if p.starts_with("/api/plugins/") && p.contains("/records/") => {
                self.plugin_records(p)
            }
            ("POST", p) if p.starts_with("/api/plugins/") && p.contains("/records/") => {
                self.plugin_record_put(p, body)
            }
            // 动态扩展目录：模块级目录与轮次边界重载。
            ("GET", "/api/extensions") => (200, self.extensions(query)),
            ("POST", "/api/extensions/reload") => self.reload_extensions(body),
            // 控制台前端 i18n：当前请求 locale + 完整 message map。
            ("GET", "/api/i18n") => (200, acb_i18n::catalog_json_for(acb_i18n::request_locale())),
            ("GET", "/api/feedback") => self.feedback(query),
            ("POST", p) if p.starts_with("/api/feedback/") && p.ends_with("/reply") => {
                self.feedback_reply(p, body)
            }
            ("POST", p) if p.starts_with("/api/feedback/") => self.set_feedback(p, body),
            ("POST", p) if p.starts_with("/api/session/") => self.reset(p),
            // 配置只读视图（方案 A 第一步）。敏感字段在桥这侧就遮掉，完整原文
            // 永远不发给浏览器。编辑写回 + 热重载是第二步（ADR-0009）。
            ("GET", "/api/config") => (200, self.config_view()),
            // TOML 原文编辑（方案 A 第二步，ADR-0009）：读原文 → 管理员改 →
            // 保存时先校验再落盘（备份旧文件），解析通过就暂存进桥，下一轮生效。
            ("GET", "/api/config/toml") => self.config_toml(),
            ("POST", "/api/config/toml") => self.save_config_toml(body),
            // 结构化表单补丁（主入口，ADR-0009「用户不该需要懂文件格式」）：
            // 页面表单 → 一组 op → toml_edit 在原文档上改键（注释排版不丢）→
            // 同一条「校验 → 备份 → 落盘 → 暂存」流水线。
            ("POST", "/api/config/patch") => self.save_config_patch(body),
            ("POST", "/api/config/discard") => {
                let mut bridge = lock(&self.bridge);
                let discarded = bridge.discard_staged_config();
                (200, json!({"ok": true, "discarded": discarded}).to_string())
            }
            // 策略（权限规则 / 房间信任 / 审批路由 / 管理员）。自 schema 13 起这些东西
            // 在库里，不在 config.toml；所以它不走上面那条「改文件再暂存」的路。
            ("GET", "/api/policy") => self.policy_view(),
            ("POST", "/api/policy") => self.save_policy(body),
            ("GET", "/api/model-fallbacks") => {
                self.model_fallbacks_view(query_param(query, "session"))
            }
            ("POST", "/api/model-fallbacks") => self.save_model_fallbacks(body),
            _ => error_response(404, acb_i18n::msg!("bridge.console.no_endpoint"), None),
        }
    }

    /// 连接视图（带来源标注）。
    ///
    /// config.toml 声明的与提供者注入的并存时，`source` 说明这条连接归谁管：
    /// 改错地方会在下次注入/注册时被覆盖回来。token 与 url 查询串沿用
    /// config_view 的遮蔽规则。
    fn connections_view(&self) -> String {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        let providers = bridge.provider_connection_ids();
        let connections = config
            .connections
            .iter()
            .map(|c| {
                let source = providers
                    .iter()
                    .find(|(_, ids)| ids.contains(&c.id))
                    .map(|(kind, _)| format!("provider:{kind}"))
                    .unwrap_or_else(|| "config".to_owned());
                json!({
                    "id": c.id,
                    "platform": c.platform,
                    "kind": c.kind,
                    "transport": c.transport.as_str(),
                    "url": c.url.as_deref().map(mask_url_query).unwrap_or_default(),
                    "token": c.token.as_deref().map(mask_secret),
                    "account": c.account,
                    "admin_qqs": c.admin_qqs,
                    "source": source,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "config_version": bridge.config_version(),
            "connections": connections,
        })
        .to_string()
    }

    /// 记忆管家状态。读的是管家自己原子替换的 status.json；读不到就如实报错。
    fn memory_steward_status(&self) -> String {
        let Some(path) = &self.memory_steward else {
            return json!({"enabled": false}).to_string();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => {
                let locale = acb_i18n::request_locale();
                json!({
                    "enabled": true,
                    "error": acb_i18n::msg!("bridge.console.memory_status_unavailable")
                        .render(locale),
                    // 底层 IO 原文只作附注，不混进主文案。
                    "detail": format!("{}: {error}", path.display()),
                })
                .to_string()
            }
        }
    }

    /// 当前配置的只读 JSON 视图。遮蔽规则：
    /// - 字段名按 `_`/`-` 切段后命中敏感词（token/password/secret/cookie/credential/key）
    ///   的整值换成 `******`（有值才遮，没有如实给 null）；
    /// - 连接 url 里查询参数名敏感的值同样遮掉（`ws://…?token=xxx` 是常见写法）；
    /// - 控制台自己的 token 根本不进这个视图。
    fn config_view(&self) -> String {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        json!({
            "personas": config.personas.iter().map(|p| json!({
                "id": p.id, "name": p.name, "text": p.text,
            })).collect::<Vec<_>>(),
            "memories": config.memories.iter().map(|m| json!({
                "id": m.id, "name": m.name, "kind": m.kind, "location": m.location,
            })).collect::<Vec<_>>(),
            "harnesses": config.harnesses.iter().map(|h| json!({
                "id": h.id, "name": h.name, "models": h.models,
            })).collect::<Vec<_>>(),
            "sessions": config.sessions.iter().map(|s| json!({
                "id": s.id,
                "name": s.name,
                "harness": s.harness,
                "persona": s.persona,
                "model": s.model,
                "memories": s.memories,
                "token": mask_secret(&s.token),
                "subscriptions": s.subscriptions.iter().map(|sub| json!({
                    "room": sub.room.to_string(),
                    "wake": json!({
                        "provider": sub.wake.provider.to_string(),
                        "at_mention": sub.wake.at_mention,
                        "reply": sub.wake.reply,
                        "name_mention": sub.wake.name_mention,
                        "names": sub.wake.names,
                        "accumulate": sub.wake.accumulate,
                        // 「被拍醒」是旧开关：现在等价于没有把 poke 放进忽略标签。
                        "poke": !sub.wake.ignore_event_tags.iter().any(|tag| tag == "poke"),
                        "event_tags": sub.wake.event_tags,
                        "ignore_event_tags": sub.wake.ignore_event_tags,
                        "events": sub.wake.events,
                        "keywords": sub.wake.keywords,
                        "ignore_speakers": sub.wake.ignore_speakers,
                    }),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "connections": config.connections.iter().map(|c| json!({
                "id": c.id,
                "platform": c.platform,
                "kind": c.kind,
                "transport": c.transport.as_str(),
                "url": c.url.as_deref().map(mask_url_query).unwrap_or_default(),
                "token": c.token.as_deref().map(mask_secret),
                "account": c.account,
                "admin_qqs": c.admin_qqs,
            })).collect::<Vec<_>>(),
            "file_relay": config.file_relay.as_ref().map(|r| json!({
                "mode": r.mode,
                "listen": r.listen,
                "public_base": r.public_base,
                "dir": r.dir,
            })),
            "storage": { "path": config.storage.path },
            "retention": {
                "max_records_per_room": config.retention.max_records_per_room,
                "max_age_days": config.retention.max_age_days,
            },
            "worktrees_root": config.worktrees_root,
            "delivery_limit": config.delivery_limit,
            "turn_timeout_secs": config.turn_timeout_secs,
            "default_on_turn_error": config.default_on_turn_error.iter().map(|action| match action {
                acb_kernel::config::TurnErrorAction::Log => json!("log"),
                acb_kernel::config::TurnErrorAction::Send { room } => json!(format!("send:{room}")),
            }).collect::<Vec<_>>(),
        })
        .to_string()
    }

    fn status(&self) -> String {
        let dependencies = self.deps.summary();
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        let inbound = self.inbound_stats.snapshot();
        let route = self.route_worker.stats().snapshot();
        json!({
            // 构建版本（flake 注入 ACB_BUILD_VERSION；非 nix 构建回落 dev）。
            // 控制台头部显示它，用于分辨「这台桥跑的是哪次构建」。
            "version": std::env::var("ACB_BUILD_VERSION")
                .unwrap_or_else(|_| "dev".to_owned()),
            "connections": config.connections.len(),
            "sessions": config.sessions.len(),
            "harnesses": config.harnesses.len(),
            "busy_sessions": bridge.busy_sessions(),
            "queued_turns": bridge.queued_turns(),
            "config_version": bridge.config_version(),
            "staged_config": bridge.has_staged_config(),
            "config_fragments": self.config_sources.iter().map(|source| json!({
                "path": source.path,
                "connections": source.connections,
                "sessions": source.sessions,
                "rules": source.rules,
            })).collect::<Vec<_>>(),
            "rules": config.grants.rules.len(),
            // 最近 15 分钟各房间的失败次数（**含被抑制的**）。群里「没人回话」时先看这里：
            // 有计数 = 每轮都在失败、只是通知被限流了；没计数 = 真的没轮到它。
            "recent_failures": bridge
                .recent_failure_stats(RECENT_FAILURE_WINDOW_SECS)
                .iter()
                .map(|stat| json!({
                    "session": stat.session,
                    "room": stat.room,
                    "count": stat.count,
                    "last_at": stat.last_at,
                    "class": stat.last_class,
                }))
                .collect::<Vec<_>>(),
            "queues": queue_status(&inbound, &route, self.route_worker.busy()),
            "dependencies": {
                "status": dependencies["status"],
                "up": dependencies["up"],
                "down": dependencies["down"],
                "unknown": dependencies["unknown"],
            },
        })
        .to_string()
    }

    /// 策略快照：权限规则、房间政策、审批路由、管理员。
    ///
    /// 这是**库里**的东西（schema 13），不是配置文件。`source` 写在返回里，
    /// 免得有人以为这个页面改的是 `config.toml`——那两个地方都能改权限时，
    /// 「为什么这条规则不生效」就变成了猜谜。
    fn policy_view(&self) -> (u16, String) {
        let bridge = lock(&self.bridge);
        let mut grants = bridge.config().grants.clone();
        // 内置角色只做展示/迁移；库里还没有角色时先把它们端出来，避免页面空白。
        if grants.roles.is_empty() {
            grants.roles = acb_kernel::grants::builtin_roles();
        }
        (
            200,
            json!({
                "source": "database",
                "grants": grants,
                "pending_reload": bridge.has_pending_policy_reload(),
                "config_version": bridge.config_version(),
            })
            .to_string(),
        )
    }

    /// 整份保存策略：`{"grants": {...}}`，形状与 `GET /api/policy` 回来的一致。
    ///
    /// 为什么整份而不是逐条 op：页面上就是一张表单，改完一次提交；整份在事务里
    /// 一次落地，不会出现「规则改了一半」的中间态。代价是两个人同时编辑会互相
    /// 覆盖——当前控制台是单人运维工具，先按这个来，将来要多人再拆 ops。
    fn save_policy(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.policy_body_invalid"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(raw) = parsed.get("grants") else {
            return config_error(
                400,
                acb_i18n::msg!("bridge.console.policy_grants_required"),
                None,
            );
        };
        let grants: acb_kernel::GrantsConfig = match serde_json::from_value(raw.clone()) {
            Ok(grants) => grants,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.policy_shape_invalid"),
                    Some(error.to_string()),
                );
            }
        };

        let mut bridge = lock(&self.bridge);
        let rooms = grants.rooms.len();
        let rules = grants.rules.len() + grants.deny.len();
        match bridge.replace_policy(&grants) {
            Ok(()) => (
                200,
                json!({
                    "ok": true,
                    "rules": rules,
                    "rooms": rooms,
                    // 轮次边界才生效，如实说清楚，免得人去群里试然后怀疑没保存。
                    "pending_reload": true,
                })
                .to_string(),
            ),
            Err(error) => config_error(
                400,
                acb_i18n::msg!("bridge.console.policy_save_failed"),
                Some(error),
            ),
        }
    }

    /// 后备模型链（库表 `model_fallbacks` / `session_model_fallbacks`，schema 18）。
    ///
    /// 不带 `session` = 全局默认链；带 `session` = 那个会话实际生效的链，
    /// 并告诉调用方它是「自己配的」还是「继承全局」。
    /// 和策略同一个语义：库是唯一来源，改完轮次边界生效。
    fn model_fallbacks_view(&self, session: Option<&str>) -> (u16, String) {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        let models = match session {
            Some(id) => bridge.session_fallbacks(id),
            None => bridge.model_fallbacks(),
        };
        let (harness, available) = match session {
            Some(id) => (
                config.session(id).map(|session| session.harness.clone()),
                config.harness_models(id).to_vec(),
            ),
            None => (None, Vec::new()),
        };
        (
            200,
            json!({
                "source": "database",
                "session": session,
                "models": models,
                "inherited": session.is_some_and(|id| !bridge.has_session_fallbacks(id)),
                "harness": harness,
                "available": available,
                "pending_reload": bridge.has_pending_policy_reload(),
                "config_version": bridge.config_version(),
            })
            .to_string(),
        )
    }

    /// 整份保存后备链：`{"session": "id"?, "models": ["provider/model", ...]}`，
    /// 顺序即优先级。不带 `session` 改全局默认。
    ///
    /// 校验在 kernel 里做（`provider/model` 的拆分规则只有一处）。这里只负责
    /// 把 JSON 变成 `Vec<String>`，以及把拒绝原因原样带回去——「哪一项写错了」
    /// 比「保存失败」有用得多。
    fn save_model_fallbacks(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.model_fallbacks_body_invalid"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(raw) = parsed.get("models") else {
            return config_error(
                400,
                acb_i18n::msg!("bridge.console.model_fallbacks_body_invalid"),
                None,
            );
        };
        let models: Vec<String> = match serde_json::from_value(raw.clone()) {
            Ok(models) => models,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.model_fallbacks_body_invalid"),
                    Some(error.to_string()),
                );
            }
        };
        // 空 `session` 字符串按「没带」处理：前端清空选择框时会这样发。
        let session = parsed
            .get("session")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let mut bridge = lock(&self.bridge);
        match bridge.replace_model_fallbacks(session, &models) {
            Ok(()) => (
                200,
                json!({
                    "ok": true,
                    "session": session,
                    "models": models,
                    "pending_reload": true,
                })
                .to_string(),
            ),
            Err(error) => config_error(
                400,
                acb_i18n::msg!("bridge.console.model_fallbacks_save_failed"),
                Some(error),
            ),
        }
    }

    /// 配置 TOML 原文。**只在鉴权之后给**——原文里就是明文密钥，管理员编辑用。
    fn config_toml(&self) -> (u16, String) {
        match self.console_document() {
            Ok((path, text)) => (200, json!({"path": path, "text": text}).to_string()),
            Err(response) => response,
        }
    }

    /// 保存编辑后的配置 TOML（`POST /api/config/toml`，body `{"text": "..."}`）。
    ///
    /// 顺序是先校验后落盘：解析不过就原样退回，一个字节都不写——
    /// 「用户不该需要懂文件格式」的底线是**坏配置永远进不了磁盘**。
    /// 保存目标固定为 `<config_dir>/90-console.toml`，不再改写 `--config`；
    /// 旧覆盖层备份成 `90-console.toml.bak`（只留最近一份）。
    /// 解析通过的新配置暂存进桥，下一次开轮换上（ADR-0009 轮次边界重载）。
    /// 返回里带影响清单：哪些下一轮自动生效、哪些要重启桥。
    fn save_config_toml(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.body_json_text"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(text) = parsed.get("text").and_then(serde_json::Value::as_str) else {
            return config_error(
                400,
                acb_i18n::msg!("bridge.console.body_text_required"),
                None,
            );
        };

        // 0. 已经入库的段不许再从文件改。以前覆盖层能整类盖掉，改了不生效还没提示
        //    （实测吞过 9 条 plugin 授权）；现在宁可报错。
        match crate::config_objects::migrated_keys_in(text) {
            Ok(keys) if !keys.is_empty() => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.config_key_migrated", keys = keys.join(", ")),
                    Some(acb_i18n::t!("bridge.console.config_key_migrated_detail").to_owned()),
                );
            }
            Ok(_) => {}
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.config_parse_failed"),
                    Some(error.to_string()),
                );
            }
        }

        let objects = {
            let bridge = lock(&self.bridge);
            match bridge.config_objects() {
                Ok(objects) => objects,
                Err(error) => {
                    return config_error(
                        500,
                        acb_i18n::msg!("bridge.console.config_objects_read_failed"),
                        Some(error.to_string()),
                    );
                }
            }
        };

        // 1. 校验：把这段文本当作 90-console 覆盖层，和 base + 其他分片一起解析。
        //    五类业务对象一律用库里那一份——文件里就算写了也不参与。
        let new_config = match acb_kernel::Config::load_with_console_overlay(
            Path::new(&self.config_path),
            Path::new(&self.config_dir),
            text,
            Some(&objects),
        ) {
            Ok(loaded) => loaded.config,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.config_parse_failed"),
                    Some(error.to_string()),
                );
            }
        };

        // 2. 收尾（共用）：算影响清单 → 备份 → 落盘 → 暂存。
        match self.finish_config_save(new_config, Some(text), None, Vec::new()) {
            Ok(value) => (200, value.to_string()),
            Err((status, value)) => (status, value.to_string()),
        }
    }

    /// 结构化表单补丁保存：`POST /api/config/patch`，body `{"ops":[...]}`。
    /// 表单改动的**主入口**（ADR-0009「用户不该需要懂文件格式」）：
    /// toml_edit 在原 TOML 上外科手术式改键——注释、排版、没碰过的键全都不动。
    /// 之后走同一条「校验 → 备份 → 落盘 → 暂存」流水线，任何一步失败整体报错。
    fn save_config_patch(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.body_json_ops"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(ops) = parsed.get("ops").and_then(serde_json::Value::as_array) else {
            return config_error(400, acb_i18n::msg!("bridge.console.ops_required"), None);
        };
        if ops.is_empty() {
            return config_error(400, acb_i18n::msg!("bridge.console.ops_empty"), None);
        }

        // 补丁按落点分两组：对象类写库，`global` 写文件。混着改会让「改了没生效」
        // 重新变成猜谜——文件里那五段已经不再参与合并了。
        let mut object_ops = Vec::new();
        let mut file_ops = Vec::new();
        for op in ops {
            match op
                .get("op")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
            {
                "session" | "subscription" | "persona" | "memory" | "harness" | "connection"
                | "delete" => object_ops.push(op.clone()),
                "global" => file_ops.push(op.clone()),
                other => {
                    return config_error(
                        400,
                        acb_i18n::msg!("bridge.patch.unknown_op", op = other),
                        None,
                    );
                }
            }
        }

        // 新建会话必须带一个专属接入令牌。以前这里要求名字已经在 `[secrets]` 里声明，
        // 于是「在页面上加会话」必然失败（`references undeclared secret source`）——
        // 页面上根本没法声明来源。现在缺什么就当场补：生成 32 字节 token 写 0600
        // 文件，并往 `config.d/50-session-secrets.toml` 追加一条来源声明；
        // 后面校验失败会整体回滚（`SessionSecretMint` 的 Drop）。
        let _mint = match self.prepare_session_secrets(&mut object_ops) {
            Ok(mint) => mint,
            Err(response) => return response,
        };
        let mut _mint = _mint;

        let (_path, current_text) = match self.console_document() {
            Ok(document) => document,
            Err(response) => return response,
        };
        let (overlay_text, mut changes) = if file_ops.is_empty() {
            (current_text, Vec::new())
        } else {
            match crate::config_patch::apply_patch(&current_text, &file_ops) {
                Ok(result) => result,
                Err(error) => {
                    return config_error(400, error.message, error.detail);
                }
            }
        };

        let objects = {
            let bridge = lock(&self.bridge);
            match bridge.config_objects() {
                Ok(objects) => objects,
                Err(error) => {
                    return config_error(
                        500,
                        acb_i18n::msg!("bridge.console.config_objects_read_failed"),
                        Some(error.to_string()),
                    );
                }
            }
        };
        // 对象侧：在「库里的那一份」上打补丁，改完再拆回一条一条。
        let next_objects = if object_ops.is_empty() {
            None
        } else {
            let document = crate::config_objects::document(&objects);
            let (patched, object_changes) =
                match crate::config_patch::apply_patch(&document, &object_ops) {
                    Ok(result) => result,
                    Err(error) => {
                        return config_error(400, error.message, error.detail);
                    }
                };
            changes.extend(object_changes);
            match crate::config_objects::document_to_objects(&patched) {
                Ok(objects) => Some(objects),
                Err(error) => {
                    return config_error(
                        400,
                        acb_i18n::msg!("bridge.console.config_objects_parse_failed"),
                        Some(error.to_string()),
                    );
                }
            }
        };

        // 兜底：对象补丁只该「改」或「显式删」。任何一条没被显式 delete 却消失了，
        // 都说明补丁在文本层把数据吃掉了——宁可整单拒绝，也不能静默丢会话
        // （2026-11-10 现场：新建会话后 `dsh-dev` 消失，只能从备份库补回）。
        if let Some(next) = &next_objects {
            if let Some((kind, id)) = lost_object(&objects, next, &object_ops) {
                return config_error(
                    500,
                    acb_i18n::msg!(
                        "bridge.console.patch_lost_object",
                        kind = kind,
                        id = id.clone()
                    ),
                    Some(acb_i18n::t!("bridge.console.patch_lost_object_detail").to_owned()),
                );
            }
        }

        let effective = next_objects.as_ref().unwrap_or(&objects);
        let new_config = match acb_kernel::Config::load_with_console_overlay(
            Path::new(&self.config_path),
            Path::new(&self.config_dir),
            &overlay_text,
            Some(effective),
        ) {
            Ok(loaded) => loaded.config,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.patched_config_parse_failed"),
                    Some(error.to_string()),
                );
            }
        };
        // 只有真的动了文件侧才回写覆盖层；纯对象改动不碰任何文件。
        let write_text = (!file_ops.is_empty()).then_some(overlay_text.as_str());
        // 校验过了再落库：新令牌进 `secrets` 表（密文），文件那份留给还没换新的 acbctl。
        let mut db_written: Vec<String> = Vec::new();
        if let Err(response) = self.persist_minted_secrets(&_mint.tokens, &mut db_written) {
            return response;
        }
        let result =
            self.finish_config_save(new_config, write_text, next_objects.as_ref(), changes);
        if result.is_ok() {
            _mint.commit();
        } else {
            let mut bridge = lock(&self.bridge);
            for name in &db_written {
                let _ = bridge.delete_secret(name);
            }
        }
        match result {
            Ok(value) => (200, value.to_string()),
            Err((status, value)) => (status, value.to_string()),
        }
    }

    /// 给「新建会话」补上令牌来源。返回的守卫在校验失败时把改动撤掉。
    fn prepare_session_secrets(
        &self,
        ops: &mut Vec<serde_json::Value>,
    ) -> Result<SessionSecretMint, (u16, String)> {
        let mut mint = SessionSecretMint::default();
        let declared =
            declared_secret_names(Path::new(&self.config_path), Path::new(&self.config_dir));
        for op in ops.iter_mut() {
            if op.get("op").and_then(serde_json::Value::as_str) != Some("session") {
                continue;
            }
            let id = op
                .get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_owned();
            let Some(create) = op
                .get_mut("create")
                .and_then(serde_json::Value::as_object_mut)
            else {
                continue;
            };
            let name = create
                .get("secret")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{id}-token"));
            if name.is_empty() {
                continue;
            }
            if !valid_secret_name(&name) {
                return Err(config_error(
                    400,
                    acb_i18n::msg!("bridge.console.secret_name_invalid", name = name.clone()),
                    None,
                ));
            }
            create.insert("secret".to_owned(), serde_json::Value::String(name.clone()));
            if declared.contains(&name) || mint.contains(&name) {
                continue;
            }
            self.mint_session_secret(&name, &mut mint)?;
        }
        Ok(mint)
    }

    fn mint_session_secret(
        &self,
        name: &str,
        mint: &mut SessionSecretMint,
    ) -> Result<(), (u16, String)> {
        let dir = Path::new(&self.config_dir);
        let token_dir = dir.join("secrets");
        if let Err(error) = std::fs::create_dir_all(&token_dir) {
            return Err(self.mint_error(format!("create {}: {error}", token_dir.display())));
        }
        let token_path = token_dir.join(format!("{name}.token"));
        let token = match acb_kernel::crypto::random_token() {
            Ok(token) => token,
            Err(error) => return Err(self.mint_error(error)),
        };
        if let Err(error) = write_private_file(&token_path, &format!("{token}\n")) {
            return Err(self.mint_error(format!("write {}: {error}", token_path.display())));
        }
        mint.token_paths.push(token_path.clone());
        mint.tokens.push((name.to_owned(), token.clone()));

        let fragment = dir.join("50-session-secrets.toml");
        let previous = std::fs::read_to_string(&fragment).ok();
        if mint.fragment_previous.is_none() {
            mint.fragment_previous = Some(previous.clone());
            mint.fragment_path = Some(fragment.clone());
        }
        let mut text = previous.unwrap_or_default();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!(
            "\n# auto-generated by console for session token {name}\n[[secrets.sources]]\nname = \"{name}\"\nkind = \"file\"\npath = \"{}\"\nformat = \"raw\"\n",
            token_path.display()
        ));
        if let Err(error) = std::fs::write(&fragment, text) {
            return Err(self.mint_error(format!("write {}: {error}", fragment.display())));
        }
        mint.names.push(name.to_owned());
        Ok(())
    }

    fn persist_minted_secrets(
        &self,
        tokens: &[(String, String)],
        written: &mut Vec<String>,
    ) -> Result<(), (u16, String)> {
        if tokens.is_empty() {
            return Ok(());
        }
        // 没有 ACB_KEY 就只留文件：旧 acbctl 靠文件，桥靠 preload，都还能跑。
        let key = match acb_kernel::crypto::AcbKey::load() {
            Ok(Some(key)) => key,
            Ok(None) => return Ok(()),
            Err(error) => return Err(self.mint_error(error)),
        };
        for (name, token) in tokens {
            let cipher = key.encrypt(token).map_err(|error| self.mint_error(error))?;
            let mut bridge = lock(&self.bridge);
            match bridge.put_secret(name, &cipher, true, Some(key.kid()), Some("console")) {
                Ok(()) => written.push(name.clone()),
                Err(error) => return Err(self.mint_error(error.to_string())),
            }
        }
        Ok(())
    }

    fn mint_error(&self, detail: String) -> (u16, String) {
        config_error(
            500,
            acb_i18n::msg!("bridge.console.secret_mint_failed"),
            Some(detail),
        )
    }

    /// 保存收尾（解析已通过）：算影响清单 → 备份旧文件 → 落盘 → 暂存进桥。
    /// `changes` 是表单/补丁的人话改动清单，回给页面展示。
    fn finish_config_save(
        &self,
        new_config: acb_kernel::Config,
        text: Option<&str>,
        objects: Option<&acb_kernel::ConfigObjects>,
        changes: Vec<acb_i18n::Message>,
    ) -> Result<serde_json::Value, (u16, serde_json::Value)> {
        let locale = acb_i18n::request_locale();
        // 影响清单在改桥之前算（用的是当前生效的这份）。
        let (hot, restart) = {
            let bridge = lock(&self.bridge);
            reload_impact(bridge.config(), &new_config)
        };

        // provider 校验必须在落盘之前：未知 provider 不该被写进磁盘。
        {
            let bridge = lock(&self.bridge);
            if let Err(error) = bridge.validate_plugins_for(&new_config) {
                return Err((
                    400,
                    json!({
                        "error": acb_i18n::msg!("bridge.console.provider_unavailable")
                            .render(locale),
                        "detail": error.to_string(),
                        "saved": false,
                    }),
                ));
            }
        }

        // 文件侧：只有文本真的改过才写覆盖层（纯对象改动不碰文件）。
        let backup = match text {
            None => None,
            Some(text) => match persist_console_config(&self.config_dir, text) {
                Ok(backup) => backup,
                Err(error) => {
                    return Err((
                        500,
                        json!({
                            "error": acb_i18n::msg!(
                                "bridge.console.write_config_failed",
                                path = self.console_config_path().display().to_string()
                            )
                            .render(locale),
                            "detail": error.to_string(),
                            "saved": false
                        }),
                    ));
                }
            },
        };

        {
            let mut bridge = lock(&self.bridge);
            // 对象先落库再暂存：库写失败就整体失败，不能出现「内存里改了、库里没有」
            // ——重启一次就回到改之前，用户会以为保存按钮坏了。
            if let Some(objects) = objects {
                if let Err(error) = bridge.replace_config_objects(objects) {
                    return Err((
                        500,
                        json!({
                            "error": acb_i18n::msg!("bridge.console.write_objects_failed")
                                .render(locale),
                            "detail": error,
                            "saved": false
                        }),
                    ));
                }
            }
            bridge.stage_config(new_config);
        }

        Ok(json!({
            "ok": true,
            "saved": true,
            "note": acb_i18n::msg!("bridge.console.saved_note").render(locale),
            "changes": changes.iter().map(|change| change.render(locale)).collect::<Vec<_>>(),
            "hot_reload": hot.iter().map(|item| item.render(locale)).collect::<Vec<_>>(),
            "needs_restart": restart.iter().map(|item| item.render(locale)).collect::<Vec<_>>(),
            "backup": backup,
        }))
    }

    fn sessions(&self) -> String {
        let bridge = lock(&self.bridge);
        let config = bridge.config();
        let maps = bridge.session_maps().unwrap_or_default();
        let sessions: Vec<_> = config
            .sessions
            .iter()
            .map(|session| {
                let map = maps.iter().find(|m| m.session == session.id);
                // 投过 ≥3 次还没确认的未读：agent 可能卡住了，也可能是该换人看一眼。
                // 只读，不推进水位；控制台每次刷新现算。
                let stuck: Vec<(String, StuckUnread)> = session
                    .subscriptions
                    .iter()
                    .filter_map(|subscription| {
                        let cursor = Cursor::new(session.id.clone(), subscription.room.clone());
                        let stuck = bridge
                            .store()
                            .stuck_unread(&cursor, STUCK_UNREAD_THRESHOLD)
                            .unwrap_or_default();
                        (stuck.count > 0).then(|| (subscription.room.to_string(), stuck))
                    })
                    .collect();
                let stuck_count: usize = stuck.iter().map(|(_, stuck)| stuck.count).sum();
                let stuck_max = stuck
                    .iter()
                    .map(|(_, stuck)| stuck.max_times)
                    .max()
                    .unwrap_or(0);
                json!({
                    "id": session.id,
                    "name": session.name,
                    "harness": session.harness,
                    "persona": session.persona,
                    "model": session.model,
                    "model_fallbacks": bridge.session_fallbacks(&session.id),
                    "model_fallbacks_inherited": !bridge.has_session_fallbacks(&session.id),
                    "memories": session.memories,
                    "subscriptions": session.subscriptions.iter().map(|s| s.room.to_string()).collect::<Vec<_>>(),
                    "stuck_unread": {
                        "count": stuck_count,
                        "max_times": stuck_max,
                        "rooms": stuck.iter().map(|(room, stuck)| json!({
                            "room": room,
                            "count": stuck.count,
                            "max_times": stuck.max_times,
                        })).collect::<Vec<_>>(),
                    },
                    "map": map.map(|m| json!({
                        "harness_session": m.harness_session,
                        "meta": m.meta,
                        "updated_at": m.updated_at,
                    })),
                })
            })
            .collect();
        json!({ "sessions": sessions }).to_string()
    }

    /// 请求卸载或重载一个动态插件；真正的动作在下一个轮次边界执行。
    fn plugin_action(&self, body: &str, reload: bool) -> (u16, String) {
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else {
            return error_response(400, acb_i18n::msg!("bridge.console.body_plugin_id"), None);
        };
        let mut bridge = lock(&self.bridge);
        let result = if reload {
            bridge.queue_reload(id)
        } else {
            bridge.queue_unload(id)
        };
        match result {
            Ok(()) => {
                let state = if reload {
                    "pending_reload"
                } else {
                    "pending_unload"
                };
                let locale = acb_i18n::request_locale();
                let note = if reload {
                    acb_i18n::msg!("bridge.console.plugin_reload_queued", id = id).render(locale)
                } else {
                    acb_i18n::msg!("bridge.console.plugin_unload_queued", id = id).render(locale)
                };
                (
                    202,
                    json!({"queued": true, "id": id, "state": state, "note": note}).to_string(),
                )
            }
            Err(error) => error_response(
                409,
                acb_i18n::msg!("bridge.console.plugin_action_failed"),
                Some(error.to_string()),
            ),
        }
    }

    /// 拆 `/api/plugins/<id>/<rest>`。id 里不允许出现斜杠。
    fn plugin_path_parts(path: &str) -> Option<(String, Vec<&str>)> {
        let rest = path.strip_prefix("/api/plugins/")?;
        let mut parts = rest.split('/');
        let id = parts.next()?.to_owned();
        if id.is_empty() {
            return None;
        }
        Some((id, parts.collect()))
    }

    /// `GET /api/plugins/<id>/admin`：配置形状 + 生效值 + 资源计数 + 密钥引用名。
    fn plugin_admin(&self, path: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts != ["admin"] {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let bridge = lock(&self.bridge);
        match bridge.plugin_admin_view(&id) {
            Ok(view) => (200, view.to_string()),
            Err(error) => {
                error_response(404, acb_i18n::msg!("bridge.console.not_found"), Some(error))
            }
        }
    }

    /// `POST /api/plugins/<id>/config`：只提交要改的键，校验在 kernel。
    fn plugin_config_update(&self, path: &str, body: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts != ["config"] {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(assignments) = value.get("config").and_then(serde_json::Value::as_object) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        let mut bridge = lock(&self.bridge);
        match bridge.plugin_set_config(&id, assignments) {
            Ok(written) => (200, json!({"ok": true, "written": written}).to_string()),
            Err(error) => error_response(
                400,
                acb_i18n::msg!("bridge.console.bad_request"),
                Some(error),
            ),
        }
    }

    /// `POST /api/plugins/<id>/secret-bind`，body `{"name": "...", "secret_id": N}`。
    /// x-secret-ref / secret 字段的绑定入口；值本体永远只存在密钥库。
    fn plugin_secret_bind(&self, path: &str, body: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts != ["secret-bind"] {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(name) = value.get("name").and_then(serde_json::Value::as_str) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        let Some(secret_id) = value.get("secret_id").and_then(serde_json::Value::as_i64) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        let mut bridge = lock(&self.bridge);
        match bridge.plugin_bind_secret(&id, name, secret_id) {
            Ok(()) => (200, json!({"ok": true}).to_string()),
            Err(error) => error_response(
                400,
                acb_i18n::msg!("bridge.console.bad_request"),
                Some(error),
            ),
        }
    }

    /// `GET /api/secrets`：密钥名单（id + 名字）。密钥选择器的数据源；
    /// 值永远不出这个接口。
    fn secrets_view(&self) -> String {
        let bridge = lock(&self.bridge);
        let secrets = match bridge.secrets_index() {
            Ok(secrets) => secrets,
            Err(error) => {
                return error_response(
                    500,
                    acb_i18n::msg!("bridge.console.bad_request"),
                    Some(error),
                )
                .1;
            }
        };
        json!({
            "secrets": secrets
                .into_iter()
                .map(|(id, name)| json!({"id": id, "name": name}))
                .collect::<Vec<_>>(),
        })
        .to_string()
    }

    /// `POST /api/secrets`，body `{"name": "...", "value": "..."}`：控制台创建
    /// 具名密钥。
    ///
    /// 三份落点与铸造流程同构（镜像 `persist_minted_secrets`）：
    /// 1. 值文件（0600）——运行期 staged config 的解析来源；
    /// 2. config.d 声明片段——fail-closed 语义靠它成立（引用一个不存在
    ///    的来源必须启动失败，而不是静默放空）；
    /// 3. AcbKey 密文进库——重启 preload 的权威值。
    /// 没有 AcbKey 时跳过入库（与铸造一致：只留文件），选择器列表会少这一条。
    /// 重名拒绝：引用它的连接/插件不能因为一次误保存被悄悄换值。
    fn create_secret(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let name = parsed
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let value = parsed
            .get("value")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if name.is_empty() || value.is_empty() {
            return config_error(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        }
        if !valid_secret_name(name) {
            return config_error(
                400,
                acb_i18n::msg!("bridge.console.secret_name_invalid", name = name.to_owned()),
                None,
            );
        }
        {
            let bridge = lock(&self.bridge);
            let exists = bridge
                .secrets_index()
                .map(|index| index.iter().any(|(_, existing)| existing == name))
                .unwrap_or(false);
            if exists {
                return config_error(
                    400,
                    acb_i18n::msg!("bridge.console.secret_exists", name = name.to_owned()),
                    None,
                );
            }
        }

        // 1. 值文件（0600）。
        let dir = Path::new(&self.config_dir);
        let value_path = dir.join("secrets").join(format!("{name}.secret"));
        if let Some(parent) = value_path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                return self.secret_error(format!("create {}: {error}", parent.display()));
            }
        }
        if let Err(error) = write_private_file(&value_path, &format!("{value}\n")) {
            return self.secret_error(format!("write {}: {error}", value_path.display()));
        }

        // 2. 声明片段（逐名追加；重跑不重复写）。
        let fragment = dir.join("60-console-secrets.toml");
        let mut text = std::fs::read_to_string(&fragment).unwrap_or_default();
        let marker = format!("name = \"{name}\"");
        if !text.contains(&marker) {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!(
                "\n# auto-generated by console for secret {name}\n[[secrets.sources]]\nname = \"{name}\"\nkind = \"file\"\npath = \"{}\"\nformat = \"raw\"\n",
                value_path.display()
            ));
            if let Err(error) = std::fs::write(&fragment, text) {
                return self.secret_error(format!("write {}: {error}", fragment.display()));
            }
        }

        // 3. AcbKey 密文入库（没有 key 就只留文件，与铸造流程一致）。
        match acb_kernel::crypto::AcbKey::load() {
            Ok(Some(key)) => {
                let cipher = match key.encrypt(value) {
                    Ok(cipher) => cipher,
                    Err(error) => return self.secret_error(error),
                };
                let mut bridge = lock(&self.bridge);
                if let Err(error) =
                    bridge.put_secret(name, &cipher, true, Some(key.kid()), Some("console"))
                {
                    return self.secret_error(error.to_string());
                }
            }
            Ok(None) => {}
            Err(error) => return self.secret_error(error),
        }

        let locale = acb_i18n::request_locale();
        (
            200,
            json!({
                "ok": true,
                "name": name,
                "note": acb_i18n::msg!("bridge.console.secret_created_note").render(locale),
            })
            .to_string(),
        )
    }

    fn secret_error(&self, detail: String) -> (u16, String) {
        config_error(
            500,
            acb_i18n::msg!("bridge.console.secret_mint_failed"),
            Some(detail),
        )
    }

    /// `POST /api/plugins/<id>/action`，body `{"action": "...", "args": {...}}`。
    fn plugin_run_action(&self, path: &str, body: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts != ["action"] {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(action) = value.get("action").and_then(serde_json::Value::as_str) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        let args = value
            .get("args")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let Some(runner) = self
            .plugin_actions
            .iter()
            .find(|runner| runner.plugin_id() == id)
        else {
            return error_response(
                404,
                acb_i18n::msg!("bridge.console.not_found"),
                Some(format!("plugin {id} has no running action runner")),
            );
        };
        match runner.run(action, args) {
            Ok(result) => (200, json!({"ok": true, "result": result}).to_string()),
            Err(error) => error_response(
                502,
                acb_i18n::msg!("bridge.console.plugin_action_failed"),
                Some(error),
            ),
        }
    }

    /// `GET /api/plugins/<id>/records/<resource>`。
    fn plugin_records(&self, path: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts.len() != 2 || parts[0] != "records" {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let resource = parts[1];
        let bridge = lock(&self.bridge);
        match bridge.plugin_records(&id, resource) {
            Ok(records) => (
                200,
                json!({"resource": resource, "records": records}).to_string(),
            ),
            Err(error) => error_response(
                400,
                acb_i18n::msg!("bridge.console.bad_request"),
                Some(error),
            ),
        }
    }

    /// `POST /api/plugins/<id>/records/<resource>`：body 是记录对象本身。
    fn plugin_record_put(&self, path: &str, body: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts.len() != 2 || parts[0] != "records" {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let resource = parts[1];
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let record = value.get("record").unwrap_or(&value);
        let record_id = value
            .get("id")
            .and_then(serde_json::Value::as_str)
            .or_else(|| record.get("id").and_then(serde_json::Value::as_str));
        let mut bridge = lock(&self.bridge);
        match bridge.plugin_put_record(&id, resource, record_id, record) {
            Ok(record_id) => (200, json!({"ok": true, "id": record_id}).to_string()),
            Err(error) => error_response(
                400,
                acb_i18n::msg!("bridge.console.bad_request"),
                Some(error),
            ),
        }
    }

    /// `POST /api/plugins/<id>/records/<resource>/delete`，body `{"id": "..."}`。
    fn plugin_record_delete(&self, path: &str, body: &str) -> (u16, String) {
        let Some((id, parts)) = Self::plugin_path_parts(path) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        if parts.len() != 3 || parts[0] != "records" || parts[2] != "delete" {
            return error_response(404, acb_i18n::msg!("bridge.console.not_found"), None);
        }
        let resource = parts[1];
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let Some(record_id) = value.get("id").and_then(serde_json::Value::as_str) else {
            return error_response(400, acb_i18n::msg!("bridge.console.bad_request"), None);
        };
        let mut bridge = lock(&self.bridge);
        match bridge.plugin_delete_record(&id, resource, record_id) {
            Ok(deleted) => (200, json!({"ok": true, "deleted": deleted}).to_string()),
            Err(error) => error_response(
                400,
                acb_i18n::msg!("bridge.console.bad_request"),
                Some(error),
            ),
        }
    }

    /// 动态扩展目录的模块级视图。默认 namespace 是 `dyn`。
    fn extensions(&self, query: &str) -> String {
        let namespace = query_param(query, "namespace")
            .filter(|value| !value.is_empty())
            .unwrap_or("dyn");
        lock(&self.bridge).extension_catalog(namespace).to_string()
    }

    /// 排队重载动态扩展目录；真正扫描/替换在下一个轮次边界。
    fn reload_extensions(&self, body: &str) -> (u16, String) {
        let value: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return error_response(
                    400,
                    acb_i18n::msg!("bridge.console.invalid_json"),
                    Some(error.to_string()),
                );
            }
        };
        let namespace = value
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("dyn");
        let mut bridge = lock(&self.bridge);
        match bridge.queue_extension_reload(namespace) {
            Ok(()) => (
                202,
                json!({
                    "queued": true,
                    "namespace": namespace,
                    "state": "pending_reload",
                    "note": acb_i18n::msg!(
                        "bridge.console.extension_reload_queued",
                        namespace = namespace
                    )
                    .render(acb_i18n::request_locale()),
                })
                .to_string(),
            ),
            Err(error) => error_response(
                409,
                acb_i18n::msg!("bridge.console.extension_action_failed"),
                Some(error.to_string()),
            ),
        }
    }

    /// 列反馈。默认只给最近 50 条，按最近一次被提的时间倒序。
    fn feedback(&self, query: &str) -> (u16, String) {
        let status = query_param(query, "status").filter(|value| !value.is_empty());
        let limit = query_param(query, "limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(50)
            .clamp(1, 500);

        let bridge = lock(&self.bridge);
        match bridge.store().feedback(status, limit) {
            Ok(items) => {
                let returned = items.len();
                let body = json!({
                    "returned": returned,
                    "feedback": items.iter().map(feedback_json).collect::<Vec<_>>(),
                });
                (200, body.to_string())
            }
            Err(error) => error_response(
                500,
                acb_i18n::msg!("bridge.console.feedback_list_failed"),
                Some(error.to_string()),
            ),
        }
    }

    /// 改一条反馈的状态。`POST /api/feedback/:id/status`，body `{"status":"resolved","note":"..."}`。
    ///
    /// 为什么要有这一步：反馈是给人处理用的，只进不出会变成垃圾场。
    /// 处理完标掉，下次再看列表就只剩真没解决的。
    fn set_feedback(&self, path: &str, body: &str) -> (u16, String) {
        let rest = path
            .trim_start_matches("/api/feedback/")
            .trim_end_matches("/status");
        let Ok(id) = rest.parse::<i64>() else {
            return error_response(400, acb_i18n::msg!("bridge.console.feedback_path"), None);
        };
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
        let status = parsed
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("resolved");
        if !["open", "acked", "resolved", "wontfix"].contains(&status) {
            return error_response(
                400,
                acb_i18n::msg!("bridge.console.bad_feedback_status"),
                None,
            );
        }
        let note = parsed.get("note").and_then(serde_json::Value::as_str);

        let mut bridge = lock(&self.bridge);
        match bridge.store_mut().set_feedback_status(id, status, note) {
            Ok(true) => (
                200,
                json!({"ok": true, "id": id, "status": status}).to_string(),
            ),
            Ok(false) => error_response(
                404,
                acb_i18n::msg!("bridge.console.feedback_not_found", id = id),
                None,
            ),
            Err(error) => error_response(
                500,
                acb_i18n::msg!("bridge.console.feedback_update_failed"),
                Some(error.to_string()),
            ),
        }
    }

    /// 回复一条反馈（T7c）。回复会作为合成事件通知原会话。
    /// `POST /api/feedback/:id/reply`，body `{"text":"..."}`。
    fn feedback_reply(&self, path: &str, body: &str) -> (u16, String) {
        let rest = path
            .trim_start_matches("/api/feedback/")
            .trim_end_matches("/reply");
        let Ok(id) = rest.parse::<i64>() else {
            return error_response(400, acb_i18n::msg!("bridge.console.feedback_path"), None);
        };
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
        let text = parsed
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return error_response(
                400,
                acb_i18n::msg!("bridge.console.feedback_reply_empty"),
                None,
            );
        }
        let mut bridge = lock(&self.bridge);
        match bridge.reply_feedback(id, "console", "console", text) {
            Ok(result) => (200, result.to_string()),
            Err(error) => error_response(
                400,
                acb_i18n::msg!("bridge.console.feedback_reply_failed"),
                Some(error.message),
            ),
        }
    }

    fn reset(&self, path: &str) -> (u16, String) {
        // path 形如 /api/session/:id/reset 或 /api/session/:id/interrupt。
        // 两者同一套转发逻辑，只是指令名不同：reset 重置映射，interrupt 打断当前轮。
        let rest = path.trim_start_matches("/api/session/");
        let (session, command) = if let Some(s) = rest.strip_suffix("/interrupt") {
            (s, "interrupt")
        } else {
            (rest.trim_end_matches("/reset"), "reset")
        };
        let session = session.to_owned();
        if session.is_empty() {
            return error_response(400, acb_i18n::msg!("bridge.console.session_id_empty"), None);
        }
        // 确认会话存在。
        let exists = {
            let bridge = lock(&self.bridge);
            bridge.config().session(&session).is_some()
        };
        if !exists {
            return error_response(
                404,
                acb_i18n::msg!("bridge.console.session_not_found", session = session),
                None,
            );
        }
        // 发 control 帧（reset / interrupt）给管理这个会话的 harness。
        let frame = Frame::Control {
            id: format!("ctl-{}", now_millis()),
            session: session.clone(),
            command: command.into(),
            args: json!({}),
        };
        let delivered = self.controls.send_control(frame);
        if delivered {
            (
                200,
                json!({
                    "ok": true,
                    "session": session,
                    "note": acb_i18n::msg!("bridge.console.command_sent", command = command)
                        .render(acb_i18n::request_locale())
                })
                .to_string(),
            )
        } else {
            (
                409,
                json!({
                    "ok": false,
                    "session": session,
                    "error": acb_i18n::msg!("bridge.console.no_harness", session = session)
                        .render(acb_i18n::request_locale())
                })
                .to_string(),
            )
        }
    }
}

fn lock<S: Store>(bridge: &Mutex<Bridge<S>>) -> std::sync::MutexGuard<'_, Bridge<S>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 一条反馈怎么给控制台看。字段名和存储层对齐，省得两头各起一套名字。
fn feedback_json(item: &acb_kernel::Feedback) -> serde_json::Value {
    json!({
        "id": item.id,
        "title": item.title,
        "body": item.body,
        "suggestion": item.suggestion,
        "kind": item.kind,
        "severity": item.severity,
        "refs": item.refs,
        "occurrences": item.occurrences,
        "repeats": item.repeats,
        "tool": item.tool,
        "session": item.session,
        "harness": item.harness,
        "room": item.room,
        "context": item.context,
        "status": item.status,
        "note": item.note,
        "replies": item.replies,
        "first_at": item.first_at,
        "last_at": item.last_at,
    })
}

/// 把 `/api/feedback?status=open&limit=20` 拆成路径与查询串。
fn split_query(path: &str) -> (&str, &str) {
    match path.split_once('?') {
        Some((head, query)) => (head, query),
        None => (path, ""),
    }
}

/// 敏感字段名黑名单。字段名按 `_`/`-` 切段后逐段对——`keywords` 里虽然含
/// 「key」字样，但切段后是 `keywords` 整段，不匹配 `key`，不会误伤。
const SENSITIVE_KEY_PARTS: &[&str] = &[
    "token",
    "password",
    "passwd",
    "secret",
    "cookie",
    "credential",
    "credentials",
    "privatekey",
    "private",
    "apikey",
    "api",
    "key",
    "accesskey",
];

/// 字段名是否敏感。
fn is_sensitive_key(field: &str) -> bool {
    field
        .split(['_', '-'])
        .any(|part| SENSITIVE_KEY_PARTS.contains(&part))
}

/// `?token=<…>` 查询串鉴权。自由函数：纯逻辑，直接可测。
fn query_token_matches(query: &str, token: &str) -> bool {
    match query_param(query, "token") {
        Some(supplied) if !supplied.is_empty() => constant_time_eq(supplied, token),
        _ => false,
    }
}

/// `Cookie: …; acb_console=<token>; …` 鉴权（浏览器进门后种下的，刷新不掉登录）。
fn cookie_token_matches(cookie_header: &str, token: &str) -> bool {
    cookie_header.split(';').any(|pair| {
        let pair = pair.trim();
        match pair.strip_prefix("acb_console=") {
            Some(supplied) if !supplied.is_empty() => constant_time_eq(supplied, token),
            _ => false,
        }
    })
}

/// 有值就遮成 `******`（遮罩本身进 JSON，页面认它画遮蔽样式）；空值如实给 null。
fn mask_secret(value: &str) -> serde_json::Value {
    if value.is_empty() {
        serde_json::Value::Null
    } else {
        json!("******")
    }
}

/// 把 URL 查询串里敏感参数的值遮掉（`ws://h:3001/ws?token=x` → `?token=******`）。
/// 非 http(s)/ws(s) 或没有查询串时原样返回。路径与主机部分不动。
fn mask_url_query(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let masked: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) if is_sensitive_key(name) && !value.is_empty() => {
                format!("{name}=******")
            }
            _ => pair.to_owned(),
        })
        .collect();
    format!("{head}?{}", masked.join("&"))
}

/// 从查询串里取一个参数（`a=1&b=2`）。不引 url 库：只认 `k=v`，够用。
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

fn parse_request_line(header_text: &str) -> (String, String, String) {
    let mut lines = header_text.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut auth = String::new();
    for line in lines {
        // 头名大小写不敏感（RFC 9110 §5.1）：Go/Node 的客户端会把 `Authorization`
        // 规范化成小写，按字面匹配等于把整类客户端挡在门外。
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                auth = value.trim().to_owned();
            }
        }
    }
    (method, path, auth)
}

fn read_body(stream: &mut TcpStream, leftover: &[u8], header_text: &str) -> String {
    let mut body = leftover.to_vec();
    if let Some(len) = header_text.lines().find_map(|l| {
        l.strip_prefix("Content-Length:")
            .and_then(|v| v.trim().parse::<usize>().ok())
    }) {
        while body.len() < len {
            let mut chunk = [0_u8; 8192];
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
    }
    String::from_utf8_lossy(&body).to_string()
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
    extra_headers: &[&str],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    // 安全响应头照抄旧 Node 版控制台：管理页不许被别处 iframe/引脚本，缓存一律不留。
    // 不再给 `Access-Control-Allow-Origin: *`——token 鉴权的服务不该跨域放行。
    const SECURITY_HEADERS: &str = "X-Frame-Options: DENY\r\n\
        Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'\r\n\
        Cache-Control: no-store\r\n\
        X-Content-Type-Options: nosniff\r\n\
        Referrer-Policy: no-referrer\r\n";
    let mut header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{SECURITY_HEADERS}",
        body.len()
    );
    for extra in extra_headers {
        header.push_str(extra);
    }
    header.push_str("Connection: close\r\n\r\n");
    stream.write_all(header.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// 常量时间字符串比较。
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|e| e.as_millis() as i64)
        .unwrap_or(0)
}

/// 比较新旧配置，列出热重载影响（ADR-0009 轮次边界重载的边界在哪）：
///
/// - 返回第一项（`hot`）：下一轮自动生效的改动清单；
/// - 返回第二项（`restart`）：**要重启桥才生效**的改动清单——这些是启动时
///   就定死的东西：监听地址绑定、守护线程手里攥着的连接信息、打开的库路径。
///
/// 注意 `file_relay`：`listen` 变了要重启（HTTP 服务启动时绑定），
/// 但 `public_base`/`dir` 不用——发布端（桥这边）每轮用当前配置重建。
fn reload_impact(
    old: &acb_kernel::Config,
    new: &acb_kernel::Config,
) -> (Vec<acb_i18n::Message>, Vec<acb_i18n::Message>) {
    let mut hot = Vec::new();
    let mut restart = Vec::new();

    if old.sessions != new.sessions {
        hot.push(acb_i18n::msg!("bridge.console.hot_sessions"));
    }
    if old.harnesses != new.harnesses {
        hot.push(acb_i18n::msg!("bridge.console.hot_harnesses"));
    }
    if old.personas != new.personas {
        hot.push(acb_i18n::msg!("bridge.console.hot_personas"));
    }
    if old.memories != new.memories {
        hot.push(acb_i18n::msg!("bridge.console.hot_memories"));
    }
    if old.retention != new.retention {
        hot.push(acb_i18n::msg!("bridge.console.hot_retention"));
    }
    if old.default_on_turn_error != new.default_on_turn_error {
        hot.push(acb_i18n::msg!("bridge.console.hot_turn_error"));
    }
    if old.delivery_limit != new.delivery_limit {
        hot.push(acb_i18n::msg!("bridge.console.hot_delivery_limit"));
    }
    if old.turn_timeout_secs != new.turn_timeout_secs {
        hot.push(acb_i18n::msg!("bridge.console.hot_turn_timeout"));
    }
    match (&old.file_relay, &new.file_relay) {
        (Some(a), Some(b)) => {
            if a.public_base != b.public_base || a.dir != b.dir {
                hot.push(acb_i18n::msg!("bridge.console.hot_file_relay"));
            }
            if a.listen != b.listen {
                restart.push(acb_i18n::msg!("bridge.console.restart_file_relay_listen"));
            }
        }
        (None, Some(_)) | (Some(_), None) => {
            restart.push(acb_i18n::msg!("bridge.console.restart_file_relay_toggle"));
        }
        (None, None) => {}
    }

    if old.endpoint != new.endpoint {
        restart.push(acb_i18n::msg!("bridge.console.restart_endpoint"));
    }
    if old.console != new.console {
        restart.push(acb_i18n::msg!("bridge.console.restart_console"));
    }
    if old.connections != new.connections {
        restart.push(acb_i18n::msg!("bridge.console.restart_connections"));
    }
    if old.storage != new.storage {
        restart.push(acb_i18n::msg!("bridge.console.restart_storage"));
    }
    if old.worktrees_root != new.worktrees_root {
        restart.push(acb_i18n::msg!("bridge.console.restart_worktrees_root"));
    }

    (hot, restart)
}

/// 控制台保存落点：创建 `<config_dir>/90-console.toml`，旧覆盖层备份成 `.bak`。
/// 返回备份路径（首次保存没有旧文件时为空）。与桥无关，便于单测。
fn persist_console_config(config_dir: &str, text: &str) -> std::io::Result<Option<String>> {
    let target = Path::new(config_dir).join(CONSOLE_CONFIG_FILENAME);
    std::fs::create_dir_all(config_dir)?;
    let old_text = std::fs::read_to_string(&target).ok();
    let backup = old_text
        .as_ref()
        .map(|_| format!("{}.bak", target.display()));
    if let Some(old_text) = &old_text {
        let backup_path = format!("{}.bak", target.display());
        if let Err(error) = std::fs::write(&backup_path, old_text) {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "bridge.console.backup_failed",
                    path = backup_path,
                    error = error
                )
            );
        }
    }
    std::fs::write(&target, text)?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_line_reads_authorization_case_insensitively() {
        // Node/Go 客户端会把头名规范成小写；按字面匹配会让它们全部 401。
        let (_, _, auth) =
            parse_request_line("GET /api/status HTTP/1.1\r\nauthorization: Bearer abc\r\n\r\n");
        assert_eq!(auth, "Bearer abc");
        let (_, _, auth) =
            parse_request_line("GET /api/status HTTP/1.1\r\nAUTHORIZATION:  Bearer def \r\n\r\n");
        assert_eq!(auth, "Bearer def");
    }

    fn is_cjk(ch: char) -> bool {
        matches!(
            ch as u32,
            0x3000..=0x303F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF
        )
    }

    fn count_cjk(text: &str) -> usize {
        text.chars().filter(|ch| is_cjk(*ch)).count()
    }

    fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        let start = source
            .find(start)
            .unwrap_or_else(|| panic!("缺少起始标记 {start}"))
            + start.len();
        let end = source[start..]
            .find(end)
            .unwrap_or_else(|| panic!("缺少结束标记 {end}"))
            + start;
        &source[start..end]
    }

    fn strip_js_comments(source: &str) -> String {
        let mut out = String::new();
        let mut chars = source.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '/' {
                match chars.peek() {
                    Some('/') => {
                        chars.next();
                        for next in chars.by_ref() {
                            if next == '\n' {
                                out.push('\n');
                                break;
                            }
                        }
                    }
                    Some('*') => {
                        chars.next();
                        let mut previous = ' ';
                        for next in chars.by_ref() {
                            if previous == '*' && next == '/' {
                                break;
                            }
                            previous = next;
                        }
                    }
                    _ => out.push(ch),
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    fn strip_rust_comments(source: &str) -> String {
        let mut out = String::new();
        let mut chars = source.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '/' {
                match chars.peek() {
                    Some('/') => {
                        chars.next();
                        for next in chars.by_ref() {
                            if next == '\n' {
                                out.push('\n');
                                break;
                            }
                        }
                    }
                    Some('*') => {
                        chars.next();
                        let mut previous = ' ';
                        for next in chars.by_ref() {
                            if previous == '*' && next == '/' {
                                break;
                            }
                            previous = next;
                        }
                    }
                    _ => out.push(ch),
                }
            } else if ch == '"' {
                out.push(ch);
                let mut escaped = false;
                for next in chars.by_ref() {
                    out.push(next);
                    if escaped {
                        escaped = false;
                    } else if next == '\\' {
                        escaped = true;
                    } else if next == '"' {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// 样板页（概览）范围零硬编码：HTML 区块与标记之间的 JS 都不允许出现中文。
    #[test]
    fn overview_sample_scope_has_no_hardcoded_cjk() {
        let section = between(
            CONSOLE_HTML,
            "<!-- status-scope:start -->",
            "<!-- status-scope:end -->",
        );
        assert_eq!(
            count_cjk(section),
            0,
            "概览页 HTML 仍有硬编码中文：{section}"
        );
        let js = strip_js_comments(between(
            CONSOLE_HTML,
            "// status-scope:start",
            "// status-scope:end",
        ));
        assert_eq!(count_cjk(&js), 0, "概览页 JS 仍有硬编码中文：{js}");
    }

    /// 控制台相关的 Rust 非测试代码里不允许出现中文成品文案；
    /// 中文只允许出现在 locale 表与注释里。
    #[test]
    fn rust_console_copy_has_no_hardcoded_cjk() {
        for (name, source) in [
            ("console.rs", include_str!("console.rs")),
            ("config_patch.rs", include_str!("config_patch.rs")),
        ] {
            let code = source
                .split("#[cfg(test)]")
                .next()
                .expect("至少有一段非测试代码");
            let stripped = strip_rust_comments(code);
            assert_eq!(
                count_cjk(&stripped),
                0,
                "{name} 的非测试代码里仍有硬编码中文"
            );
        }
    }

    /// 页面用到的每个 i18n key 都必须在 en / zh-CN 两张表里存在。
    #[test]
    fn console_i18n_keys_exist_in_both_locales() {
        let mut keys = Vec::new();
        for attribute in ["data-i18n=\"", "data-i18n-aria=\"", "data-tip-key=\""] {
            for (index, _) in CONSOLE_HTML.match_indices(attribute) {
                let start = index + attribute.len();
                let Some(end) = CONSOLE_HTML[start..].find('"') else {
                    continue;
                };
                let key = &CONSOLE_HTML[start..start + end];
                if key.contains('$') || key.contains('{') {
                    continue;
                }
                keys.push(key.to_owned());
            }
        }
        for (index, _) in CONSOLE_HTML.match_indices("t(\"") {
            if index > 0 {
                let previous = CONSOLE_HTML[..index].chars().next_back().unwrap_or(' ');
                if previous.is_alphanumeric() || previous == '_' || previous == '.' {
                    continue;
                }
            }
            let start = index + 3;
            let Some(end) = CONSOLE_HTML[start..].find('"') else {
                continue;
            };
            let key = &CONSOLE_HTML[start..start + end];
            if !key.ends_with('.')
                && !key.contains(" + ")
                && !key.contains('$')
                && !key.contains('{')
            {
                keys.push(key.to_owned());
            }
        }
        // 运行时拼出来的 key（前缀 + 枚举值）单独列出。
        for key in [
            "console.deps.kind.harness",
            "console.deps.kind.model",
            "console.deps.kind.platform",
            "console.deps.state.up",
            "console.deps.state.down",
            "console.deps.state.unknown",
            "console.deps.account.online",
            "console.deps.account.offline",
            "console.deps.account.unknown",
            "console.plugins.state.resident",
            "console.plugins.state.unloadable",
            "console.plugins.state.pending_unload",
            "console.plugins.state.pending_reload",
            "console.plugins.state.unloaded",
        ] {
            keys.push(key.to_owned());
        }
        for key in keys {
            assert!(
                acb_i18n::has_key("en", &key),
                "en locale 缺少控制台 key：{key}"
            );
            assert!(
                acb_i18n::has_key("zh-CN", &key),
                "zh-CN locale 缺少控制台 key：{key}"
            );
        }
    }

    /// `?locale=` 优先于 `Accept-Language`；两者都没有时回退到进程 locale。
    #[test]
    fn request_locale_prefers_query_over_accept_language() {
        assert_eq!(
            request_locale("Accept-Language: zh-CN,zh;q=0.9\r\n", "locale=en"),
            "en"
        );
        assert_eq!(
            request_locale("Accept-Language: en-US,en;q=0.9\r\n", ""),
            "en"
        );
        assert_eq!(request_locale("Accept-Language: zh-Hant\r\n", ""), "zh-CN");
        assert_eq!(
            request_locale("Accept-Language: fr-FR\r\n", ""),
            acb_i18n::locale()
        );
    }

    /// 错误响应在请求 locale 下渲染，而不是进程 locale。
    #[test]
    fn error_json_renders_in_request_locale() {
        let en = acb_i18n::with_locale("en", || {
            error_json(acb_i18n::msg!("bridge.console.unauthorized"), None)
        });
        assert!(en.contains("unauthorized"), "{en}");
        let zh = acb_i18n::with_locale("zh-CN", || {
            error_json(acb_i18n::msg!("bridge.console.unauthorized"), None)
        });
        assert!(zh.contains("未授权"), "{zh}");
    }

    #[test]
    fn sensitive_key_matches_parts_not_substrings() {
        assert!(is_sensitive_key("token"));
        assert!(is_sensitive_key("access_token"));
        assert!(is_sensitive_key("api-key"));
        assert!(is_sensitive_key("console_token"));
        // keywords 不是密钥——别因为含 key 字样就误伤。
        assert!(!is_sensitive_key("keywords"));
        assert!(!is_sensitive_key("url"));
    }

    #[test]
    fn status_includes_queue_depths_and_rejections() {
        let inbound = QueueStats::new("inbound", 4096).snapshot();
        let route = QueueStats::new("route", 1024).snapshot();
        let value = queue_status(&inbound, &route, 2);
        assert_eq!(value["inbound"]["capacity"], 4096);
        assert_eq!(value["inbound"]["depth"], 0);
        assert_eq!(value["inbound"]["warnPercent"], 80);
        assert_eq!(value["route"]["capacity"], 1024);
        assert_eq!(value["route"]["busy"], 2);
        assert_eq!(value["route"]["rejected"], 0);
    }

    #[test]
    fn memory_stuck_warning_is_wired_into_the_console() {
        let html = include_str!("console.html");
        assert!(html.contains("id=\"memory-warning\""));
        assert!(html.contains("renderMemoryWarning"));
        assert!(html.contains("state.stuck"));
    }

    #[test]
    fn mask_secret_masks_only_when_present() {
        assert_eq!(mask_secret("abc"), json!("******"));
        assert_eq!(mask_secret(""), serde_json::Value::Null);
    }

    #[test]
    fn mask_url_query_masks_sensitive_params_only() {
        assert_eq!(
            mask_url_query("ws://127.0.0.1:3001/ws?token=abc&flag=1"),
            "ws://127.0.0.1:3001/ws?token=******&flag=1"
        );
        assert_eq!(
            mask_url_query("ws://127.0.0.1:3001/ws"),
            "ws://127.0.0.1:3001/ws"
        );
        // 空值不遮——没有秘密可保。
        assert_eq!(
            mask_url_query("http://h/p?token=&a=1"),
            "http://h/p?token=&a=1"
        );
    }

    #[test]
    fn query_token_authorized_accepts_exact_match_only() {
        assert!(query_token_matches("a=1&token=s3cret", "s3cret"));
        assert!(!query_token_matches("token=wrong", "s3cret"));
        assert!(!query_token_matches("token=", "s3cret"));
        assert!(!query_token_matches("no-token-here", "s3cret"));
        assert!(!query_token_matches("token=S3CRET", "s3cret"));
    }

    #[test]
    fn cookie_token_matches_reads_acb_console_pair() {
        assert!(cookie_token_matches("acb_console=s3cret", "s3cret"));
        assert!(cookie_token_matches(
            "other=1; acb_console=s3cret",
            "s3cret"
        ));
        assert!(cookie_token_matches(
            "acb_console=s3cret; extra=2",
            "s3cret"
        ));
        assert!(!cookie_token_matches("acb_console=wrong", "s3cret"));
        assert!(!cookie_token_matches("acb_console=", "s3cret"));
        assert!(!cookie_token_matches("other=1", "s3cret"));
        // 别的 cookie 里带了 token= 前缀也不该误命中。
        assert!(!cookie_token_matches("xacb_console=s3cret", "s3cret"));
    }

    #[test]
    fn console_html_defines_distinct_dependency_state_rendering() {
        // 前端没有独立测试框架；这条静态契约测试保证三态映射和三种视觉样式
        // 不会被后续改动合并成同一分支。实际渲染仍由浏览器完成。
        assert!(CONSOLE_HTML.contains("function depStateClass"));
        assert!(CONSOLE_HTML.contains(r#"if (state === "up") return "dep-up";"#));
        assert!(CONSOLE_HTML.contains(r#"if (state === "down") return "dep-down";"#));
        assert!(CONSOLE_HTML.contains(r#"return "dep-unknown";"#));
        for class in [".pill.dep-up", ".pill.dep-down", ".pill.dep-unknown"] {
            assert!(CONSOLE_HTML.contains(class), "缺少样式 {class}");
        }
    }

    /// 一份能通过解析的最小配置。改字段测影响对比用。
    fn toml_with(wake_line: &str, endpoint_listen: &str, connection_url: &str) -> String {
        format!(
            r#"
[[personas]]
id = "default"
name = "默认"
text = "测试。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "{connection_url}"
account = "10001"
token = {{ secret = "conn-secret" }}

[[sessions]]
id = "main"
harness = "dsh"
persona = "default"
token = {{ secret = "sess-secret" }}

[[sessions.subscriptions]]
connection = "qq"
room = "group/123"
{wake_line}

[endpoint]
listen = "{endpoint_listen}"

[console]
listen = "127.0.0.1:31990"
token = {{ secret = "ctok123" }}
"#
        )
    }

    fn rendered(items: &[acb_i18n::Message]) -> Vec<String> {
        items.iter().map(|item| item.render("en")).collect()
    }

    #[test]
    fn reload_impact_sorts_hot_from_restart() {
        // 只改唤醒策略 → 热重载；只改监听/连接 → 要重启；什么都不改 → 两边都空。
        let base = toml_with("", "127.0.0.1:37700", "ws://127.0.0.1:53001/ws");
        let old = acb_kernel::Config::parse_fixture(&base).expect("能解析");

        let hot_only = toml_with(
            // 注意：at_mention 默认就是 true，改它等于没改；keywords 默认空，一定有差异。
            "\n[sessions.subscriptions.wake]\nkeywords = [\"喂\"]",
            "127.0.0.1:37700",
            "ws://127.0.0.1:53001/ws",
        );
        let new = acb_kernel::Config::parse_fixture(&hot_only).expect("能解析");
        let (hot, restart) = reload_impact(&old, &new);
        assert!(restart.is_empty());
        assert!(rendered(&hot).iter().any(|item| item.contains("sessions")));

        let restart_only = toml_with("", "127.0.0.1:39999", "ws://127.0.0.1:53001/ws");
        let new = acb_kernel::Config::parse_fixture(&restart_only).expect("能解析");
        let (hot, restart) = reload_impact(&old, &new);
        assert!(hot.is_empty());
        assert!(
            rendered(&restart)
                .iter()
                .any(|item| item.contains("endpoint"))
        );

        let conn_changed = toml_with(
            "",
            "127.0.0.1:37700",
            "ws://127.0.0.1:53002/ws?token=newsecret",
        );
        let new = acb_kernel::Config::parse_fixture(&conn_changed).expect("能解析");
        let (_, restart) = reload_impact(&old, &new);
        assert!(
            rendered(&restart)
                .iter()
                .any(|item| item.contains("connections"))
        );

        let same = acb_kernel::Config::parse_fixture(&base).expect("能解析");
        let (hot, restart) = reload_impact(&old, &same);
        assert!(hot.is_empty());
        assert!(restart.is_empty());
    }

    static TEST_DIR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "acb-console-save-{label}-{}-{}",
                std::process::id(),
                TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const SAVE_BASE: &str = r#"
delivery_limit = 10

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "10001"

[[personas]]
id = "default"
name = "默认人格"
text = "你是桥。"

[[harnesses]]
id = "dsh"
name = "DeepSeek"

[[sessions]]
id = "main"
harness = "dsh"
persona = "default"
token = { secret = "t-main" }

[[sessions.subscriptions]]
connection = "qq"
room = "group/123"
"#;

    #[test]
    fn console_save_lands_in_90_and_survives_base_regeneration() {
        let dir = TestDir::new("persist");
        let base = dir.0.join("base.toml");
        let config_dir = dir.0.join("config.d");
        std::fs::write(&base, SAVE_BASE).expect("写 base");
        let console_text = SAVE_BASE.replace("delivery_limit = 10", "delivery_limit = 50");
        let saved = persist_console_config(&config_dir.display().to_string(), &console_text)
            .expect("写 90-console");
        assert!(saved.is_none(), "首次保存没有旧覆盖层");
        assert!(
            config_dir.join(CONSOLE_CONFIG_FILENAME).exists(),
            "90-console 文件必须存在"
        );
        assert_eq!(
            std::fs::read_to_string(&base).expect("读 base"),
            SAVE_BASE,
            "控制台保存不能改写 base"
        );

        let first = acb_kernel::Config::load_fixture(&base, Some(&config_dir)).expect("首次加载");
        assert_eq!(first.config.delivery_limit, 50);

        // 模拟部署侧重新生成 base：控制台改动必须仍然生效。
        std::fs::write(
            &base,
            SAVE_BASE.replace("delivery_limit = 10", "delivery_limit = 20"),
        )
        .expect("重写 base");
        let second =
            acb_kernel::Config::load_fixture(&base, Some(&config_dir)).expect("重生成后加载");
        assert_eq!(second.config.delivery_limit, 50, "控制台改动仍在");

        let console_text = console_text.replace("delivery_limit = 50", "delivery_limit = 60");
        let backup = persist_console_config(&config_dir.display().to_string(), &console_text)
            .expect("第二次写 90-console")
            .expect("第二次保存有备份");
        let backup_text = std::fs::read_to_string(backup).expect("读备份");
        assert!(
            backup_text.contains("delivery_limit = 50"),
            "备份是旧覆盖层"
        );
    }
}
