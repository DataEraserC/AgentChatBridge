//! 桥的入口：读配置、接平台、等 harness。
//!
//! 启动顺序：
//! 1. 校验配置（不对就在启动时死掉，不带病运行）；可容忍的冲突只警告，停三秒再继续；
//! 2. 打开状态库（流水、水位、笔记、投递计数都在里面，重启接着用）；
//! 3. 给每条连接配一个适配器，拿到「发送端口」；
//! 4. 建桥（配置 + 存储 + 一组发送端口）；
//! 5. 每条连接起一个守护线程，连不上就等一会儿再连；
//! 6. 起一个清理线程，定时削旧流水（只削读过的）；
//! 7. 起接入点，等 harness 连进来。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acb_adapter_dynamic::DynamicLoader;
use acb_adapter_onebot_snowluma::OneBotSnowLumaAdapterFactory;
use acb_adapter_process::ProcessAdapterFactory;
use acb_kernel::{
    AdapterHost, AdapterInstance, AdapterStatus, Bridge, Config, ControlRegistry,
    DependencyRegistry, EventQueue, FileRelay, Outbounds, PluginManager, RouteWorker, SqliteStore,
    drain_events, serve,
};
use acb_plugin_lua::LuaLoader;
use acb_plugin_process_tools::{ProcessNotifier, ProcessToolLoader};
use acb_plugin_trusted::TrustedLoader;

mod config_patch;
mod console;
use console::Console;

/// 清理线程隔多久扫一次。来源：实现默认——桥不是数据库，5 分钟一次足够，
/// 真需要更勤/更懒再做成配置项。
const PRUNE_EVERY: Duration = Duration::from_secs(300);

/// 文件中继过期目录的清理周期。保留期是 24h；每小时扫一次意味着过期文件最多多留
/// 1 小时。单次扫描只 read_dir 一层并删除过期 token 目录，成本可忽略。
const RELAY_SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// 排空预算：到点还在跑就强制退出（内存任务记录会丢，消息不丢）。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 排空期间的轮询间隔。
const DRAIN_POLL: Duration = Duration::from_millis(250);

/// 退出码合同（systemd 依赖它决定是否拉起）：
/// 0 = 运维停止；75 = core 请求重启；78 = 配置/启动失败，不应自动重启。
const EXIT_RESTART: u8 = 75;
const EXIT_CONFIG: u8 = 78;

/// 信号处理只做一件事：把信号编号写进原子量。handler 内不碰锁、不分配。
static PENDING_SIGNAL: AtomicUsize = AtomicUsize::new(0);

