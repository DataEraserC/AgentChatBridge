//! 外置进程平台适配器（ADR-0027 Phase 3）。
//!
//! 桥按连接配置里的 `options.command` 启动一个子进程，双方在 stdin/stdout 上
//! 一行一条 JSON（见 `acb-adapter-api`）。适配器实现可以用任何语言写：
//!
//! ```text
//! core ──Send/Call/Shutdown──▶ 子进程
//! core ◀──Hello/Event/Reply── 子进程
//! ```
//!
//! 这条连接对 core 来说和其它适配器完全一样：一个 `Outbound` + 一个 `launch`
//! 闭包。进程崩溃不会带倒桥；之后的发送/调用会明确返回「通道不可用」。
//!
//! 配置形状：
//!
//! ```toml
//! [[connections]]
//! id = "demo"
//! platform = "demo"
//! kind = "process"
//! transport = "process"
//! account = "bot"
//!
//! [connections.options]
//! command = ["/path/to/adapter", "--stdio"]
//! # 可选：env = { KEY = "value" }，cwd = "/path"，startup_timeout_secs = 10
//! ```

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireAdapterInfo, WireMethodSpec, WireResult, decode_line,
    encode_line,
};
use acb_kernel::wire::{call_error, convert_catalog, send_error};
use acb_kernel::{
    ADAPTER_API_VERSION, AdapterCapability, AdapterError, AdapterFactory, AdapterHost,
    AdapterInstance, AdapterManifest, CallError, CallOutcome, Connection, Element, EventQueue,
    MethodSpec, Outbound, OutboundError, OutgoingRecord, PluginEffect, PluginMetadata, RoomId,
    Sent,
};

/// 等子进程回一条动作结果的上限。
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// 子进程启动后必须在这个时间内说 hello。
pub const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// 子进程退出后的重启退避：从 500ms 起翻倍，最多 10 秒。
pub const RESTART_DELAY: Duration = Duration::from_millis(500);
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(10);
/// 退出时给子进程收尾的时间，过了就 kill。
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// 写入队列深度。64 行足够吸收一次突发；队列写满说明子进程已经不读 stdin。
const WRITE_QUEUE_LINES: usize = 64;
/// 单行写入排队的最长等待。超过就认为子进程停止读取：标记连接不可用并杀掉重启。
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("process")];
static CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    AdapterCapability::Send,
    AdapterCapability::Catalog,
];

/// 这条适配器的静态声明。平台名由连接配置里的 `platform` 决定，
/// 这里只声明「外置进程」这个载体类型。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: Cow::Borrowed("acb.adapter.process"),
    platform: Cow::Borrowed("external"),
    kind: Cow::Borrowed("process"),
    transports: Cow::Borrowed(TRANSPORTS),
    api_version: ADAPTER_API_VERSION,
    capabilities: Cow::Borrowed(CAPABILITIES),
    // 子进程自己决定发什么事件、打什么标签；载体只负责把 at_self / reply_to_self 原样带进来。
    events: Cow::Borrowed(&[]),
    event_tags: Cow::Borrowed(&[]),
    supports_at: true,
    supports_reply: true,
    text_limit: None,
    metadata: PluginMetadata {
        title: Some(Cow::Borrowed("外置进程适配器")),
        summary: Some(Cow::Borrowed(
            "按连接的 options 启动子进程，stdio JSON 行协议",
        )),
        description: None,
        tags: Cow::Borrowed(&[]),
        effects: Cow::Borrowed(&[PluginEffect::ExternalCall, PluginEffect::Nondeterministic]),
        docs_url: None,
        options_schema: None,
    },
};

/// 外置进程适配器工厂。
#[derive(Debug, Default)]
pub struct ProcessAdapterFactory;

impl AdapterFactory for ProcessAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        MANIFEST.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        let (outbound, launch, manifest) = spawn_process(connection, events)
            .map_err(|detail| AdapterError::Create(connection.id.clone(), detail))?;
        Ok(AdapterInstance {
            outbound,
            launch,
            manifest,
        })
    }
}

struct ProcessConfig {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    cwd: Option<String>,
    startup_timeout: Duration,
}

