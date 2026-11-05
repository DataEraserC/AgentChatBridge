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
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{Bridge, ControlRegistry, DependencyRegistry, QueueStats, RouteWorker, Store};
use acb_proto::Frame;
use serde_json::json;

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

/// 没带 token 打开 `/` 时给的一小块登录页：弹窗要 token，拼进 URL 重进。
/// 跟旧 Node 版同一招——不让完整页面在未授权时先渲染出来。
const LOGIN_HTML: &str = r#"<!doctype html><meta charset="utf-8"><title>需要令牌</title>
<body style="font-family:system-ui;background:#14161a;color:#d8dce3;display:grid;place-items:center;height:100vh">
<div style="text-align:center">
<p>请输入控制台访问令牌</p>
<input id="t" type="password" style="padding:6px 10px;border-radius:6px;border:1px solid #33373f;background:#1e2127;color:#d8dce3">
<button onclick="go()" style="padding:6px 14px;margin-left:8px;cursor:pointer">进入</button>
</div>
<script>function go(){var t=document.getElementById('t').value;if(t)location.href='/?token='+encodeURIComponent(t)}
document.getElementById('t').addEventListener('keydown',function(e){if(e.key==='Enter')go()})</script>
</body>"#;

/// 控制台服务。桥本体之外的一条 HTTP 服务，跟接入点（TCP 7700）互不相干。
pub struct Console<S: Store> {
    bridge: Arc<Mutex<Bridge<S>>>,
    controls: ControlRegistry,
    token: String,
    /// 配置文件路径：编辑写回要落到这个文件（ADR-0009）。
    config_path: String,
    /// 入站队列统计。
    inbound_stats: QueueStats,
    /// 路由动作 worker（拿队列统计与 busy 数）。
    route_worker: RouteWorker,
    /// 依赖状态表（只读缓存）。
    deps: DependencyRegistry,
    /// 记忆管家状态文件；未启用时 `None`。
    memory_steward: Option<PathBuf>,
}