extern "C" fn record_signal(signal: libc::c_int) {
    PENDING_SIGNAL.store(signal as usize, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: `signal` 是标准 C 接口；handler 只做原子写，满足 async-signal-safe。
    unsafe {
        libc::signal(
            libc::SIGTERM,
            record_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGHUP,
            record_signal as *const () as libc::sighandler_t,
        );
    }
}

fn main() -> ExitCode {
    acb_i18n::init();
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!(
                "{}",
                acb_i18n::t!("bridge.main.start_failed", error = error)
            );
            ExitCode::from(EXIT_CONFIG)
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let options = Options::parse(std::env::args().skip(1))?;

    let text = std::fs::read_to_string(&options.config)?;
    let config = Config::parse(&text)?;
    print_summary(&config, &options.config);
    if let Some(admin) = &config.grants.system_admin {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "bridge.main.system_admin_configured",
                connection = admin.connection,
                account = admin.account
            )
        );
    }

    // 依赖状态表：只读缓存，锁独立于 Bridge。启动时登记 harness 与平台连接；
    // model 条目在第一次轮次观测时按需创建。
    let deps = DependencyRegistry::from_config(&config);
    // 带外通知：只有 [notifications] 完整配置时才启动子进程；否则不注册、
    // 不建线程、不打日志。
    if let Some(notifications) = config.notifications.as_ref().filter(|n| n.active()) {
        match ProcessNotifier::spawn(notifications) {
            Ok(notifier) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "bridge.main.notifications_enabled",
                        url = notifications.url.as_deref().unwrap_or_default(),
                        topic = notifications.topic.as_deref().unwrap_or_default()
                    )
                );
                deps.set_notice_sink(std::sync::Arc::new(notifier));
            }
            Err(error) => eprintln!(
                "{}",
                acb_i18n::t!("bridge.main.notifications_disabled", error = error)
            ),
        }
    }

    // 有些配置是用户故意这么写的（两个会话订同一个房间）。不拦，但要说清楚它意味着什么，
    // 并且停三秒——刚启动时人还在看屏幕。
    let warnings = config.warnings();
    if !warnings.is_empty() {
        for warning in &warnings {
            eprintln!(
                "{}",
                acb_i18n::t!("bridge.main.config_warning", warning = warning)
            );
        }
        eprintln!("{}", acb_i18n::t!("bridge.main.warning_continue"));
        std::thread::sleep(Duration::from_secs(3));
    }

    if options.check_only {
        println!("{}", acb_i18n::t!("bridge.main.check_ok"));
        return Ok(ExitCode::SUCCESS);
    }

    install_signal_handlers();
    if let Some(console) = &config.console {
        write_health_token(&console.token);
    }

    // 先开库：库打不开就别往下走。宁可起不来，也别用内存悄悄跑起来——
    // 那样它看着一切正常，直到你重启才发现什么都没留下。
    let store = SqliteStore::open(&config.storage.path)?;
    let counts = store.counts()?;
    println!(
        "{}",
        acb_i18n::t!(
            "bridge.main.store_summary",
            path = config.storage.path,
            records = counts.records,
            rooms = counts.rooms,
            notes = counts.notes
        )
    );

    let shutdown = Arc::new(AtomicBool::new(false));

    // 扩展宿主：先装内置 provider，再装树外 manager（ADR-0026 Phase 3）。
    // 树外 manager 只是把 provider 注册进 Root；配置里引用 `trusted.*` 才生效。
    let mut plugins = PluginManager::with_native_builtins();
    plugins.install_loader(&TrustedLoader)?;
    // Lua 脚本插件是可选能力：设了 ACB_LUA_DIR 才装，目录里的 *.lua 变成 lua.* provider。
    if let Some(lua) = LuaLoader::from_env() {
        plugins.install_loader(&lua)?;
    }
    // 动态库插件：配了 [[plugins]] 才装载；插件自己报 manifest（适配器或策略 provider）。
    if !config.plugins.is_empty() {
        plugins.install_loader_owned(Arc::new(DynamicLoader::new(&config.plugins)))?;
    }
    // 外置进程工具：只承载第三方工具，第一方工具仍走进程内 ToolLoader。
    if !config.tool_plugins.is_empty() {
        plugins.install_loader_owned(Arc::new(ProcessToolLoader::new(&config.tool_plugins)))?;
    }
    // 动态扩展目录（ADR-0029 P1）：只从 ACB_DYNAMIC_DIR 发现声明式 route 模块。
    if let Some(extensions) = acb_extensions::DynamicRouteManager::from_env()? {
        plugins.install_manager(Arc::new(extensions))?;
    }
    // 平台适配器也登记在 Root 里：内置 OneBot、外置进程、动态库走同一张表。
    plugins.register_adapter(Arc::new(OneBotSnowLumaAdapterFactory))?;
    plugins.register_adapter(Arc::new(ProcessAdapterFactory))?;

    // 入站队列：适配器只往里丢记录，core 的 drain 线程统一 ingest。
    let (events, event_rx, inbound_stats) = EventQueue::channel_with(config.queues);

    // 每条连接先占一个发送口，桥建好之后再起守护线程去连。
    // 适配器由 Root 按 kind 实例化；main 不再知道 onebot11 的构造细节。
    let mut outbounds = Outbounds::new();
    let mut launches = Vec::new();
    for connection in &config.connections {
        let instance = plugins.create_adapter(connection, events.clone())?;
        let AdapterInstance {
            outbound,
            launch,
            manifest,
        } = instance;
        outbounds = outbounds
            .with(connection.id.clone(), outbound)
            .with_manifest(connection.id.clone(), manifest);
        launches.push((connection.id.clone(), launch));
    }

    let lkg_path = plugins_lkg_path(&config.storage.path);
    let bridge = Arc::new(Mutex::new(
        Bridge::with_plugins_lkg(
            config.clone(),
            store,
            Box::new(outbounds),
            plugins,
            lkg_path,
        )
        .with_dependencies(deps.clone()),
    ));

    // provider id 写错要在启动时死，不能等第一轮才报。
    {
        let guard = bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.validate_plugins()?;
    }

    // 控制指令注册表：控制台和路由插件（如 /stop）都靠它把 control 帧推给
    // 管理某会话的 harness 连接。drain 线程要先拿到它，所以建在启动线程之前。
    let controls = ControlRegistry::new();

    // 路由动作的后台执行者：发送/任务在锁外跑，不挡入站队列。
    let route_worker = RouteWorker::spawn_with_limit(
        Arc::clone(&bridge),
        Arc::clone(&shutdown),
        config.queues.route,
    );

    // core 的入站 drain 线程：所有适配器的事件都从这里进流水，顺序由队列保证。
    {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        let controls = controls.clone();
        let route_worker = route_worker.clone();
        std::thread::spawn(move || {
            drain_events(bridge, event_rx, shutdown, controls, route_worker)
        });
    }

    for (connection_id, launch) in launches {
        let host = AdapterHost {
            events: events.clone(),
            shutdown: Arc::clone(&shutdown),
            status: AdapterStatus::new(connection_id, deps.clone()),
        };
        std::thread::spawn(move || launch(host));
    }

    match &config.endpoint {
        Some(endpoint) => {
            let listener = std::net::TcpListener::bind(&endpoint.listen)?;
            let bridge = Arc::clone(&bridge);
            let shutdown = Arc::clone(&shutdown);
            let controls = controls.clone();
            let endpoint_deps = deps.clone();
            std::thread::spawn(move || {
                if let Err(error) = serve(listener, bridge, shutdown, controls, endpoint_deps) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("bridge.main.endpoint_stopped", error = error)
                    );
                }
            });
            println!(
                "{}",
                acb_i18n::t!("bridge.main.endpoint_listening", listen = endpoint.listen)
            );
        }
        None => println!("{}", acb_i18n::t!("bridge.main.endpoint_disabled")),
    }

    // 控制台：可远程访问的管理 HTTP 服务（ADR-0018）。
    match &config.console {
        Some(console) => {
            let listener = std::net::TcpListener::bind(&console.listen)?;
            let console_service = Arc::new(Console::new(
                Arc::clone(&bridge),
                controls,
                console.token.clone(),
                options.config.clone(),
                inbound_stats.clone(),
                route_worker.clone(),
                deps.clone(),
            ));
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || {
                Console::serve(listener, console_service, shutdown);
            });
            println!(
                "{}",
                acb_i18n::t!("bridge.main.console_listening", listen = console.listen)
            );
        }
        None => println!("{}", acb_i18n::t!("bridge.main.console_disabled")),
    }

    // 文件中继：平台在别的文件系统命名空间里时（典型：平台在容器、agent 在宿主机），
    // 桥替 agent 把本机文件收下来换成平台取得到的 URL（ADR-0025）。
    // 发布（写文件）在 Bridge 里做，这里只起取文件的 HTTP 服务——
    // 两边各持一个 FileRelay 实例，共享同一个目录，token 目录就是事实来源。
    match &config.file_relay {
        Some(relay_config) => {
            let relay = Arc::new(FileRelay::new(&relay_config.dir, &relay_config.public_base));
            let removed = relay.sweep_expired();
            if removed > 0 {
                println!(
                    "{}",
                    acb_i18n::t!("bridge.main.file_relay_swept", count = removed)
                );
            }
            // 运行中定期清理：长期运行的桥不能只靠重启来削过期文件。
            {
                let relay = Arc::clone(&relay);
                let shutdown = Arc::clone(&shutdown);
                std::thread::spawn(move || {
                    while !shutdown.load(Ordering::Relaxed) {
                        std::thread::sleep(RELAY_SWEEP_EVERY);
                        if shutdown.load(Ordering::Relaxed) {
                            break;
                        }
                        let removed = relay.sweep_expired();
                        if removed > 0 {
                            println!(
                                "{}",
                                acb_i18n::t!("bridge.main.file_relay_swept", count = removed)
                            );
                        }
                    }
                });
            }
            let listener = std::net::TcpListener::bind(&relay_config.listen)?;
            let shutdown = Arc::clone(&shutdown);
            std::thread::spawn(move || FileRelay::serve(listener, relay, shutdown));
            println!(
                "{}",
                acb_i18n::t!(
                    "bridge.main.file_relay_listening",
                    listen = relay_config.listen,
                    public_base = relay_config.public_base,
                    dir = relay_config.dir
                )
            );
        }
        None => println!("{}", acb_i18n::t!("bridge.main.file_relay_disabled")),
    }

    {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || {
            // 起来先扫一次：上次跑完留下的旧账不用等到下个周期才削。
            prune_once(&bridge);
            while !shutdown.load(Ordering::Relaxed) {
                std::thread::sleep(PRUNE_EVERY);
                prune_once(&bridge);
            }
        });
    }

    // 时钟：每秒评估一次订阅的时钟策略（ADR-0011）。默认全关，不配就不产生轮次。
    {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        let clock_deps = deps.clone();
        std::thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis() as i64)
                    .unwrap_or(0);
                let queued = {
                    let mut guard = bridge
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.tick_clock(now)
                };
                // 启动宽限期：从未出现过的 harness 在 30s 后转 down(not_started)。
                clock_deps.tick(now);
                if queued > 0 {
                    println!(
                        "{}",
                        acb_i18n::t!("bridge.main.clock_queued", queued = queued)
                    );
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    println!("{}", acb_i18n::t!("bridge.main.running"));
    let exit_code = wait_for_stop(&bridge, &route_worker, &shutdown);
    shutdown.store(true, Ordering::SeqCst);
    Ok(exit_code)
}