fn parse_options(connection: &Connection) -> Result<ProcessConfig, String> {
    let options = connection
        .options
        .as_object()
        .ok_or(acb_i18n::t!("adapter.process.options_not_object"))?;

    let command = options
        .get("command")
        .ok_or(acb_i18n::t!("adapter.process.command_required"))?;
    let (program, mut args) = match command {
        serde_json::Value::String(program) => {
            let program = program.trim();
            if program.is_empty() {
                return Err(acb_i18n::t!("adapter.process.command_empty"));
            }
            (program.to_owned(), Vec::new())
        }
        serde_json::Value::Array(items) => {
            let mut parts = items.iter().map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| acb_i18n::t!("adapter.process.command_array_element"))
            });
            let program = parts
                .next()
                .transpose()?
                .ok_or(acb_i18n::t!("adapter.process.command_array_empty"))?;
            let rest = parts.collect::<Result<Vec<_>, _>>()?;
            if program.trim().is_empty() {
                return Err(acb_i18n::t!("adapter.process.command_first_empty"));
            }
            (program, rest)
        }
        _ => return Err(acb_i18n::t!("adapter.process.command_wrong_type")),
    };

    if let Some(extra) = options.get("args") {
        let items = extra
            .as_array()
            .ok_or(acb_i18n::t!("adapter.process.args_not_array"))?;
        for item in items {
            args.push(
                item.as_str()
                    .ok_or(acb_i18n::t!("adapter.process.args_element"))?
                    .to_owned(),
            );
        }
    }

    let mut env = Vec::new();
    if let Some(table) = options.get("env") {
        let table = table
            .as_object()
            .ok_or(acb_i18n::t!("adapter.process.env_not_object"))?;
        for (key, value) in table {
            env.push((
                key.clone(),
                value
                    .as_str()
                    .ok_or_else(|| acb_i18n::t!("adapter.process.env_value_not_string", key = key))?
                    .to_owned(),
            ));
        }
    }

    let cwd = match options.get("cwd") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or(acb_i18n::t!("adapter.process.cwd_not_string"))?
                .to_owned(),
        ),
        None => None,
    };

    let startup_timeout = options
        .get("startup_timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT);

    Ok(ProcessConfig {
        program,
        args,
        env,
        cwd,
        startup_timeout,
    })
}

struct HelloData {
    name: String,
    catalog: Vec<WireMethodSpec>,
    info: WireAdapterInfo,
}

/// 一条连接的共享进程状态。重启时换 writer、清 pending，但 outbound 句柄不变。
struct SharedProcess {
    /// 写入队列的发送端；真正的 stdin 由 writer 线程独占，写阻塞不会占住调用方。
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    /// 写入超时或写失败；supervise 看到后杀掉子进程走重启流程。
    write_failed: Arc<AtomicBool>,
    catalog: Mutex<Arc<[MethodSpec]>>,
    /// 子进程 hello 里声明的入站能力；重启时跟着新 hello 更新。
    info: Mutex<WireAdapterInfo>,
}

// 启动期一次性返回三件异质产物（出站口、恢复闭包、manifest）；拆成类型别名反而
// 让调用点更难读，保持内联元组。
#[allow(clippy::type_complexity)]
fn spawn_process(
    connection: &Connection,
    events: EventQueue,
) -> Result<
    (
        Box<dyn Outbound>,
        Box<dyn FnOnce(AdapterHost) + Send>,
        AdapterManifest,
    ),
    String,
> {
    let config = parse_options(connection)?;
    let connection_id = connection.id.clone();
    let restart_events = events.clone();

    let shared = Arc::new(SharedProcess {
        writer: Mutex::new(None),
        pending: Arc::new(Mutex::new(HashMap::new())),
        alive: Arc::new(AtomicBool::new(false)),
        write_failed: Arc::new(AtomicBool::new(false)),
        catalog: Mutex::new(Arc::from(Vec::<MethodSpec>::new())),
        info: Mutex::new(WireAdapterInfo::default()),
    });

    let (child, writer, catalog, info) = start_child(
        &config,
        &connection_id,
        events,
        Arc::clone(&shared.pending),
        Arc::clone(&shared.alive),
        Arc::clone(&shared.write_failed),
    )?;
    *shared
        .writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(writer);
    *shared
        .catalog
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
    *shared
        .info
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = info;
    shared.alive.store(true, Ordering::Relaxed);

    // 在把 shared 交给 launch 闭包之前算好运行时报文。
    let manifest = runtime_manifest(&connection.platform, &shared);

    let outbound = ProcessOutbound {
        shared: Arc::clone(&shared),
        connection: connection_id.clone(),
        seq: 0,
        timeout: REPLY_TIMEOUT,
    };

    let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
        supervise(
            host.shutdown,
            config,
            connection_id,
            shared,
            restart_events,
            child,
        );
    });

    let outbound: Box<dyn Outbound> = Box::new(outbound);
    Ok((outbound, launch, manifest))
}

