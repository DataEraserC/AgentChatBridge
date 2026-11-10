//! 外置进程工具 provider：一个子进程承载一组第三方工具。
//!
//! 生命周期：安装时 `initialize` + `list_tools` 成功才注册；之后每次调用超时或
//! 子进程退出，都标记 Unhealthy，并按指数退避重启。Unhealthy 期间调用立即失败，
//! 不排队等待。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use acb_kernel::{
    ErrorCode, Health, ProviderId, ToolCallContext, ToolError, ToolPluginConfig, ToolProvider,
    ToolSpec,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::protocol::{Decoded, decode_response, encode_request};

const CALL_QUEUE: usize = 64;
const SUPERVISOR_TICK: Duration = Duration::from_millis(50);
const CALL_GRACE: Duration = Duration::from_millis(500);

/// 重启退避：1s、2s、4s、8s、16s、30s，之后保持 30s。
#[derive(Clone, Debug)]
pub struct Backoff {
    delays: Arc<Vec<Duration>>,
}

impl Backoff {
    pub fn new(millis: Vec<u64>) -> Self {
        Self {
            delays: Arc::new(
                millis
                    .into_iter()
                    .map(Duration::from_millis)
                    .collect::<Vec<_>>(),
            ),
        }
    }

    pub fn default_policy() -> Self {
        Self::new(vec![1000, 2000, 4000, 8000, 16_000, 30_000])
    }

    fn delay(&self, attempt: usize) -> Duration {
        if self.delays.is_empty() {
            return Duration::from_secs(1);
        }
        self.delays[attempt.min(self.delays.len() - 1)]
    }
}

#[derive(Debug)]
enum CallError {
    Timeout(Duration),
    Crash(String),
    Rpc { code: i64, message: String },
    Protocol(String),
}

impl CallError {
    fn detail(&self) -> String {
        match self {
            Self::Timeout(timeout) => format!("timed out after {} ms", timeout.as_millis()),
            Self::Crash(reason) => format!("exited: {reason}"),
            Self::Rpc { code, message } => format!("rejected the call ({code}): {message}"),
            Self::Protocol(detail) => format!("protocol error: {detail}"),
        }
    }

    fn describe(&self, id: &ProviderId) -> String {
        format!("tool plugin {id} {}", self.detail())
    }

    fn into_tool_error(self, id: &ProviderId) -> ToolError {
        let code = match self {
            Self::Timeout(_) => ErrorCode::Timeout,
            _ => ErrorCode::Internal,
        };
        ToolError::new(code, self.describe(id))
    }
}

struct CallRequest {
    tool: String,
    args: Value,
    reply: SyncSender<Result<Value, CallError>>,
}

enum LineEvent {
    Line(String),
    Closed(String),
}

struct ChildProcess {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<LineEvent>,
    reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl ChildProcess {
    fn open(config: &ToolPluginConfig) -> Result<Self, String> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // 插件配置以 ACB_PLUGIN_* 注入；密钥不在其中（走密钥引用）。
        for (key, value) in &config.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("cannot spawn {}: {error}", config.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "child stdin is unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "child stdout is unavailable".to_owned())?;
        let (tx, lines) = channel();
        let reader = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(LineEvent::Closed("stdout closed".to_owned()));
                        break;
                    }
                    Ok(_) => {
                        if tx.send(LineEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(LineEvent::Closed(format!("read failed: {error}")));
                        break;
                    }
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            lines,
            reader: Some(reader),
            next_id: 0,
        })
    }

    fn call(
        &mut self,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        let line = encode_request(id, method, params);
        self.stdin
            .write_all(line.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|error| CallError::Crash(format!("write failed: {error}")))?;
        let deadline = Instant::now() + timeout;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(CallError::Timeout(timeout));
            }
            match self
                .lines
                .recv_timeout(deadline.saturating_duration_since(now))
            {
                Ok(LineEvent::Line(line)) => match decode_response(&line, id) {
                    Decoded::Result(value) => return Ok(value),
                    Decoded::RpcError { code, message } => {
                        return Err(CallError::Rpc { code, message });
                    }
                    Decoded::Ignore => continue,
                    Decoded::Malformed(detail) => return Err(CallError::Protocol(detail)),
                },
                Ok(LineEvent::Closed(reason)) => return Err(CallError::Crash(reason)),
                Err(RecvTimeoutError::Timeout) => return Err(CallError::Timeout(timeout)),
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CallError::Crash("reader stopped".to_owned()));
                }
            }
        }
    }

    fn initialize_and_list(&mut self, timeout: Duration) -> Result<Vec<ToolSpec>, String> {
        let _ = self
            .call("initialize", json!({}), timeout)
            .map_err(|error| error.detail())?;
        let reply = self
            .call("list_tools", json!({}), timeout)
            .map_err(|error| error.detail())?;
        parse_tools(reply)
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Debug, Deserialize)]
struct ToolsReply {
    #[serde(default)]
    tools: Vec<RawTool>,
}

