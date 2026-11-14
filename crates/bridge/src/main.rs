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
use acb_adapter_qqbot_official::QqBotOfficialAdapterFactory;
use acb_adapter_process::ProcessAdapterFactory;
use acb_kernel::storage::SettingsStore;
use acb_kernel::{
    AdapterHost, AdapterInstance, AdapterStatus, Bridge, Config, ConfigSource, ControlRegistry,
    DependencyRegistry, Element, EventQueue, FileRelay, Outbounds, PluginManager, RecordDraft,
    RecordKind, RouteWorker, SecretStore, Sender, SqliteStore, carrier_create_failed, drain_events,
    run_adapter, serve,
};
use acb_memory_steward::{
    Notifier as MemoryNotifier, Steward, StewardConfig, SyncNotice, Target as MemoryTarget,
};
use acb_plugin_lua::LuaLoader;
use acb_plugin_process_provider::{ProcessProviderConfig, ProcessProviderLoader};
use acb_plugin_process_tools::{ProcessNotifier, ProcessToolLoader};
use acb_plugin_trusted::TrustedLoader;

mod config_objects;
mod config_patch;
mod console;
mod plugins;
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

/// 把库内密钥叠加到文件来源之上。密文缺 `ACB_KEY` 时启动失败，不静默当明文。
fn overlay_db_secrets(
    loaded: &mut acb_kernel::LoadedConfig,
    store: &impl acb_kernel::storage::SecretsStore,
) -> Result<(), Box<dyn std::error::Error>> {
    let rows = store.secrets_load()?;
    if rows.is_empty() {
        return Ok(());
    }
    let key = acb_kernel::crypto::AcbKey::load()?;
    for row in rows {
        let value = if row.encrypted {
            let key = key.as_ref().ok_or_else(|| {
                format!("secret {} is encrypted but ACB_KEY is not set", row.name)
            })?;
            key.decrypt(&row.value)
                .map_err(|error| format!("decrypt secret {}: {error}", row.name))?
        } else {
            row.value
        };
        loaded.secrets.insert_runtime(row.name, value);
    }
    Ok(())
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

    let config_dir = options.config_dir_path();
    // 密钥入库（P2.5）：先把库里的密文解开装进快照，后面每条重载路径都能用上。
    // 库还没建、没有密钥行时返回空表，加载回落到 [secrets] 声明，行为不变。
    let preset = acb_kernel::secrets_db::preload(Path::new(&options.config), Some(&config_dir))
        .map_err(|error| format!("preload secrets: {error}"))?;
    if !preset.is_empty() {
        eprintln!("secrets: {} name(s) loaded from store", preset.len());
        acb_kernel::secrets_db::install(preset);
    }
    let loaded = Config::load(Path::new(&options.config), Some(&config_dir))?;
    // 先开库：业务对象（连接/会话/人格/记忆/harness）以库为准，配置里那五段只在
    // 库为空时被导入一次。库打不开就别往下走——用内存悄悄跑起来比起不来更糟。
    let mut store = SqliteStore::open(&loaded.config.storage.path)?;
    let mut loaded = if options.check_only {
        // --check 零副作用：只读对齐，不做首次导入（2026-11-09 实测曾把
        // 「空文件 + 未初始化的开发库」写成空导入）。
        config_objects::inspect(&store, Path::new(&options.config), &config_dir, loaded)?
    } else {
        config_objects::sync(&mut store, Path::new(&options.config), &config_dir, loaded)?
    };
    overlay_db_secrets(&mut loaded, &store)?;
    loaded.config.reset_runtime_defaults();
    loaded.config.apply_settings(&store.settings_load()?)?;
    // 插件清单同样以库为准：首次启动把 `[[tool_plugins]]` seed 进库，之后只读库。
    // 命令不可执行只标记 degraded，不阻塞启动——插件坏了不该让桥起不来。
    let plugin_runtimes = plugins::sync(
        &mut store,
        &loaded.config.tool_plugins,
        Path::new(&options.config),
        &config_dir,
        !options.check_only,
    )?;
    for runtime in &plugin_runtimes {
        if let Some(reason) = runtime.reason() {
            eprintln!("plugin {}: degraded ({reason})", runtime.plugin.id);
        }
    }
    let tool_plugins: Vec<_> = plugin_runtimes
        .iter()
        .filter(|runtime| runtime.callable())
        .map(|runtime| {
            let mut config = runtime.plugin.tool_config();
            // 配置从库来，以 ACB_PLUGIN_* 注入子进程；读失败不拦启动，只记一笔。
            match acb_kernel::plugin_admin::config_env(&store, &runtime.plugin) {
                Ok(env) => config.env = env.into_iter().collect(),
                Err(error) => eprintln!(
                    "plugin {}: config not injected ({error})",
                    runtime.plugin.id
                ),
            }
            config
        })
        .collect();
    let config = loaded.config;
    let sources = loaded.sources;
    let duplicate_rules = loaded.duplicate_rules;
    // 记忆管家的配置独立校验：开关、调度器、合并策略都在这里定，core 不硬编码。
    // 分片模式下用合并后的 TOML，保证 fragment 里的段也能被这些独立解析器看到。
    let steward_config = StewardConfig::from_toml(&loaded.merged_toml)?;
    // 外置进程 provider 同样独立校验；列表为空就不启动任何子进程。
    let process_providers = ProcessProviderConfig::from_toml(&loaded.merged_toml)?;
    print_summary(
        &config,
        &options.config,
        &sources,
        duplicate_rules,
        &loaded.secrets,
    );
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
                        source = notifications.topic_source.as_deref().unwrap_or("<unknown>")
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
        // 通知配了却找不到子进程，线上就是静默降级；--check 必须能拦住。
        // 这里只查路径，不 spawn（check 不应产生副作用）。
        if config
            .notifications
            .as_ref()
            .filter(|notifications| notifications.active())
            .is_some()
        {
            ProcessNotifier::preflight().map_err(|error| {
                acb_i18n::t!("bridge.main.notifications_check_failed", error = error)
            })?;
        }
        println!("{}", acb_i18n::t!("bridge.main.check_ok"));
        return Ok(ExitCode::SUCCESS);
    }

    install_signal_handlers();
    if let Some(console) = &config.console {
        write_health_token(&console.token);
    }

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
    // 外置进程工具：清单来自库，只有启用且命令可执行的才装；第一方工具仍走进程内 ToolLoader。
    // 动作执行器跟着 provider 走：控制台点「执行」时调用同一队列。
    let mut action_runners: Vec<Arc<dyn acb_kernel::PluginActionRunner>> = Vec::new();
    if !tool_plugins.is_empty() {
        let loader = Arc::new(ProcessToolLoader::new(&tool_plugins));
        plugins.install_loader_owned(Arc::clone(&loader) as Arc<dyn acb_kernel::PluginLoader>)?;
        action_runners = loader.action_runners();
    }
    // 外置进程策略 provider：核心不认识 wasm，只按命令启动宿主并转发 decide。
    // 列表为空 = 不装宿主；配置里有但宿主起不来 = 启动期硬失败。
    if !process_providers.is_empty() {
        plugins.install_loader_owned(Arc::new(ProcessProviderLoader::new(&process_providers)))?;
        println!(
            "process-provider: installed {} external provider(s)",
            process_providers.len()
        );
    }
    // 动态扩展目录（ADR-0029 P1）：只从 ACB_DYNAMIC_DIR 发现声明式 route 模块。
    if let Some(extensions) = acb_extensions::DynamicRouteManager::from_env()? {
        plugins.install_manager(Arc::new(extensions))?;
    }
    // 平台适配器也登记在 Root 里：内置 OneBot、外置进程、动态库走同一张表。
    plugins.register_adapter(Arc::new(OneBotSnowLumaAdapterFactory))?;
    plugins.register_adapter(Arc::new(QqBotOfficialAdapterFactory))?;
    plugins.register_adapter(Arc::new(ProcessAdapterFactory))?;

    // 入站队列：适配器只往里丢记录，core 的 drain 线程统一 ingest。
    let (events, event_rx, inbound_stats) = EventQueue::channel_with(config.queues);

    // 适配器自有连接（state/adapters/<目录>/connections.db）由 kernel 的配置
    // 构建阶段注入（校验之前），见 kernel config.rs 的 inject_adapter_connections。
    // 每条连接先占一个发送口，桥建好之后再起守护线程去连。
    // 适配器由 Root 按 kind 实例化；main 不再知道 onebot11 的构造细节。
    let mut outbounds = Outbounds::new();
    let mut launches = Vec::new();
    for connection in &config.connections {
        let instance = match plugins.create_adapter(connection, events.clone()) {
            Ok(instance) => instance,
            Err(error) => {
                // 载体没加载起来是硬失败：立即把依赖条目打到 down，桥继续跑。
                // 不能再 `?` 直接退出——那样 /healthz 根本没机会展示这条连接。
                carrier_create_failed(&deps, &connection.id, &error.to_string());
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "bridge.main.adapter_create_failed",
                        id = connection.id,
                        error = error
                    )
                );
                continue;
            }
        };
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
        Bridge::with_outbounds_lkg(config.clone(), store, outbounds, plugins, lkg_path)
            .with_dependencies(deps.clone()),
    ));
    {
        let mut guard = bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.set_plugin_actions(action_runners.clone());
    }

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

    // 记忆管家：后台线程按调度器做 fetch/merge/push。它只读配置与 worktree，
    // 不碰 core 的轮次逻辑；失败/冲突写状态并通过入站队列通知会话。
    let steward_status = if steward_config.enabled {
        let targets = memory_targets(&config);
        let target_count = targets.len();
        let notifier = Arc::new(MemorySyncNotifier {
            events: events.clone(),
        });
        let steward = Steward::new(steward_config, targets, notifier)?;
        let status = steward.status_path().to_path_buf();
        let shutdown = Arc::clone(&shutdown);
        std::thread::spawn(move || steward.run_forever(shutdown.as_ref()));
        println!(
            "memory-steward: enabled, {target_count} target(s), status={}",
            status.display()
        );
        Some(status)
    } else {
        None
    };

    // 适配器自有状态目录的根：与 acb.db 同级（state/adapters/<连接名>/）。
    let adapters_state_root = std::path::Path::new(&config.storage.path)
        .parent()
        .map(|parent| parent.join("adapters"))
        .unwrap_or_else(|| std::path::PathBuf::from("state/adapters"));
    for (connection_id, launch) in launches {
        let state_dir = adapters_state_root.join(&connection_id);
        if let Err(error) = std::fs::create_dir_all(&state_dir) {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "bridge.main.adapter_state_dir_failed",
                    dir = state_dir.display().to_string(),
                    error = error
                )
            );
        }
        let host = AdapterHost {
            events: events.clone(),
            shutdown: Arc::clone(&shutdown),
            status: AdapterStatus::new(connection_id.clone(), deps.clone()),
            state_dir,
        };
        std::thread::spawn(move || run_adapter(host, launch));
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
                config_dir.display().to_string(),
                sources.clone(),
                inbound_stats.clone(),
                route_worker.clone(),
                deps.clone(),
                steward_status.clone(),
                action_runners,
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
            let relay = Arc::new(FileRelay::new(
                &relay_config.dir,
                &relay_config.public_base,
                relay_config.max_publish_bytes,
            ));
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
                    // 同一秒顺带扫票据过期：终态写申请房间流水，按会话+房间聚合唤醒。
                    guard.tick_clock(now) + guard.tick_tickets(now)
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
    // 外置进程 manager 在这里收尾：停子进程，避免手动运行时留孤儿。
    {
        let bridge = bridge
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        bridge.shutdown_plugins();
    }
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
            // 排空判据必须覆盖所有「还没落盘的部分」：只在跑的轮次/任务不算完，
            // 排队等开轮的、排队等 worker 的、已准备但还没入队的动作，都是
            // 一退出就消失的东西（2026-11-09 审查 P0-4）。
            if last["active_turns"].as_u64() == Some(0)
                && last["queued_turns"].as_u64() == Some(0)
                && last["queued_tasks"].as_u64() == Some(0)
                && last["running_tasks"].as_u64() == Some(0)
                && last["pending_route_actions"].as_u64() == Some(0)
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