/// 静态 process manifest + 子进程 hello 里声明的入站能力。
fn runtime_manifest(platform: &str, shared: &SharedProcess) -> AdapterManifest {
    let info = shared
        .info
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let events: Vec<Cow<'static, str>> = info.events.into_iter().map(Cow::Owned).collect();
    let event_tags: Vec<Cow<'static, str>> = info.event_tags.into_iter().map(Cow::Owned).collect();
    AdapterManifest {
        platform: Cow::Owned(platform.to_owned()),
        events: events.into(),
        event_tags: event_tags.into(),
        supports_at: info.supports_at,
        supports_reply: info.supports_reply,
        text_limit: info.text_limit,
        ..MANIFEST.clone()
    }
}

/// 启动一个子进程并等到 hello。失败时把已经起的进程收掉，不留孤儿。
// 返回值是启动阶段的四元组，只在 `spawn_process` 里解构一次；类型别名无复用价值。
#[allow(clippy::type_complexity)]
fn start_child(
    config: &ProcessConfig,
    connection: &str,
    events: EventQueue,
    pending: Pending,
    alive: Arc<AtomicBool>,
    write_failed: Arc<AtomicBool>,
) -> Result<
    (
        Child,
        SyncSender<Vec<u8>>,
        Arc<[MethodSpec]>,
        WireAdapterInfo,
    ),
    String,
> {
    let mut command = Command::new(&config.program);
    command
        .args(&config.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &config.env {
        command.env(key, value);
    }
    if let Some(cwd) = &config.cwd {
        command.current_dir(cwd);
    }

    let mut child = command.spawn().map_err(|error| {
        acb_i18n::t!(
            "adapter.process.start_failed",
            program = config.program,
            error = error
        )
    })?;
    let stdin = child.stdin.take().ok_or(acb_i18n::t!(
        "adapter.process.child_no_stdio",
        stream = "stdin"
    ))?;
    let stdout = child.stdout.take().ok_or(acb_i18n::t!(
        "adapter.process.child_no_stdio",
        stream = "stdout"
    ))?;
    let stderr = child.stderr.take().ok_or(acb_i18n::t!(
        "adapter.process.child_no_stdio",
        stream = "stderr"
    ))?;

    spawn_stderr(connection.to_owned(), stderr);
    let (hello_tx, hello_rx) = mpsc::channel();
    spawn_reader(
        connection.to_owned(),
        stdout,
        events,
        pending,
        Arc::clone(&alive),
        hello_tx,
    );

    let hello = match hello_rx.recv_timeout(config.startup_timeout) {
        Ok(Ok(hello)) => hello,
        Ok(Err(detail)) => {
            stop_child(&mut child);
            return Err(detail);
        }
        Err(RecvTimeoutError::Timeout) => {
            stop_child(&mut child);
            return Err(acb_i18n::t!(
                "adapter.process.hello_timeout",
                seconds = config.startup_timeout.as_secs()
            ));
        }
        Err(RecvTimeoutError::Disconnected) => {
            stop_child(&mut child);
            return Err(acb_i18n::t!("adapter.process.hello_missing"));
        }
    };
    // hello 刚说完就退出的进程不能算启动成功，否则第一次请求会写进死管道。
    if let Ok(Some(status)) = child.try_wait() {
        stop_child(&mut child);
        return Err(acb_i18n::t!(
            "adapter.process.hello_then_exit",
            status = status
        ));
    }
    eprintln!(
        "{}",
        acb_i18n::t!(
            "adapter.process.started",
            connection = connection,
            name = hello.name,
            count = hello.catalog.len()
        )
    );

    let catalog = convert_catalog(hello.catalog).map_err(|detail| {
        stop_child(&mut child);
        acb_i18n::t!("adapter.process.catalog_invalid", detail = detail)
    })?;
    let writer = spawn_writer(
        connection.to_owned(),
        stdin,
        Arc::clone(&alive),
        write_failed,
        WRITE_QUEUE_LINES,
    );

    Ok((child, writer, catalog, hello.info))
}

/// 守着子进程：退出就按退避重启，桥要停才收尾。
fn supervise(
    host_shutdown: Arc<AtomicBool>,
    config: ProcessConfig,
    connection: String,
    shared: Arc<SharedProcess>,
    events: EventQueue,
    mut child: Child,
) {
    let mut delay = RESTART_DELAY;
    loop {
        let shutdown = wait_for_exit(&host_shutdown, &shared, &mut child, &connection);
        shared.alive.store(false, Ordering::Relaxed);
        *shared
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        let waiting = std::mem::take(
            &mut *shared
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, reply) in waiting {
            let _ = reply.send(Err(acb_i18n::t!("adapter.process.exited")));
        }
        if shutdown {
            return;
        }
        let _ = child.wait();

        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.process.restarting",
                connection = connection,
                millis = delay.as_millis()
            )
        );
        let wake = Instant::now() + delay;
        while Instant::now() < wake {
            if host_shutdown.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }

        match start_child(
            &config,
            &connection,
            events.clone(),
            Arc::clone(&shared.pending),
            Arc::clone(&shared.alive),
            Arc::clone(&shared.write_failed),
        ) {
            Ok((new_child, writer, catalog, info)) => {
                shared.write_failed.store(false, Ordering::Relaxed);
                *shared
                    .writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(writer);
                *shared
                    .catalog
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = catalog;
                *shared
                    .info
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = info;
                shared.alive.store(true, Ordering::Relaxed);
                child = new_child;
                delay = RESTART_DELAY;
            }
            Err(detail) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.process.restart_failed",
                        connection = connection,
                        detail = detail
                    )
                );
                delay = (delay * 2).min(MAX_RESTART_DELAY);
            }
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<WireResult, String>>>>>;