impl<S: Store + Send + 'static> Console<S> {
    // 控制台把桥的各个只读视图都拼在一个服务里；拆成结构体只会让 main 更难读。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bridge: Arc<Mutex<Bridge<S>>>,
        controls: ControlRegistry,
        token: String,
        config_path: String,
        inbound_stats: QueueStats,
        route_worker: RouteWorker,
        deps: DependencyRegistry,
        memory_steward: Option<PathBuf>,
    ) -> Self {
        Self {
            bridge,
            controls,
            token,
            config_path,
            inbound_stats,
            route_worker,
            deps,
            memory_steward,
        }
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
            let body =
                json!({"error": acb_i18n::t!("bridge.console.incomplete_headers")}).to_string();
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
                return write_response(
                    &mut stream,
                    401,
                    "text/html; charset=utf-8",
                    LOGIN_HTML,
                    &[],
                );
            }
            let body = json!({"error": acb_i18n::t!("bridge.console.unauthorized")}).to_string();
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
                    return (
                        403,
                        json!({"error": acb_i18n::t!("bridge.console.health_loopback_only")})
                            .to_string(),
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
                (
                    200,
                    json!({
                        "count": plugins.len(),
                        "plugins": plugins,
                        "connections": self.deps.platform_items(),
                        "wakeAudit": wake_audit,
                    })
                    .to_string(),
                )
            }
            // 动态插件卸载/重载：只入队，真正的 dlclose/dlopen 在轮次边界执行。
            ("POST", "/api/plugins/unload") => self.plugin_action(body, false),
            ("POST", "/api/plugins/reload") => self.plugin_action(body, true),
            // 动态扩展目录：模块级目录与轮次边界重载。
            ("GET", "/api/extensions") => (200, self.extensions(query)),
            ("POST", "/api/extensions/reload") => self.reload_extensions(body),
            // 控制台前端 i18n：当前 locale + 完整 message map。
            ("GET", "/api/i18n") => (200, acb_i18n::catalog_json()),
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
            _ => (
                404,
                json!({"error": acb_i18n::t!("bridge.console.no_endpoint")}).to_string(),
            ),
        }
    }

    /// 记忆管家状态。读的是管家自己原子替换的 status.json；读不到就如实报错。
    fn memory_steward_status(&self) -> String {
        let Some(path) = &self.memory_steward else {
            return json!({"enabled": false}).to_string();
        };
        match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => json!({
                "enabled": true,
                "error": format!("cannot read {}: {error}", path.display()),
            })
            .to_string(),
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
        use acb_kernel::config::DestructivePolicy;
        json!({
            "personas": config.personas.iter().map(|p| json!({
                "id": p.id, "name": p.name, "text": p.text,
            })).collect::<Vec<_>>(),
            "memories": config.memories.iter().map(|m| json!({
                "id": m.id, "name": m.name, "kind": m.kind, "location": m.location,
            })).collect::<Vec<_>>(),
            "harnesses": config.harnesses.iter().map(|h| json!({
                "id": h.id, "name": h.name,
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
                "allow_destructive": match &c.allow_destructive {
                    DestructivePolicy::None => {
                        json!(acb_i18n::t!("bridge.console.destructive_none"))
                    }
                    DestructivePolicy::All => {
                        json!(acb_i18n::t!("bridge.console.destructive_all"))
                    }
                    DestructivePolicy::Only(methods) => json!(methods),
                },
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
            "connections": config.connections.len(),
            "sessions": config.sessions.len(),
            "harnesses": config.harnesses.len(),
            "busy_sessions": bridge.busy_sessions(),
            "queued_turns": bridge.queued_turns(),
            "config_version": bridge.config_version(),
            "staged_config": bridge.has_staged_config(),
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

    /// 配置 TOML 原文。**只在鉴权之后给**——原文里就是明文密钥，管理员编辑用。
    fn config_toml(&self) -> (u16, String) {
        match std::fs::read_to_string(&self.config_path) {
            Ok(text) => (
                200,
                json!({"path": self.config_path, "text": text}).to_string(),
            ),
            Err(error) => (
                500,
                json!({"error": acb_i18n::t!(
                    "bridge.console.read_config_failed",
                    path = self.config_path,
                    error = error
                )})
                .to_string(),
            ),
        }
    }

    /// 保存编辑后的配置 TOML（`POST /api/config/toml`，body `{"text": "..."}`）。
    ///
    /// 顺序是先校验后落盘：解析不过就原样退回，一个字节都不写——
    /// 「用户不该需要懂文件格式」的底线是**坏配置永远进不了磁盘**。
    /// 落盘前把旧文件备份成 `<路径>.bak`（只留最近一份）。
    /// 解析通过的新配置暂存进桥，下一次开轮换上（ADR-0009 轮次边界重载）。
    /// 返回里带影响清单：哪些下一轮自动生效、哪些要重启桥。
    fn save_config_toml(&self, body: &str) -> (u16, String) {
        let parsed: serde_json::Value = match serde_json::from_str(body) {
            Ok(value) => value,
            Err(error) => {
                return (
                    400,
                    json!({"error": acb_i18n::t!("bridge.console.body_json_text", error = error)})
                        .to_string(),
                );
            }
        };
        let Some(text) = parsed.get("text").and_then(serde_json::Value::as_str) else {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.body_text_required")}).to_string(),
            );
        };

        // 1. 校验：解析不过就拒绝，什么都不写。
        let new_config = match acb_kernel::Config::parse(text) {
            Ok(config) => config,
            Err(error) => {
                return (
                    400,
                    json!({
                        "error": acb_i18n::t!(
                            "bridge.console.config_parse_failed",
                            error = error
                        ),
                        "saved": false,
                    })
                    .to_string(),
                );
            }
        };

        // 2. 收尾（共用）：算影响清单 → 备份 → 落盘 → 暂存。
        match self.finish_config_save(new_config, text, Vec::new()) {
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
                return (
                    400,
                    json!({"error": acb_i18n::t!("bridge.console.body_json_ops", error = error)})
                        .to_string(),
                );
            }
        };
        let Some(ops) = parsed.get("ops").and_then(serde_json::Value::as_array) else {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.ops_required")}).to_string(),
            );
        };
        if ops.is_empty() {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.ops_empty")}).to_string(),
            );
        }

        let text = match std::fs::read_to_string(&self.config_path) {
            Ok(text) => text,
            Err(error) => {
                return (
                    500,
                    json!({"error": acb_i18n::t!(
                        "bridge.console.read_config_failed",
                        path = self.config_path,
                        error = error
                    )})
                    .to_string(),
                );
            }
        };
        let (new_text, changes) = match crate::config_patch::apply_patch(&text, ops) {
            Ok(result) => result,
            Err(error) => {
                return (400, json!({"error": error, "saved": false}).to_string());
            }
        };
        let new_config = match acb_kernel::Config::parse(&new_text) {
            Ok(config) => config,
            Err(error) => {
                return (
                    400,
                    json!({
                        "error": acb_i18n::t!(
                            "bridge.console.patched_config_parse_failed",
                            error = error
                        ),
                        "saved": false,
                    })
                    .to_string(),
                );
            }
        };
        match self.finish_config_save(new_config, &new_text, changes) {
            Ok(value) => (200, value.to_string()),
            Err((status, value)) => (status, value.to_string()),
        }
    }

    /// 保存收尾（解析已通过）：算影响清单 → 备份旧文件 → 落盘 → 暂存进桥。
    /// `changes` 是表单/补丁的人话改动清单，回给页面展示。
    fn finish_config_save(
        &self,
        new_config: acb_kernel::Config,
        text: &str,
        changes: Vec<String>,
    ) -> Result<serde_json::Value, (u16, serde_json::Value)> {
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
                        "error": acb_i18n::t!(
                            "bridge.console.provider_unavailable",
                            error = error
                        ),
                        "saved": false,
                    }),
                ));
            }
        }

        // 备份失败不算致命（首次保存时旧文件可能不存在），但落盘失败必须报——
        // 不能让「磁盘上和桥里跑的」悄悄分家。
        let old_text = std::fs::read_to_string(&self.config_path).ok();
        if let Some(old_text) = &old_text {
            let backup_path = format!("{}.bak", self.config_path);
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
        if let Err(error) = std::fs::write(&self.config_path, text) {
            return Err((
                500,
                json!({
                    "error": acb_i18n::t!(
                        "bridge.console.write_config_failed",
                        path = self.config_path,
                        error = error
                    ),
                    "saved": false
                }),
            ));
        }

        {
            let mut bridge = lock(&self.bridge);
            bridge.stage_config(new_config);
        }

        Ok(json!({
            "ok": true,
            "saved": true,
            "note": acb_i18n::t!("bridge.console.saved_note"),
            "changes": changes,
            "hot_reload": hot,
            "needs_restart": restart,
            "backup": old_text.map(|_| format!("{}.bak", self.config_path)),
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
                json!({
                    "id": session.id,
                    "name": session.name,
                    "harness": session.harness,
                    "persona": session.persona,
                    "model": session.model,
                    "memories": session.memories,
                    "subscriptions": session.subscriptions.iter().map(|s| s.room.to_string()).collect::<Vec<_>>(),
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
                return (
                    400,
                    json!({"error": acb_i18n::t!("bridge.console.invalid_json", error = error)})
                        .to_string(),
                );
            }
        };
        let Some(id) = value.get("id").and_then(serde_json::Value::as_str) else {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.body_plugin_id")}).to_string(),
            );
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
                let note = if reload {
                    acb_i18n::t!("bridge.console.plugin_reload_queued", id = id)
                } else {
                    acb_i18n::t!("bridge.console.plugin_unload_queued", id = id)
                };
                (
                    202,
                    json!({"queued": true, "id": id, "state": state, "note": note}).to_string(),
                )
            }
            Err(error) => (409, json!({"error": error.to_string()}).to_string()),
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
                return (
                    400,
                    json!({"error": acb_i18n::t!("bridge.console.invalid_json", error = error)})
                        .to_string(),
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
                    "note": acb_i18n::t!(
                        "bridge.console.extension_reload_queued",
                        namespace = namespace
                    ),
                })
                .to_string(),
            ),
            Err(error) => (409, json!({"error": error.to_string()}).to_string()),
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
            Err(error) => (500, json!({"error": error.to_string()}).to_string()),
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
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.feedback_path")}).to_string(),
            );
        };
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
        let status = parsed
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("resolved");
        if !["open", "acked", "resolved", "wontfix"].contains(&status) {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.bad_feedback_status")}).to_string(),
            );
        }
        let note = parsed.get("note").and_then(serde_json::Value::as_str);

        let mut bridge = lock(&self.bridge);
        match bridge.store_mut().set_feedback_status(id, status, note) {
            Ok(true) => (
                200,
                json!({"ok": true, "id": id, "status": status}).to_string(),
            ),
            Ok(false) => (
                404,
                json!({"error": acb_i18n::t!("bridge.console.feedback_not_found", id = id)})
                    .to_string(),
            ),
            Err(error) => (500, json!({"error": error.to_string()}).to_string()),
        }
    }

    /// 回复一条反馈（T7c）。回复会作为合成事件通知原会话。
    /// `POST /api/feedback/:id/reply`，body `{"text":"..."}`。
    fn feedback_reply(&self, path: &str, body: &str) -> (u16, String) {
        let rest = path
            .trim_start_matches("/api/feedback/")
            .trim_end_matches("/reply");
        let Ok(id) = rest.parse::<i64>() else {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.feedback_path")}).to_string(),
            );
        };
        let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_else(|_| json!({}));
        let text = parsed
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.feedback_reply_empty")}).to_string(),
            );
        }
        let mut bridge = lock(&self.bridge);
        match bridge.reply_feedback(id, "console", "console", text) {
            Ok(result) => (200, result.to_string()),
            Err(error) => (400, json!({"error": error.message}).to_string()),
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
            return (
                400,
                json!({"error": acb_i18n::t!("bridge.console.session_id_empty")}).to_string(),
            );
        }
        // 确认会话存在。
        let exists = {
            let bridge = lock(&self.bridge);
            bridge.config().session(&session).is_some()
        };
        if !exists {
            return (
                404,
                json!({"error": acb_i18n::t!("bridge.console.session_not_found", session = session)})
                    .to_string(),
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
                    "note": acb_i18n::t!("bridge.console.command_sent", command = command)
                })
                .to_string(),
            )
        } else {
            (
                409,
                json!({
                    "ok": false,
                    "session": session,
                    "error": acb_i18n::t!("bridge.console.no_harness", session = session)
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
        if let Some(value) = line.strip_prefix("Authorization:") {
            auth = value.trim().to_owned();
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
fn reload_impact(old: &acb_kernel::Config, new: &acb_kernel::Config) -> (Vec<String>, Vec<String>) {
    let mut hot = Vec::new();
    let mut restart = Vec::new();

    if old.sessions != new.sessions {
        hot.push(acb_i18n::t!("bridge.console.hot_sessions"));
    }
    if old.harnesses != new.harnesses {
        hot.push(acb_i18n::t!("bridge.console.hot_harnesses"));
    }
    if old.personas != new.personas {
        hot.push(acb_i18n::t!("bridge.console.hot_personas"));
    }
    if old.memories != new.memories {
        hot.push(acb_i18n::t!("bridge.console.hot_memories"));
    }
    if old.retention != new.retention {
        hot.push(acb_i18n::t!("bridge.console.hot_retention"));
    }
    if old.default_on_turn_error != new.default_on_turn_error {
        hot.push(acb_i18n::t!("bridge.console.hot_turn_error"));
    }
    if old.delivery_limit != new.delivery_limit {
        hot.push(acb_i18n::t!("bridge.console.hot_delivery_limit"));
    }
    if old.turn_timeout_secs != new.turn_timeout_secs {
        hot.push(acb_i18n::t!("bridge.console.hot_turn_timeout"));
    }
    match (&old.file_relay, &new.file_relay) {
        (Some(a), Some(b)) => {
            if a.public_base != b.public_base || a.dir != b.dir {
                hot.push(acb_i18n::t!("bridge.console.hot_file_relay"));
            }
            if a.listen != b.listen {
                restart.push(acb_i18n::t!("bridge.console.restart_file_relay_listen"));
            }
        }
        (None, Some(_)) | (Some(_), None) => {
            restart.push(acb_i18n::t!("bridge.console.restart_file_relay_toggle"));
        }
        (None, None) => {}
    }

    if old.endpoint != new.endpoint {
        restart.push(acb_i18n::t!("bridge.console.restart_endpoint"));
    }
    if old.console != new.console {
        restart.push(acb_i18n::t!("bridge.console.restart_console"));
    }
    if old.connections != new.connections {
        restart.push(acb_i18n::t!("bridge.console.restart_connections"));
    }
    if old.storage != new.storage {
        restart.push(acb_i18n::t!("bridge.console.restart_storage"));
    }
    if old.worktrees_root != new.worktrees_root {
        restart.push(acb_i18n::t!("bridge.console.restart_worktrees_root"));
    }

    (hot, restart)
}

#[cfg(test)]
mod tests {
    use super::*;

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
token = "conn-secret"

[[sessions]]
id = "main"
harness = "dsh"
persona = "default"
token = "sess-secret"

[[sessions.subscriptions]]
connection = "qq"
room = "group/123"
{wake_line}

[endpoint]
listen = "{endpoint_listen}"

[console]
listen = "127.0.0.1:31990"
token = "ctok123"
"#
        )
    }

    #[test]
    fn reload_impact_sorts_hot_from_restart() {
        // 只改唤醒策略 → 热重载；只改监听/连接 → 要重启；什么都不改 → 两边都空。
        let base = toml_with("", "127.0.0.1:37700", "ws://127.0.0.1:53001/ws");
        let old = acb_kernel::Config::parse(&base).expect("能解析");

        let hot_only = toml_with(
            // 注意：at_mention 默认就是 true，改它等于没改；keywords 默认空，一定有差异。
            "\n[sessions.subscriptions.wake]\nkeywords = [\"喂\"]",
            "127.0.0.1:37700",
            "ws://127.0.0.1:53001/ws",
        );
        let new = acb_kernel::Config::parse(&hot_only).expect("能解析");
        let (hot, restart) = reload_impact(&old, &new);
        assert_eq!(restart, Vec::<String>::new());
        assert!(hot.iter().any(|item| item.contains("sessions")));

        let restart_only = toml_with("", "127.0.0.1:39999", "ws://127.0.0.1:53001/ws");
        let new = acb_kernel::Config::parse(&restart_only).expect("能解析");
        let (hot, restart) = reload_impact(&old, &new);
        assert_eq!(hot, Vec::<String>::new());
        assert!(restart.iter().any(|item| item.contains("endpoint")));

        let conn_changed = toml_with(
            "",
            "127.0.0.1:37700",
            "ws://127.0.0.1:53002/ws?token=newsecret",
        );
        let new = acb_kernel::Config::parse(&conn_changed).expect("能解析");
        let (_, restart) = reload_impact(&old, &new);
        assert!(restart.iter().any(|item| item.contains("connections")));

        let same = acb_kernel::Config::parse(&base).expect("能解析");
        let (hot, restart) = reload_impact(&old, &same);
        assert_eq!(hot, Vec::<String>::new());
        assert_eq!(restart, Vec::<String>::new());
    }
}
