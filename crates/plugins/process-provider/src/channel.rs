//! 子进程通道：启动、stdio 行读写、decide 往返、健康与收尾。
//!
//! 与 `crates/adapters/process` 的适配器通道同构，但只承载策略 provider：
//! 不建 `Outbound`，不接收平台事件，只做 `decide` 请求/回执。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_adapter_api::{CoreMessage, WireResult, decode_line, encode_line};
use acb_kernel::{Health, PLUGIN_API_VERSION};

use crate::config::ProcessProviderConfig;
use crate::protocol::{HostLine, ProviderManifestLine};

const WRITE_QUEUE_LINES: usize = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type Pending = Arc<Mutex<HashMap<u64, Sender<Result<WireResult, String>>>>>;

pub struct ProcessChannel {
    label: String,
    child: Mutex<Option<Child>>,
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    seq: AtomicU64,
    timeout: Duration,
}

impl ProcessChannel {
    /// 启动子进程并等第一行 manifest。失败时把已经起的进程收掉，不留孤儿。
    pub fn spawn(
        config: &ProcessProviderConfig,
    ) -> Result<(Arc<Self>, ProviderManifestLine), String> {
        let mut command = Command::new(&config.command[0]);
        command
            .args(&config.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &config.env {
            command.env(key, value);
        }
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }

        let label = config.id.clone();
        let mut child = command.spawn().map_err(|error| {
            format!(
                "process provider {label}: cannot start {}: {error}",
                config.command[0]
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("process provider {label}: no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("process provider {label}: no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| format!("process provider {label}: no stderr"))?;

        spawn_stderr(label.clone(), stderr);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let failure = Arc::new(Mutex::new(None));
        let (hello_tx, hello_rx) = mpsc::channel();
        spawn_reader(
            label.clone(),
            stdout,
            Arc::clone(&pending),
            Arc::clone(&alive),
            Arc::clone(&failure),
            hello_tx,
        );

        let manifest = match hello_rx.recv_timeout(config.startup_timeout()) {
            Ok(Ok(manifest)) => manifest,
            Ok(Err(detail)) => {
                stop_child(&mut child);
                return Err(format!("process provider {label}: {detail}"));
            }
            Err(RecvTimeoutError::Timeout) => {
                stop_child(&mut child);
                return Err(format!("process provider {label}: manifest timeout"));
            }
            Err(RecvTimeoutError::Disconnected) => {
                stop_child(&mut child);
                return Err(format!(
                    "process provider {label}: process exited before manifest"
                ));
            }
        };

        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "process provider {label}: exited right after manifest ({status})"
            ));
        }
        if manifest.extension != config.extension {
            stop_child(&mut child);
            return Err(format!(
                "process provider {label}: manifest extension {} != configured {}",
                manifest.extension, config.extension
            ));
        }
        if manifest.api_version != PLUGIN_API_VERSION {
            stop_child(&mut child);
            return Err(format!(
                "process provider {label}: manifest api_version {} != {}",
                manifest.api_version, PLUGIN_API_VERSION
            ));
        }

        let writer = spawn_writer(
            label.clone(),
            stdin,
            Arc::clone(&alive),
            Arc::clone(&failure),
        );
        let channel = Arc::new(Self {
            label,
            child: Mutex::new(Some(child)),
            writer: Mutex::new(Some(writer)),
            pending,
            alive,
            failure,
            seq: AtomicU64::new(0),
            timeout: config.call_timeout(),
        });
        Ok((channel, manifest))
    }

