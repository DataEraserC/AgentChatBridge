//! 外置通知进程的宿主。
//!
//! 复用本 crate 的 stdio JSON-RPC 载体思路：bridge 把事件写成行 JSON 交给
//! `acb-notifier` 子进程，HTTP 推送与去重都在子进程里。kernel 只看到
//! [`NoticeSink`]，不引入 HTTP 客户端，也不等网络。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use acb_kernel::{NoticeEvent, NoticeSink, NotificationsConfig};
use serde::Serialize;

/// 主流程最多排多少个待写事件；满了直接丢并计数，绝不阻塞。
const NOTICE_QUEUE: usize = 128;
const WRITE_TICK: Duration = Duration::from_millis(50);
const BINARY_NAME: &str = "acb-notifier";

#[derive(Serialize)]
struct Notification<'a> {
    jsonrpc: &'static str,
    method: &'static str,
    params: &'a NoticeEvent,
}

/// 一个受 bridge 管理的通知子进程。
pub struct ProcessNotifier {
    tx: SyncSender<String>,
    shutdown: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    child: Arc<Mutex<Option<Child>>>,
    writer: Mutex<Option<JoinHandle<()>>>,
}

impl ProcessNotifier {
    /// 按 `[notifications]` 启动子进程。未配置完整时返回错误，调用方不应注册。
    pub fn spawn(config: &NotificationsConfig) -> Result<Self, String> {
        if !config.active() {
            return Err("notifications are not fully configured".to_owned());
        }
        let url = config.url.as_deref().unwrap_or_default();
        let topic = config.topic.as_deref().unwrap_or_default();
        let bin = notifier_binary();
        let mut command = Command::new(&bin);
        command
            .arg("--url")
            .arg(url)
            .arg("--topic")
            .arg(topic)
            .arg("--min-interval-secs")
            .arg(config.min_interval_secs.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(token) = config.token.as_deref() {
            command.arg("--token").arg(token);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot spawn {}: {error}", bin.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "notifier stdin is unavailable".to_owned())?;
        let (tx, rx) = sync_channel(NOTICE_QUEUE);
        let shutdown = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_shutdown = Arc::clone(&shutdown);
        let writer = thread::spawn(move || write_loop(stdin, rx, writer_shutdown));
        Ok(Self {
            tx,
            shutdown,
            dropped,
            child: Arc::new(Mutex::new(Some(child))),
            writer: Mutex::new(Some(writer)),
        })
    }

    /// 因队列满丢掉的事件数（诊断/测试用）。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl NoticeSink for ProcessNotifier {
    fn notify(&self, event: NoticeEvent) {
        let frame = Notification {
            jsonrpc: "2.0",
            method: "notify",
            params: &event,
        };
        let line = match serde_json::to_string(&frame) {
            Ok(line) => line,
            Err(error) => {
                eprintln!("notifier: cannot encode event: {error}");
                return;
            }
        };
        match self.tx.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped == 1 || dropped % 100 == 0 {
                    eprintln!("notifier: queue full; dropped={dropped}");
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                eprintln!("notifier: writer stopped; dropping event");
            }
        }
    }
}

impl Drop for ProcessNotifier {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(mut child) = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(writer) = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = writer.join();
        }
    }
}

fn write_loop(mut stdin: ChildStdin, lines: Receiver<String>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        let line = match lines.recv_timeout(WRITE_TICK) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if let Err(error) = stdin.write_all(line.as_bytes()).and_then(|_| {
            stdin.write_all(b"\n")?;
            stdin.flush()
        }) {
            eprintln!("notifier: write failed: {error}; stopping");
            break;
        }
    }
}

/// 默认找与当前可执行文件同目录的 `acb-notifier`；测试/部署可用环境变量覆盖。
fn notifier_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("ACB_NOTIFIER_BIN") {
        return PathBuf::from(path);
    }
    let exe = std::env::current_exe().unwrap_or_default();
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let name = if cfg!(windows) {
        format!("{BINARY_NAME}.exe")
    } else {
        BINARY_NAME.to_owned()
    };
    let sibling = dir.join(&name);
    if sibling.is_file() {
        return sibling;
    }
    // `cargo test` 的可执行文件在 target/<profile>/deps 下，二进制在上一级。
    dir.parent()
        .map(|parent| parent.join(name))
        .unwrap_or(sibling)
}