/// 插件 LKG 状态文件：跟存储同目录，不归 Nix 所有，也不需要 root。
fn plugins_lkg_path(storage_path: &str) -> PathBuf {
    Path::new(storage_path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("plugins-lkg.toml")
}

/// 主循环：等退出信号。SIGTERM = 排空后正常退出；SIGHUP = 排空后请求重启（75）。
fn wait_for_stop<S: acb_kernel::Store + Send + 'static>(
    bridge: &Arc<Mutex<Bridge<S>>>,
    worker: &RouteWorker,
    shutdown: &AtomicBool,
) -> ExitCode {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return ExitCode::SUCCESS;
        }
        let signal = PENDING_SIGNAL.swap(0, Ordering::SeqCst);
        if signal == 0 {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        let restarting = signal == libc::SIGHUP as usize;
        let snapshot = {
            let mut bridge = bridge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            bridge.begin_drain()
        };
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.drain_start",
                signal = signal,
                active = snapshot["active_turns"],
                queued = snapshot["queued_turns"],
                tasks = snapshot["running_tasks"]
            )
        );

        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut drained = false;
        let mut last = snapshot;
        while Instant::now() < deadline {
            last = {
                let bridge = bridge
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                bridge.drain_snapshot()
            };
            if last["active_turns"].as_u64() == Some(0)
                && last["running_tasks"].as_u64() == Some(0)
                && worker.busy() == 0
            {
                drained = true;
                break;
            }
            std::thread::sleep(DRAIN_POLL);
        }

        if !drained {
            let leftovers = {
                let bridge = bridge
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                bridge.drain_leftovers()
            };
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "bridge.main.drain_timeout",
                    timeout = DRAIN_TIMEOUT.as_secs(),
                    leftover = leftovers
                )
            );
        }
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.drain_done",
                state = last["state"],
                active = last["active_turns"],
                queued = last["queued_turns"]
            )
        );
        return if restarting {
            ExitCode::from(EXIT_RESTART)
        } else {
            ExitCode::SUCCESS
        };
    }
}