/// 把配置里的「会话 × 记忆」展开成管家的同步目标。
///
/// 路径必须与 `MemoryRepo::worktree_path` 一致：`worktrees_root/<记忆id>/<会话id>`。
fn memory_targets(config: &Config) -> Vec<MemoryTarget> {
    let root = Path::new(&config.worktrees_root);
    let mut targets = Vec::new();
    for session in &config.sessions {
        let room = session
            .subscriptions
            .first()
            .map(|subscription| subscription.room.clone());
        for memory in &session.memories {
            targets.push(MemoryTarget {
                memory: memory.clone(),
                session: session.id.clone(),
                room: room.clone(),
                worktree: root.join(memory).join(&session.id),
            });
        }
    }
    targets
}

/// 管家通知：写一条合成事件进入站队列，由 drain 线程照常入库并唤醒会话。
///
/// 不直接锁 `Bridge`，也不在管家线程里做投递；队列满/关闭只打日志，
/// 下一次同步会继续写状态文件，轮次不受影响。
struct MemorySyncNotifier {
    events: EventQueue,
}

impl MemoryNotifier for MemorySyncNotifier {
    fn notify(&self, target: &MemoryTarget, notice: &SyncNotice) {
        let Some(room) = target.room.clone() else {
            eprintln!(
                "memory-steward: {} has no subscribed room; notice={notice:?}",
                target.key()
            );
            return;
        };
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        let (event, tag, text, payload) = match notice {
            SyncNotice::Conflict { files } => (
                "memory_sync_conflict",
                "conflict",
                format!(
                    "记忆 {}/{} 同步冲突，已停下且未自动选边。冲突文件（{}）：{}",
                    target.memory,
                    target.session,
                    files.len(),
                    files.join(", ")
                ),
                serde_json::json!({
                    "memory": target.memory,
                    "session": target.session,
                    "conflict_files": files,
                }),
            ),
            SyncNotice::Failure { reason } => (
                "memory_sync_failed",
                "failure",
                format!(
                    "记忆 {}/{} 同步失败（轮次不受影响）：{reason}",
                    target.memory, target.session
                ),
                serde_json::json!({
                    "memory": target.memory,
                    "session": target.session,
                    "reason": reason,
                }),
            ),
        };
        let mut draft = RecordDraft::event(
            room,
            Sender {
                id: "acb.memory-steward".into(),
                name: "memory-steward".into(),
                role: None,
            },
            at,
            event,
            vec![Element::text(text)],
            true,
        );
        if let RecordKind::Event {
            tags,
            payload: slot,
            ..
        } = &mut draft.kind
        {
            tags.push("memory_sync".into());
            tags.push(tag.into());
            *slot = Some(payload);
        }
        if let Err(error) = self.events.submit(draft) {
            eprintln!("memory-steward: cannot enqueue notice: {error}");
        }
    }
}