#[derive(Debug, Deserialize)]
struct RawTool {
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default, rename = "inputSchema")]
    input_schema: Option<Value>,
}

fn parse_tools(reply: Value) -> Result<Vec<ToolSpec>, String> {
    let reply: ToolsReply = serde_json::from_value(reply)
        .map_err(|error| format!("invalid list_tools reply: {error}"))?;
    if reply.tools.is_empty() {
        return Err("list_tools returned no tools".to_owned());
    }
    let mut tools = Vec::with_capacity(reply.tools.len());
    for tool in reply.tools {
        let name = tool.name.trim().to_owned();
        if name.is_empty() {
            return Err("list_tools returned an empty tool name".to_owned());
        }
        tools.push(ToolSpec {
            name,
            summary: tool
                .summary
                .unwrap_or_else(|| "external process tool".to_owned()),
            schema: tool
                .input_schema
                .unwrap_or_else(|| json!({"type": "object"})),
        });
    }
    Ok(tools)
}

struct Inner {
    id: ProviderId,
    tools: Vec<ToolSpec>,
    config: ToolPluginConfig,
    backoff: Backoff,
    state: Mutex<Option<String>>,
    alive: Arc<AtomicBool>,
}

impl Inner {
    fn set_unhealthy(&self, reason: String) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(reason);
    }

    fn set_healthy(&self) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn unhealthy_reason(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

/// 一个受 supervisor 管理的工具子进程。
pub struct ProcessToolProvider {
    inner: Arc<Inner>,
    calls: SyncSender<CallRequest>,
    shutdown: Arc<AtomicBool>,
    supervisor: Mutex<Option<JoinHandle<()>>>,
}

impl ProcessToolProvider {
    /// 启动子进程并完成 initialize/list_tools；失败返回明确错误，不注册半套工具。
    pub fn spawn(config: ToolPluginConfig) -> Result<Self, String> {
        Self::spawn_with_backoff(config, Backoff::default_policy())
    }

    pub fn spawn_with_backoff(config: ToolPluginConfig, backoff: Backoff) -> Result<Self, String> {
        let id = ProviderId::parse(&config.id)
            .map_err(|error| format!("invalid tool plugin id {}: {error}", config.id))?;
        let timeout = Duration::from_millis(config.timeout());
        let mut child = ChildProcess::open(&config)?;
        let tools = match child.initialize_and_list(timeout) {
            Ok(tools) => tools,
            Err(error) => {
                child.kill();
                return Err(error);
            }
        };
        let state = Mutex::new(None);
        let alive = Arc::new(AtomicBool::new(true));
        let inner = Arc::new(Inner {
            id,
            tools,
            config,
            backoff,
            state,
            alive: Arc::clone(&alive),
        });
        let (calls, receiver) = sync_channel(CALL_QUEUE);
        let shutdown = Arc::new(AtomicBool::new(false));
        let supervisor = {
            let inner = Arc::clone(&inner);
            let shutdown = Arc::clone(&shutdown);
            let alive = Arc::clone(&alive);
            thread::spawn(move || supervisor_loop(inner, receiver, shutdown, alive, child))
        };
        Ok(Self {
            inner,
            calls,
            shutdown,
            supervisor: Mutex::new(Some(supervisor)),
        })
    }

    pub fn id(&self) -> &ProviderId {
        &self.inner.id
    }

    /// 子进程当前是否活着（测试/诊断用）。
    pub fn is_alive(&self) -> bool {
        self.inner.alive.load(Ordering::SeqCst)
    }

    /// 停掉 supervisor 与子进程。幂等。
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self
            .supervisor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for ProcessToolProvider {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl ToolProvider for ProcessToolProvider {
    fn tools(&self) -> Vec<ToolSpec> {
        self.inner.tools.clone()
    }

    fn call(
        &self,
        _context: &ToolCallContext<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        if let Some(reason) = self.inner.unhealthy_reason() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!("tool plugin {} unavailable: {reason}", self.inner.id),
            ));
        }
        let (tx, rx) = sync_channel(1);
        let request = CallRequest {
            tool: tool.to_owned(),
            args,
            reply: tx,
        };
        match self.calls.try_send(request) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(ToolError::new(
                    ErrorCode::Timeout,
                    format!("tool plugin {} call queue is full", self.inner.id),
                ));
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(ToolError::new(
                    ErrorCode::Internal,
                    format!("tool plugin {} supervisor stopped", self.inner.id),
                ));
            }
        }
        let timeout = Duration::from_millis(self.inner.config.timeout()) + CALL_GRACE;
        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(unwrap_tool_result(value)),
            Ok(Err(error)) => Err(error.into_tool_error(&self.inner.id)),
            Err(_) => Err(ToolError::new(
                ErrorCode::Timeout,
                format!("tool plugin {} did not reply in time", self.inner.id),
            )),
        }
    }

    fn health(&self) -> Health {
        match self.inner.unhealthy_reason() {
            Some(reason) => Health::Unhealthy(reason),
            None => Health::Healthy,
        }
    }
}