fn spawn_stderr(connection: String, stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.process.stderr_line",
                    connection = connection,
                    line = line
                )
            );
        }
    });
}

fn spawn_reader(
    connection: String,
    stdout: std::process::ChildStdout,
    events: EventQueue,
    pending: Pending,
    alive: Arc<AtomicBool>,
    hello_tx: Sender<Result<HelloData, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut hello_tx = Some(hello_tx);
        let mut hello_seen = false;

        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "adapter.process.read_output_failed",
                            connection = connection,
                            error = error
                        )
                    );
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }

            let message = match decode_line::<AdapterMessage>(&line) {
                Ok(message) => message,
                Err(error) => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "adapter.process.bad_wire_message",
                            connection = connection,
                            error = error
                        )
                    );
                    continue;
                }
            };

            if !hello_seen {
                match message {
                    AdapterMessage::Hello {
                        name,
                        catalog,
                        info,
                    } => {
                        hello_seen = true;
                        if let Some(sender) = hello_tx.take() {
                            let _ = sender.send(Ok(HelloData {
                                name,
                                catalog,
                                info,
                            }));
                        }
                    }
                    _ => {
                        if let Some(sender) = hello_tx.take() {
                            let _ =
                                sender.send(Err(acb_i18n::t!("adapter.process.first_line_hello")));
                        }
                        return;
                    }
                }
                continue;
            }

            match message {
                AdapterMessage::Hello { .. } => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("adapter.process.duplicate_hello", connection = connection)
                    );
                }
                AdapterMessage::Event { record } => {
                    if !events.submit(record.into()) {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.process.inbound_queue_closed",
                                connection = connection
                            )
                        );
                    }
                }
                AdapterMessage::Reply { id, result } => {
                    let reply = pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(result));
                    }
                }
                AdapterMessage::Log { level, message } => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "adapter.process.log_line",
                            connection = connection,
                            level = level,
                            message = message
                        )
                    );
                }
            }
        }

        // 子进程没了：所有等回话的请求都明确失败，不让 core 无限挂着。
        alive.store(false, Ordering::Relaxed);
        let waiting = std::mem::take(
            &mut *pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, reply) in waiting {
            let _ = reply.send(Err(acb_i18n::t!("adapter.process.exited")));
        }
        if !hello_seen {
            if let Some(sender) = hello_tx.take() {
                let _ = sender.send(Err(acb_i18n::t!("adapter.process.hello_missing")));
            }
        }
    });
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 等子进程退出；桥要停时先发 shutdown，超时就 kill。返回 true 表示是桥要停。
fn wait_for_exit(
    host_shutdown: &AtomicBool,
    shared: &SharedProcess,
    child: &mut Child,
    connection: &str,
) -> bool {
    loop {
        // 写超时/写失败：子进程已经停止读 stdin。杀掉它，让上面的重启流程接管。
        if shared.write_failed.load(Ordering::Relaxed) {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.process.write_stalled_kill",
                    connection = connection
                )
            );
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
        if host_shutdown.load(Ordering::Relaxed) {
            let _ = write_line(shared, connection, &CoreMessage::Shutdown);
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return true,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) | Err(_) => {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.process.shutdown_timeout",
                                connection = connection
                            )
                        );
                        let _ = child.kill();
                        let _ = child.wait();
                        return true;
                    }
                }
            }
        }
        match child.try_wait() {
            Ok(Some(_)) => return false,
            Ok(None) => {}
            Err(_) => return false,
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// writer 线程独占 stdin；调用方只把编码好的行放进有界队列，不持锁等 I/O。
fn spawn_writer(
    connection: String,
    mut stdin: ChildStdin,
    alive: Arc<AtomicBool>,
    write_failed: Arc<AtomicBool>,
    capacity: usize,
) -> SyncSender<Vec<u8>> {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(capacity);
    thread::spawn(move || {
        while let Ok(payload) = rx.recv() {
            if let Err(error) = stdin.write_all(&payload).and_then(|_| stdin.flush()) {
                write_failed.store(true, Ordering::Relaxed);
                alive.store(false, Ordering::Relaxed);
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.process.write_failed_detail",
                        connection = connection,
                        error = error
                    )
                );
                break;
            }
        }
    });
    tx
}