/// 命令行参数。
struct Options {
    config: String,
    config_dir: Option<String>,
    check_only: bool,
}

impl Options {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, Box<dyn std::error::Error>> {
        let mut config = "config.toml".to_owned();
        let mut config_dir = None;
        let mut check_only = false;
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    config = args
                        .next()
                        .ok_or(acb_i18n::t!("bridge.main.config_arg_required"))?;
                }
                "--config-dir" => {
                    let value = args
                        .next()
                        .ok_or(acb_i18n::t!("bridge.main.config_dir_arg_required"))?;
                    if value.is_empty() {
                        return Err(acb_i18n::t!("bridge.main.config_dir_arg_required").into());
                    }
                    config_dir = Some(value);
                }
                "--check" => check_only = true,
                "--help" | "-h" => {
                    println!("{}", acb_i18n::t!("bridge.main.usage"));
                    println!("{}", acb_i18n::t!("bridge.main.usage_merge"));
                    std::process::exit(0);
                }
                other => {
                    return Err(acb_i18n::t!("bridge.main.unknown_arg", arg = other).into());
                }
            }
        }

        Ok(Self {
            config,
            config_dir,
            check_only,
        })
    }

    /// 分片目录：显式 `--config-dir`，否则取 base 配置同目录下的 `config.d`。
    fn config_dir_path(&self) -> PathBuf {
        match &self.config_dir {
            Some(dir) => PathBuf::from(dir),
            None => {
                let base = Path::new(&self.config);
                base.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."))
                    .join("config.d")
            }
        }
    }
}