/// 协议约定 `call_tool` 的结果放在 `{"result": ...}`；调用方只关心里面的值。
fn unwrap_tool_result(value: Value) -> Value {
    match value {
        Value::Object(mut fields) => fields.remove("result").unwrap_or(Value::Object(fields)),
        other => other,
    }
}

fn supervisor_loop(
    inner: Arc<Inner>,
    calls: Receiver<CallRequest>,
    shutdown: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    child: ChildProcess,
) {
    let mut child = Some(child);
    let mut attempt = 0usize;
    let mut next_retry: Option<Instant> = None;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            if let Some(mut current) = child.take() {
                current.kill();
            }
            alive.store(false, Ordering::SeqCst);
            inner.set_healthy();
            break;
        }

        if child.is_none() && next_retry.is_some_and(|at| Instant::now() >= at) {
            match open_and_verify(&inner) {
                Ok(candidate) => {
                    child = Some(candidate);
                    alive.store(true, Ordering::SeqCst);
                    inner.set_healthy();
                    attempt = 0;
                    next_retry = None;
                    eprintln!("process-tools: {} restarted", inner.id);
                }
                Err(error) => {
                    inner.set_unhealthy(error.clone());
                    attempt = attempt.saturating_add(1);
                    let delay = inner.backoff.delay(attempt);
                    next_retry = Some(Instant::now() + delay);
                    eprintln!(
                        "process-tools: {} restart failed: {error}; retrying in {} ms",
                        inner.id,
                        delay.as_millis()
                    );
                }
            }
        }

        let request = match calls.recv_timeout(SUPERVISOR_TICK) {
            Ok(request) => request,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                if let Some(mut current) = child.take() {
                    current.kill();
                }
                alive.store(false, Ordering::SeqCst);
                break;
            }
        };

        let Some(current) = child.as_mut() else {
            let reason = inner
                .unhealthy_reason()
                .unwrap_or_else(|| "restarting".to_owned());
            let _ = request.reply.send(Err(CallError::Crash(reason)));
            continue;
        };
        let timeout = Duration::from_millis(inner.config.timeout());
        let result = current.call(
            "call_tool",
            json!({"name": request.tool, "arguments": request.args}),
            timeout,
        );
        match result {
            Ok(value) => {
                let _ = request.reply.send(Ok(value));
            }
            Err(error) => {
                let message = error.describe(&inner.id);
                if let Some(mut dead) = child.take() {
                    dead.kill();
                }
                inner.set_unhealthy(message.clone());
                alive.store(false, Ordering::SeqCst);
                attempt = attempt.saturating_add(1);
                let delay = inner.backoff.delay(attempt);
                next_retry = Some(Instant::now() + delay);
                eprintln!(
                    "process-tools: {} marked unhealthy: {message}; retrying in {} ms",
                    inner.id,
                    delay.as_millis()
                );
                let _ = request.reply.send(Err(error));
            }
        }
    }
}

/// 重启时重新 spawn，并校验工具集合与安装时一致。
fn open_and_verify(inner: &Inner) -> Result<ChildProcess, String> {
    let mut candidate = ChildProcess::open(&inner.config)?;
    let timeout = Duration::from_millis(inner.config.timeout());
    let tools = match candidate.initialize_and_list(timeout) {
        Ok(tools) => tools,
        Err(error) => {
            candidate.kill();
            return Err(error);
        }
    };
    let current: Vec<String> = inner.tools.iter().map(|tool| tool.name.clone()).collect();
    let found: Vec<String> = tools.iter().map(|tool| tool.name.clone()).collect();
    if current != found {
        candidate.kill();
        return Err("tool list changed after restart".to_owned());
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_reply() {
        let reply = json!({
            "tools": [
                {"name": "echo", "summary": "echo", "inputSchema": {"type": "object"}}
            ]
        });
        let tools = parse_tools(reply).expect("合法回复");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[test]
    fn empty_tool_list_is_rejected() {
        assert!(parse_tools(json!({"tools": []})).is_err());
    }

    #[test]
    fn backoff_caps_at_last_delay() {
        let backoff = Backoff::new(vec![10, 20]);
        assert_eq!(backoff.delay(0), Duration::from_millis(10));
        assert_eq!(backoff.delay(1), Duration::from_millis(20));
        assert_eq!(backoff.delay(99), Duration::from_millis(20));
    }
}