fn write_line(
    shared: &SharedProcess,
    connection: &str,
    message: &CoreMessage,
) -> Result<(), String> {
    write_line_with_timeout(shared, connection, message, WRITE_TIMEOUT)
}

fn write_line_with_timeout(
    shared: &SharedProcess,
    connection: &str,
    message: &CoreMessage,
    timeout: Duration,
) -> Result<(), String> {
    let line = encode_line(message).map_err(|error| error.to_string())?;
    let payload = format!("{line}\n").into_bytes();
    let sender = shared
        .writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
        .ok_or_else(|| acb_i18n::t!("adapter.process.not_running"))?;
    let deadline = Instant::now() + timeout;
    let mut payload = payload;
    loop {
        match sender.try_send(payload) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(_)) => {
                return Err(acb_i18n::t!("adapter.process.not_running"));
            }
            Err(TrySendError::Full(returned)) => {
                payload = returned;
                if Instant::now() >= deadline {
                    shared.write_failed.store(true, Ordering::Relaxed);
                    shared.alive.store(false, Ordering::Relaxed);
                    let detail = acb_i18n::t!(
                        "adapter.process.write_timeout",
                        connection = connection,
                        millis = timeout.as_millis()
                    );
                    eprintln!("{detail}");
                    return Err(detail);
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

struct ProcessOutbound {
    shared: Arc<SharedProcess>,
    connection: String,
    seq: u64,
    timeout: Duration,
}

impl ProcessOutbound {
    fn request(&mut self, message: CoreMessage, id: u64) -> Result<WireResult, String> {
        if !self.shared.alive.load(Ordering::Relaxed) {
            return Err(acb_i18n::t!("adapter.process.not_running"));
        }

        let (reply_tx, reply_rx) = mpsc::channel();
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, reply_tx);

        if let Err(detail) = write_line(&self.shared, &self.connection, &message) {
            self.shared
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            self.shared.alive.store(false, Ordering::Relaxed);
            return Err(detail);
        }

        match reply_rx.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.shared
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                Err(acb_i18n::t!("adapter.process.call_timeout"))
            }
            Err(RecvTimeoutError::Disconnected) => Err(acb_i18n::t!("adapter.process.exited")),
        }
    }
}