/// 打印连接 URL 时遮掉查询串里的 token/access_token 值，避免启动日志泄露密钥。
fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let mut parts = Vec::new();
    for pair in query.split('&') {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (pair, None),
        };
        if key.trim().eq_ignore_ascii_case("token")
            || key.trim().eq_ignore_ascii_case("access_token")
        {
            parts.push(match value {
                Some(_) => format!("{key}=***"),
                None => key.to_owned(),
            });
        } else {
            parts.push(pair.to_owned());
        }
    }
    format!("{base}?{}", parts.join("&"))
}

fn print_summary(
    config: &Config,
    path: &str,
    sources: &[ConfigSource],
    duplicate_rules: bool,
    secrets: &SecretStore,
) {
    println!("{}", acb_i18n::t!("bridge.main.config_path", path = path));
    for source in sources {
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.config_source",
                path = source.path,
                connections = source.connections,
                sessions = source.sessions,
                rules = source.rules
            )
        );
    }
    println!(
        "{}",
        acb_i18n::t!(
            "bridge.main.config_rules_total",
            count = config.grants.rules.len()
        )
    );
    if duplicate_rules {
        println!("{}", acb_i18n::t!("bridge.main.duplicate_rules_warning"));
    }

    let secret_entries = secrets.entries();
    println!(
        "{}",
        acb_i18n::t!("bridge.main.secrets_count", count = secret_entries.len())
    );
    for entry in &secret_entries {
        println!(
            "{}",
            acb_i18n::t!(
                "bridge.main.secret_source",
                name = entry.name,
                kind = entry.kind,
                origin = entry.origin
            )
        );
    }

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
                url = redact_url(&url),
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