    pub fn health(&self) -> Health {
        if self.alive.load(Ordering::Relaxed) {
            return Health::Healthy;
        }
        let detail = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| "process provider is down".to_owned());
        Health::Unhealthy(detail)
    }

    pub fn decide(
        &self,
        options: serde_json::Value,
        request: serde_json::Value,
    ) -> Result<WireResult, String> {
        self.request(CoreMessage::Decide {
            id: self.seq.fetch_add(1, Ordering::Relaxed),
            point: "access".to_owned(),
            options,
            request,
        })
    }

    pub fn shutdown(&self) {
        if self.alive.load(Ordering::Relaxed) {
            let _ = self.send_line(&CoreMessage::Shutdown);
            let deadline = Instant::now() + SHUTDOWN_GRACE;
            loop {
                let mut slot = self
                    .child
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(child) = slot.as_mut() else {
                    break;
                };
                match child.try_wait() {
                    Ok(Some(_)) => {
                        *slot = None;
                        break;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        drop(slot);
                        thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) | Err(_) => break,
                }
            }
        }
        self.kill();
        self.mark_down("process provider shut down");
    }

    pub fn note_invalid_reply(&self, detail: &str) {
        self.mark_down(detail);
    }

    fn request(&self, message: CoreMessage) -> Result<WireResult, String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(self.down_detail());
        }
        let id = match &message {
            CoreMessage::Decide { id, .. } => *id,
            _ => return Err("unsupported process provider request".to_owned()),
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, reply_tx);

        if let Err(detail) = self.send_line(&message) {
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            return Err(detail);
        }

        match reply_rx.recv_timeout(self.timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
                self.mark_down(&format!("decide timed out after {:?}", self.timeout));
                self.kill();
                Err(self.down_detail())
            }
            Err(RecvTimeoutError::Disconnected) => {
                Err("process provider reply channel closed".to_owned())
            }
        }
    }

    fn send_line(&self, message: &CoreMessage) -> Result<(), String> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(self.down_detail());
        }
        let line = encode_line(message).map_err(|error| error.to_string())?;
        let payload = format!("{line}\n").into_bytes();
        let sender = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or_else(|| self.down_detail())?;
        let deadline = Instant::now() + WRITE_TIMEOUT;
        let mut payload = payload;
        loop {
            match sender.try_send(payload) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => {
                    self.mark_down("process provider stdin closed");
                    return Err(self.down_detail());
                }
                Err(TrySendError::Full(returned)) => {
                    payload = returned;
                    if Instant::now() >= deadline {
                        self.mark_down("process provider is not reading stdin");
                        self.kill();
                        return Err(self.down_detail());
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }
    }

    fn kill(&self) {
        let mut slot = self
            .child
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(mut child) = slot.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn mark_down(&self, reason: &str) {
        self.alive.store(false, Ordering::Relaxed);
        let mut failure = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failure.is_none() {
            *failure = Some(format!("{}: {reason}", self.label));
        }
    }

    fn down_detail(&self) -> String {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .unwrap_or_else(|| format!("{}: process provider is down", self.label))
    }
}

impl Drop for ProcessChannel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_stderr(label: String, stderr: std::process::ChildStderr) {
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("process-provider[{label}] stderr: {line}");
        }
    });
}

fn spawn_reader(
    label: String,
    stdout: std::process::ChildStdout,
    pending: Pending,
    alive: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    hello_tx: Sender<Result<ProviderManifestLine, String>>,
) {
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut hello_tx = Some(hello_tx);
        let mut hello_seen = false;
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    if let Some(sender) = hello_tx.take() {
                        let _ = sender.send(Err(format!("read stdout failed: {error}")));
                    }
                    mark_down(
                        &alive,
                        &failure,
                        &label,
                        &format!("read stdout failed: {error}"),
                    );
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            if !hello_seen {
                match HostLine::first_manifest(&line) {
                    Ok(manifest) => {
                        hello_seen = true;
                        if let Some(sender) = hello_tx.take() {
                            let _ = sender.send(Ok(manifest));
                        }
                    }
                    Err(error) => {
                        if let Some(sender) = hello_tx.take() {
                            let _ = sender.send(Err(error.clone()));
                        }
                        mark_down(&alive, &failure, &label, &error);
                        return;
                    }
                }
                continue;
            }
            match decode_line::<HostLine>(&line) {
                Ok(HostLine::Reply { id, result }) => {
                    let reply = pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&id);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(result));
                    }
                }
                Ok(HostLine::Log { level, message }) => {
                    eprintln!("process-provider[{label}] {level}: {message}");
                }
                Ok(HostLine::ProviderManifest { .. }) => {
                    eprintln!("process-provider[{label}]: duplicate manifest ignored");
                }
                Err(error) => {
                    eprintln!("process-provider[{label}]: bad stdout line: {error}");
                }
            }
        }

        mark_down(&alive, &failure, &label, "process exited");
        let waiting = std::mem::take(
            &mut *pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for (_, reply) in waiting {
            let _ = reply.send(Err(format!("{label}: process exited")));
        }
        if !hello_seen {
            if let Some(sender) = hello_tx.take() {
                let _ = sender.send(Err("process exited before manifest".to_owned()));
            }
        }
    });
}

fn spawn_writer(
    label: String,
    mut stdin: ChildStdin,
    alive: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
) -> SyncSender<Vec<u8>> {
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(WRITE_QUEUE_LINES);
    thread::spawn(move || {
        while let Ok(payload) = rx.recv() {
            if let Err(error) = stdin.write_all(&payload).and_then(|_| stdin.flush()) {
                mark_down(
                    &alive,
                    &failure,
                    &label,
                    &format!("write stdin failed: {error}"),
                );
                break;
            }
        }
    });
    tx
}

fn mark_down(alive: &AtomicBool, failure: &Mutex<Option<String>>, label: &str, reason: &str) {
    alive.store(false, Ordering::Relaxed);
    let mut failure = failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if failure.is_none() {
        *failure = Some(format!("{label}: {reason}"));
    }
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}