/// 把控制台 token 写进运行时目录，供本机健康探针读取（0600）。
///
/// 探针脚本只需要文件路径，不必把 token 抄进单元文件；token 本身仍只来自配置。
fn write_health_token(token: &str) {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join("acb-health.token");
    if let Err(error) = std::fs::write(&path, token) {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "bridge.main.health_token_failed",
                path = path.display(),
                error = error
            )
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "bridge.main.health_token_failed",
                    path = path.display(),
                    error = error
                )
            );
        }
    }
}

/// 扫一次旧流水。删了什么就打一行，没删就不吭声——安静的日志才好读。
fn prune_once(bridge: &Mutex<Bridge<SqliteStore>>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0);

    let report = {
        // 锁被毒死也接着用：桥的状态没有「半条」这种中间态（同 endpoint::lock）。
        let mut bridge = bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        bridge.prune(now)
    };

    match report {
        Ok(pruned) => {
            for item in pruned {
                println!(
                    "{}",
                    acb_i18n::t!(
                        "bridge.main.prune",
                        room = item.room,
                        deleted = item.deleted,
                        kept = item.kept
                    )
                );
            }
        }
        Err(error) => {
            eprintln!(
                "{}",
                acb_i18n::t!("bridge.main.prune_failed", error = error)
            )
        }
    }
}