impl Outbound for ProcessOutbound {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unavailable(acb_i18n::t!(
                "adapter.process.connection_mismatch",
                connection = self.connection,
                room = room.connection
            )));
        }

        self.seq += 1;
        let id = self.seq;
        let result = self
            .request(
                CoreMessage::Send {
                    id,
                    room: room.clone(),
                    elements: elements.to_vec(),
                },
                id,
            )
            .map_err(OutboundError::Unavailable)?;

        match result {
            WireResult::Sent { platform_id, at } => Ok(Sent { platform_id, at }),
            WireResult::Error { error } => Err(send_error(error)),
            _ => Err(OutboundError::Unavailable(acb_i18n::t!(
                "adapter.process.not_send_result"
            ))),
        }
    }

    fn catalog(&self, _connection: &str) -> Arc<[MethodSpec]> {
        Arc::clone(
            &self
                .shared
                .catalog
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    fn call(
        &mut self,
        room: &RoomId,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<CallOutcome, CallError> {
        if room.connection != self.connection {
            return Err(CallError::Forbidden(acb_i18n::t!(
                "adapter.process.call_connection_mismatch",
                connection = self.connection,
                room = room.connection
            )));
        }

        self.seq += 1;
        let id = self.seq;
        let result = self
            .request(
                CoreMessage::Call {
                    id,
                    room: room.clone(),
                    method: method.to_owned(),
                    params: params.clone(),
                },
                id,
            )
            .map_err(CallError::Unavailable)?;

        match result {
            WireResult::Call { value, record } => {
                let record = record
                    .map(OutgoingRecord::try_from)
                    .transpose()
                    .map_err(CallError::Unavailable)?;
                Ok(CallOutcome { value, record })
            }
            WireResult::Error { error } => Err(call_error(error)),
            _ => Err(CallError::Unavailable(acb_i18n::t!(
                "adapter.process.not_call_result"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_timeout_marks_connection_failed_and_supervisor_kills_child() {
        // 一个只 sleep、不读 stdin 的子进程。
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("能起 sh");
        let stdin = child.stdin.take().expect("有 stdin");

        let alive = Arc::new(AtomicBool::new(true));
        let write_failed = Arc::new(AtomicBool::new(false));
        // 容量 0：writer 一旦卡在写管道，下一次排队就会超时。
        let writer = spawn_writer(
            "test".to_owned(),
            stdin,
            Arc::clone(&alive),
            Arc::clone(&write_failed),
            0,
        );
        let shared = SharedProcess {
            writer: Mutex::new(Some(writer.clone())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            alive: Arc::clone(&alive),
            write_failed: Arc::clone(&write_failed),
            catalog: Mutex::new(Arc::from(Vec::<MethodSpec>::new())),
            info: Mutex::new(WireAdapterInfo::default()),
        };

        // 1 MiB 远大于管道缓冲；writer 取走后会卡在 write_all。
        // 容量 0 的队列只在 writer 正等 recv 时才能入队，因此重试到它接手为止。
        let big = vec![b'x'; 1024 * 1024];
        let deadline = Instant::now() + Duration::from_millis(500);
        let mut pending = Some(big);
        while let Some(payload) = pending.take() {
            match writer.try_send(payload) {
                Ok(()) => break,
                Err(TrySendError::Full(payload)) => {
                    assert!(Instant::now() < deadline, "writer 线程没有启动");
                    pending = Some(payload);
                    thread::sleep(Duration::from_millis(5));
                }
                Err(TrySendError::Disconnected(_)) => panic!("writer 线程意外退出"),
            }
        }

        let error = write_line_with_timeout(
            &shared,
            "test",
            &CoreMessage::Shutdown,
            Duration::from_millis(100),
        )
        .expect_err("子进程不读 stdin 时必须超时");
        assert!(error.contains("write"), "{error}");
        assert!(write_failed.load(Ordering::Relaxed));
        assert!(!alive.load(Ordering::Relaxed));

        // supervise 的检查分支看到 write_failed 后应当杀掉子进程。
        let host_shutdown = AtomicBool::new(false);
        assert!(!wait_for_exit(&host_shutdown, &shared, &mut child, "test"));
        assert!(
            child.try_wait().expect("能查状态").is_some(),
            "子进程应已被杀掉"
        );
    }
}