/// 命令行参数。
struct Options {
    config: String,
    check_only: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = "config.toml".to_owned();
        let mut check_only = false;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    config = args
                        .next()
                        .ok_or(acb_i18n::t!("bridge.main.config_arg_required"))?;
                }
                "--check" => check_only = true,
                "--help" | "-h" => {
                    println!("{}", acb_i18n::t!("bridge.main.usage"));
                    std::process::exit(0);
                }
                other => {
                    return Err(acb_i18n::t!("bridge.main.unknown_arg", arg = other).into());
                }
            }
        }

        Ok(Self { config, check_only })
    }
}

fn print_summary(config: &Config, path: &str) {
    println!("{}", acb_i18n::t!("bridge.main.config_path", path = path));

    println!(
        "{}",
        acb_i18n::t!(
            "bridge.main.connections_count",
            count = config.connections.len()
        )
    );
    for connection in &config.connections {
        let url = connection
            .url
            .clone()
            .unwrap_or_else(|| acb_i18n::t!("bridge.main.no_url"));
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.connection_line",
                id = connection.id,
                platform = connection.platform,
                kind = connection.kind,
                transport = connection.transport.as_str(),
                url = url,
                account = connection.account
            )
        );
    }

    println!(
        "{}",
        acb_i18n::t!("bridge.main.personas_count", count = config.personas.len())
    );
    for persona in &config.personas {
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.persona_line",
                id = persona.id,
                name = persona.name
            )
        );
    }

    println!(
        "{}",
        acb_i18n::t!("bridge.main.memories_count", count = config.memories.len())
    );
    for memory in &config.memories {
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.memory_line",
                id = memory.id,
                name = memory.name,
                kind = memory.kind,
                location = memory.location
            )
        );
    }

    println!(
        "{}",
        acb_i18n::t!("bridge.main.sessions_count", count = config.sessions.len())
    );
    for session in &config.sessions {
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.session_line",
                id = session.id,
                harness = session.harness,
                persona = session.persona,
                memories = format!("{:?}", session.memories),
                subscriptions = session.subscriptions.len()
            )
        );
        for subscription in &session.subscriptions {
            println!(
                "{}",
                acb_i18n::t!("bridge.main.subscription_line", room = subscription.room)
            );
        }
    }

    let retention = config.retention;
    if retention.enabled() {
        let by_count = if retention.max_records_per_room > 0 {
            acb_i18n::t!(
                "bridge.main.retention_by_count",
                count = retention.max_records_per_room
            )
        } else {
            acb_i18n::t!("bridge.main.retention_no_count")
        };
        let by_age = if retention.max_age_days > 0 {
            acb_i18n::t!(
                "bridge.main.retention_by_age",
                days = retention.max_age_days
            )
        } else {
            acb_i18n::t!("bridge.main.retention_no_age")
        };
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.retention_line",
                by_count = by_count,
                by_age = by_age
            )
        );
    } else {
        println!("{}", acb_i18n::t!("bridge.main.retention_disabled"));
    }
}
