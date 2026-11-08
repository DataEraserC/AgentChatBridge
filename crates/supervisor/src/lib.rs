//! ACB 进程监督器。
//!
//! 边界：systemd 只负责拉起/重启 supervisor 进程；版本切换与回滚由 supervisor
//! 决策并执行。supervisor 不依赖 NixOS generation：LKG 是一个命令（program + args），
//! 指针写在 `state_dir/lkg.json`。
//!
//! 决策循环：
//! 采样进程/健康/失败 → 构造 `Snapshot` → 调策略 → 执行 `fail / hold / rollback`。
//! 即时触发（进程退出）走内置策略，不在故障路径上等待外部策略。

// 只有 `pointer`（flock 单写者）和 `component_runtime`（进程组信号）申请 `unsafe`。
#![deny(unsafe_code)]

pub mod adopted;
pub mod component;
pub mod component_runtime;
pub mod component_state;
pub mod instance;
mod pointer;
pub mod upgrade;

pub use component::{Component, ComponentError, ComponentTable, ConfigFile, PresetRegistry};

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use acb_supervisor_policy::{
    Decision, DecisionKind, FailureRecord, PolicyConfig, Snapshot, VersionInfo, decide,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use pointer::{ActivationWriter, Projection, ReconcileOutcome, current_path, load_current};

pub const DEFAULT_PROBE_INTERVAL_SECS: u64 = 5;
pub const DEFAULT_STABLE_WINDOW_SECS: u64 = 60;
pub const DEFAULT_DECISION_TIMEOUT_MS: u64 = 1000;
pub const DEFAULT_POLICY_ERROR_FAIL_AFTER: u32 = 3;
pub const MAX_DECISION_DELAY_MS: u64 = 300_000;

/// 退出码契约。systemd unit 的 `RestartForceExitStatus` / `RestartPreventExitStatus`
/// 必须与这三个值一致；不要在各处写魔法数字。
pub const EXIT_OK: i32 = 0;
/// 需要回滚后重启：launcher 已把 `current.json` 换成 LKG，systemd 拉起后自然换版本。
pub const EXIT_ROLLBACK: i32 = 75;
/// 监督器放弃自动恢复（Fail / 回滚写指针失败 / 连续回滚失败）：systemd 不应再拉起。
pub const EXIT_GIVE_UP: i32 = 78;

/// 回滚由谁执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackMode {
    /// supervisor 自己停旧进程、起 LKG（`acb-supervisor` 独立进程用）。
    #[default]
    Internal,
    /// 写 `current.json` 后以 [`EXIT_ROLLBACK`] 退出，让 systemd 重新拉起 launcher
    /// 并读到新指针（`acb-launcher` 用）。
    External,
}

/// 稳定窗口结束后是否把当前版本提升为 LKG。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LkgUpdate {
    /// 健康满 `stable_window_secs` 就写 `lkg.json`（旧行为）。
    #[default]
    OnStable,
    /// 永不写 `lkg.json`；已有文件保持原样。测试或 canary 期间用它防止
    /// 坏版本被提升成「好版本」。
    Never,
}

/// 策略产出 `Rollback` 时是否真的回退到 LKG。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackPolicy {
    /// 按 LKG 回退（旧行为）。
    #[default]
    Lkg,
    /// 不回退：降级为放弃，不写 `current.json`，以 [`EXIT_GIVE_UP`] 退出。
    None,
}

/// 产物 generation 的引用。LKG 记录它，避免只记二进制路径而丢掉配置身份。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationRef {
    pub id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sha256: Option<String>,
}

/// 一条可执行的命令。LKG 指针保存的就是这个形状。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationRef>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            generation: None,
        }
    }

    pub fn id(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// 策略配置。`options` 原样交给策略实现；`provider` 决定用哪个实现。
#[derive(Debug, Clone, Deserialize)]
pub struct PolicySettings {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default = "default_decision_timeout")]
    pub decision_timeout_ms: u64,
    #[serde(default = "default_policy_error_fail_after")]
    pub policy_error_fail_after: u32,
    #[serde(default = "default_on_policy_error")]
    pub on_policy_error: String,
    #[serde(flatten)]
    pub options: PolicyConfig,
}

impl Default for PolicySettings {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            command: None,
            decision_timeout_ms: DEFAULT_DECISION_TIMEOUT_MS,
            policy_error_fail_after: DEFAULT_POLICY_ERROR_FAIL_AFTER,
            on_policy_error: default_on_policy_error(),
            options: PolicyConfig::default(),
        }
    }
}

fn default_provider() -> String {
    "acb.supervisor.default".to_owned()
}
fn default_decision_timeout() -> u64 {
    DEFAULT_DECISION_TIMEOUT_MS
}
fn default_policy_error_fail_after() -> u32 {
    DEFAULT_POLICY_ERROR_FAIL_AFTER
}
fn default_on_policy_error() -> String {
    "hold".to_owned()
}
fn default_probe_interval() -> u64 {
    DEFAULT_PROBE_INTERVAL_SECS
}
fn default_stable_window() -> u64 {
    DEFAULT_STABLE_WINDOW_SECS
}
fn default_state_dir() -> String {
    "state/supervisor".to_owned()
}

/// supervisor 配置文件的 `[supervisor]` 段。
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 当前版本所属的产物 generation；LKG 提升时会随 command 一起写入。
    #[serde(default)]
    pub generation: Option<GenerationRef>,
    /// 部署时已知的 LKG。状态文件里没有指针时才用它。
    #[serde(default)]
    pub lkg: Option<CommandSpec>,
    /// 桥的健康端点，如 `http://127.0.0.1:7700/healthz`。
    #[serde(default)]
    pub health_url: Option<String>,
    /// 健康探针鉴权用的 token 文件。不配置时自动取
    /// `$XDG_RUNTIME_DIR/acb-health.token`（无该变量则临时目录）；文件不存在
    /// 视为 bridge 还没写出 token，退回无鉴权探测。
    #[serde(default)]
    pub health_token_file: Option<String>,
    #[serde(default = "default_state_dir")]
    pub state_dir: String,
    #[serde(default = "default_probe_interval")]
    pub probe_interval_secs: u64,
    #[serde(default = "default_stable_window")]
    pub stable_window_secs: u64,
    /// 稳定窗口后是否提升 LKG。`never` 冻结 `lkg.json`。
    #[serde(default)]
    pub lkg_update: LkgUpdate,
    /// 策略要求回滚时是否真的回退。`none` 降级为放弃（78）。
    #[serde(default)]
    pub rollback: RollbackPolicy,
    #[serde(default)]
    pub rollback_mode: RollbackMode,
    #[serde(default)]
    pub policy: PolicySettings,
}

/// 读 `supervisor.toml` 的 `[supervisor]` 段；组件表同时解析并校验，
/// 非法组件声明会让启动失败（`[[component]]` 缺失时行为与旧版一致）。
pub fn load_config(path: &Path) -> Result<Config, String> {
    load_config_parts(path)
        .map(|(file, _components)| file.supervisor)
        .map_err(|error| error.to_string())
}

/// 读完整配置文件：`[supervisor]` + `[[component]]`，并在解析期校验组件表。
pub fn load_config_file(path: &Path) -> Result<ConfigFile, ComponentError> {
    load_config_parts(path).map(|(file, _components)| file)
}

/// 只读组件表（`acbctl component list/status` 用）；同样做完整校验。
pub fn load_components(path: &Path) -> Result<ComponentTable, ComponentError> {
    load_config_parts(path).map(|(_file, components)| components)
}

/// launcher 一次拿到 `[supervisor]` 与已校验的组件表。
pub fn load_supervisor_config(path: &Path) -> Result<(Config, ComponentTable), ComponentError> {
    load_config_parts(path).map(|(file, components)| (file.supervisor, components))
}

fn load_config_parts(path: &Path) -> Result<(ConfigFile, ComponentTable), ComponentError> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        ComponentError::new(
            "component_config_unreadable",
            format!("cannot read supervisor config {}: {error}", path.display()),
        )
    })?;
    let presets = component::load_presets_for(path)?;
    let (file, components) = component::parse_config_file(&text, &presets)?;
    if file.supervisor.program.trim().is_empty() {
        return Err(ComponentError::new(
            "supervisor_program_empty",
            "supervisor.program must not be empty",
        ));
    }
    if file.supervisor.policy.on_policy_error != "hold" {
        return Err(ComponentError::new(
            "supervisor_policy_invalid",
            "supervisor.policy.on_policy_error only supports \"hold\"",
        ));
    }
    Ok((file, components))
}

pub trait Clock {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        now_millis()
    }
}

pub trait Sleeper {
    fn sleep_ms(&mut self, millis: u64);
}

pub struct SystemSleeper;

impl Sleeper for SystemSleeper {
    fn sleep_ms(&mut self, millis: u64) {
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<String>,
}

pub trait Runner {
    fn start(&mut self, spec: &CommandSpec) -> Result<(), String>;
    fn stop(&mut self) -> Result<(), String>;
    fn running(&mut self) -> bool;
    /// 上一次观察到的退出信息；没有退出记录返回 `None`。
    fn last_exit(&mut self) -> Option<ExitInfo>;
    /// 当前被监督进程的 pid；用于写入可验证的实例记录。
    fn pid(&mut self) -> Option<u32> {
        None
    }
}

#[derive(Default)]
pub struct ProcessRunner {
    child: Option<Child>,
    last_exit: Option<ExitInfo>,
    /// 每次成功拉起后写入 `bridge.instance.json`，供 acbl 重启后接管。
    instance_file: Option<PathBuf>,
}

impl ProcessRunner {
    pub fn with_instance_file(path: impl Into<PathBuf>) -> Self {
        Self {
            child: None,
            last_exit: None,
            instance_file: Some(path.into()),
        }
    }
}

impl Runner for ProcessRunner {
    fn start(&mut self, spec: &CommandSpec) -> Result<(), String> {
        self.stop()?;
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let child = command
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", spec.program))?;
        let pid = child.id();
        self.child = Some(child);
        self.last_exit = None;
        if let Some(path) = &self.instance_file {
            instance::write_record(path, spec, pid)
                .map_err(|error| format!("cannot write instance record: {error}"))?;
        }
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        let _ = child.kill();
        let status = child
            .wait()
            .map_err(|error| format!("cannot wait for child: {error}"))?;
        self.last_exit = Some(exit_of(status));
        Ok(())
    }

    fn running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                self.last_exit = Some(exit_of(status));
                self.child = None;
                false
            }
            Ok(None) => true,
            Err(_) => false,
        }
    }

    fn last_exit(&mut self) -> Option<ExitInfo> {
        self.last_exit.clone()
    }

    fn pid(&mut self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }
}

fn exit_of(status: std::process::ExitStatus) -> ExitInfo {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        ExitInfo {
            code: status.code(),
            signal: status.signal().map(|signal| signal.to_string()),
        }
    }
    #[cfg(not(unix))]
    {
        ExitInfo {
            code: status.code(),
            signal: None,
        }
    }
}

pub trait Probe {
    fn healthy(&mut self) -> bool;
}

pub struct HttpProbe {
    url: String,
    timeout: Duration,
    /// `Some` 表示读这个文件做 Bearer 鉴权；`None` 表示无鉴权。
    token_file: Option<PathBuf>,
    /// 显式配置的 token 文件读不到时判不健康（不退回无鉴权），只提示一次。
    token_required: bool,
    warned: bool,
}

impl HttpProbe {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            timeout: Duration::from_millis(500),
            token_file: None,
            token_required: false,
            warned: false,
        }
    }

    /// 自动模式：token 文件存在就带 `Authorization: Bearer`，不存在按无鉴权端点处理。
    pub fn with_token_file(url: impl Into<String>, token_file: impl Into<PathBuf>) -> Self {
        Self {
            token_file: Some(token_file.into()),
            ..Self::new(url)
        }
    }

    /// 显式模式：token 文件缺失/不可读即判不健康并打印一次原因。不退回无鉴权，
    /// 否则鉴权配置错误会伪装成普通的 401/连接失败。
    pub fn require_token_file(url: impl Into<String>, token_file: impl Into<PathBuf>) -> Self {
        Self {
            token_required: true,
            ..Self::with_token_file(url, token_file)
        }
    }
}

impl Probe for HttpProbe {
    fn healthy(&mut self) -> bool {
        // 每次探测都重读：bridge 重启会换 token，缓存会让探针在轮换后一直 401。
        let token = match &self.token_file {
            None => None,
            Some(path) => match read_token(path) {
                Ok(token) => Some(token),
                Err(error) => {
                    if self.token_required {
                        if !self.warned {
                            eprintln!(
                                "acb-supervisor: cannot read health token file {}: {error}; \
                                 refusing to probe without authentication",
                                path.display()
                            );
                            self.warned = true;
                        }
                        return false;
                    }
                    // 自动模式：文件还没写出来（启动竞态），或端点本来不鉴权。
                    None
                }
            },
        };
        http_ok(&self.url, self.timeout, token.as_deref())
    }
}

/// 按 supervisor 配置构造探针；launcher / supervisor 两个入口共用。
pub fn build_probe(config: &Config) -> Box<dyn Probe> {
    let Some(url) = &config.health_url else {
        return Box::new(AlwaysHealthy);
    };
    match &config.health_token_file {
        Some(path) => Box::new(HttpProbe::require_token_file(url, path)),
        None => Box::new(HttpProbe::with_token_file(url, default_health_token_file())),
    }
}

/// bridge 写 token 的位置：`$XDG_RUNTIME_DIR/acb-health.token`；没有该变量
/// （Windows、精简容器）时退回 `std::env::temp_dir()`，与 bridge 侧写入逻辑一致。
pub fn default_health_token_file() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("acb-health.token")
}

fn read_token(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let token = text.trim();
    if token.is_empty() {
        return Err("file is empty".to_owned());
    }
    if token.contains('\r') || token.contains('\n') {
        return Err("token contains a line break".to_owned());
    }
    Ok(token.to_owned())
}

fn http_ok(url: &str, timeout: Duration, token: Option<&str>) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    let Ok(mut stream) = TcpStream::connect_timeout(
        &match authority.parse() {
            Ok(address) => address,
            Err(_) => return false,
        },
        timeout,
    ) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    // token 只进请求头，不写日志、不进配置文本。
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\n{auth}Connection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut head = [0u8; 64];
    let Ok(count) = stream.read(&mut head) else {
        return false;
    };
    let line = String::from_utf8_lossy(&head[..count]);
    line.starts_with("HTTP/1.") && line.contains(" 200")
}

pub trait Policy: Send + Sync {
    fn id(&self) -> &str;
    fn evaluate(&self, snapshot: &Snapshot, options: &PolicyConfig) -> Result<Decision, String>;
}

pub struct BuiltinPolicy;

impl Policy for BuiltinPolicy {
    fn id(&self) -> &str {
        "acb.supervisor.default"
    }

    fn evaluate(&self, snapshot: &Snapshot, options: &PolicyConfig) -> Result<Decision, String> {
        Ok(decide(options, snapshot))
    }
}

/// 外部策略：stdin 收 `{"snapshot":...,"options":...}`，stdout 回一份 `Decision` JSON。
/// 超时由 supervisor 侧统一执行；这里只负责一次调用。
pub struct CommandPolicy {
    program: String,
}

impl CommandPolicy {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
        }
    }
}

impl Policy for CommandPolicy {
    fn id(&self) -> &str {
        "command"
    }

    fn evaluate(&self, snapshot: &Snapshot, options: &PolicyConfig) -> Result<Decision, String> {
        let mut child = Command::new(&self.program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("cannot start policy {}: {error}", self.program))?;
        let request = json!({"snapshot": snapshot, "options": options});
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(request.to_string().as_bytes())
                .map_err(|error| format!("cannot write policy request: {error}"))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("cannot read policy output: {error}"))?;
        if !output.status.success() {
            return Err(format!("policy exited with {}", output.status));
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|error| format!("policy returned invalid decision: {error}"))
    }
}

pub struct Parts {
    pub clock: Box<dyn Clock>,
    pub runner: Box<dyn Runner>,
    pub probe: Box<dyn Probe>,
    pub sleeper: Box<dyn Sleeper>,
    pub policy: Arc<dyn Policy>,
}

pub struct Supervisor {
    config: Config,
    clock: Box<dyn Clock>,
    runner: Box<dyn Runner>,
    probe: Box<dyn Probe>,
    sleeper: Box<dyn Sleeper>,
    policy: Arc<dyn Policy>,
    builtin: BuiltinPolicy,
    current: CommandSpec,
    lkg: Option<CommandSpec>,
    startup_at_ms: i64,
    last_probe_at_ms: Option<i64>,
    ever_healthy: bool,
    healthy_since_ms: Option<i64>,
    consecutive_unhealthy: u32,
    unhealthy_since_ms: Option<i64>,
    failures: VecDeque<FailureRecord>,
    consecutive_failures: u32,
    consecutive_holds: u32,
    policy_errors: u32,
    failed: Option<String>,
    /// launcher 模式下给 systemd 看的退出码；`Some` 后 tick 不再做任何事。
    exit_code: Option<i32>,
    audit: PathBuf,
}

impl Supervisor {
    pub fn new(config: Config, parts: Parts) -> Result<Self, String> {
        let state_dir = PathBuf::from(&config.state_dir);
        std::fs::create_dir_all(&state_dir)
            .map_err(|error| format!("cannot create {}: {error}", state_dir.display()))?;
        let audit = state_dir.join("supervisor.log");
        let current = CommandSpec {
            program: config.program.clone(),
            args: config.args.clone(),
            generation: config.generation.clone(),
        };
        let mut lkg = load_lkg(&state_dir).or_else(|| config.lkg.clone());
        if lkg.as_ref() == Some(&current) {
            lkg = None;
        }
        let now = parts.clock.now_ms();
        let mut supervisor = Self {
            config,
            clock: parts.clock,
            runner: parts.runner,
            probe: parts.probe,
            sleeper: parts.sleeper,
            policy: parts.policy,
            builtin: BuiltinPolicy,
            current: current.clone(),
            lkg,
            startup_at_ms: now,
            last_probe_at_ms: None,
            ever_healthy: false,
            healthy_since_ms: None,
            consecutive_unhealthy: 0,
            unhealthy_since_ms: None,
            failures: VecDeque::new(),
            consecutive_failures: 0,
            consecutive_holds: 0,
            policy_errors: 0,
            failed: None,
            exit_code: None,
            audit,
        };
        supervisor.runner.start(&current).map_err(|error| {
            format!("cannot start current version {}: {error}", current.program)
        })?;
        supervisor.audit(json!({
            "event": "start",
            "at_ms": now,
            "current": current,
            "lkg": supervisor.lkg,
        }));
        Ok(supervisor)
    }

    pub fn current(&self) -> &CommandSpec {
        &self.current
    }

    pub fn lkg(&self) -> Option<&CommandSpec> {
        self.lkg.as_ref()
    }

    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    /// launcher 模式下的退出码；`None` = 继续监督。
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn stopped(&self) -> bool {
        self.exit_code.is_some()
    }

    /// 外部停止（手动运行 launcher 时）：停掉子进程并返回正常退出。
    pub fn shutdown(&mut self) {
        let _ = self.runner.stop();
        self.exit_code = Some(EXIT_OK);
    }

    /// 升级提交前停旧子进程。失败由调用方决定 abort 还是继续。
    pub fn stop_child(&mut self) -> Result<(), String> {
        self.runner.stop()
    }

    /// 指针未提交（stage/停旧/提交失败）时恢复旧版本进程。
    pub fn restart_current(&mut self) -> Result<(), String> {
        let spec = self.current.clone();
        self.runner.start(&spec)
    }

    /// launcher 主动退出：75 请求外部重拉，78 放弃自动恢复。
    pub fn request_exit(&mut self, code: i32) {
        self.exit_code = Some(code);
    }

    pub fn audit_path(&self) -> &Path {
        &self.audit
    }

    /// 一个采样周期。主循环按固定间隔调用。
    pub fn tick(&mut self) {
        if self.stopped() || self.failed.is_some() {
            return;
        }
        let now = self.clock.now_ms();
        if !self.runner.running() {
            let class = self
                .runner
                .last_exit()
                .map(|exit| {
                    if exit.code == Some(101) {
                        "panic".to_owned()
                    } else {
                        "exit".to_owned()
                    }
                })
                .unwrap_or_else(|| "exit".to_owned());
            self.record_failure(now, &class);
            let snapshot = self.snapshot(now);
            let decision = self
                .builtin
                .evaluate(&snapshot, &self.config.policy.options);
            if let Ok(decision) = decision {
                self.execute("process_exit", decision);
            }
            return;
        }

        let interval_ms = self.config.probe_interval_secs.max(1).saturating_mul(1000);
        let due = self
            .last_probe_at_ms
            .is_none_or(|last| now.saturating_sub(last) >= interval_ms as i64);
        if due {
            self.last_probe_at_ms = Some(now);
            let healthy = self.config.health_url.is_none() || self.probe.healthy();
            if healthy {
                self.ever_healthy = true;
                self.healthy_since_ms.get_or_insert(now);
                self.consecutive_unhealthy = 0;
                self.unhealthy_since_ms = None;
                self.consecutive_failures = 0;
                self.consecutive_holds = 0;
            } else {
                self.consecutive_unhealthy = self.consecutive_unhealthy.saturating_add(1);
                if self.unhealthy_since_ms.is_none() {
                    self.unhealthy_since_ms = Some(now);
                }
            }
            self.audit(json!({
                "event": "probe",
                "at_ms": now,
                "healthy": healthy,
                "consecutive_unhealthy": self.consecutive_unhealthy,
            }));
            self.maybe_promote(now);

            let snapshot = self.snapshot(now);
            let decision = self.evaluate_periodic(&snapshot);
            self.execute("health_probe", decision);
        }
    }

    fn evaluate_periodic(&mut self, snapshot: &Snapshot) -> Decision {
        let timeout = Duration::from_millis(self.config.policy.decision_timeout_ms.max(1));
        match evaluate_with_timeout(
            Arc::clone(&self.policy),
            snapshot.clone(),
            self.config.policy.options.clone(),
            timeout,
        ) {
            Ok(decision) if valid_decision(&decision, self.lkg.is_some()) => {
                self.policy_errors = 0;
                decision
            }
            Ok(decision) => {
                self.note_policy_error(&format!("invalid decision {:?}", decision.decision));
                self.policy_error_decision()
            }
            Err(error) => {
                self.note_policy_error(&error);
                self.policy_error_decision()
            }
        }
    }

    fn note_policy_error(&mut self, detail: &str) {
        self.policy_errors = self.policy_errors.saturating_add(1);
        self.audit(json!({
            "event": "policy_error",
            "at_ms": self.clock.now_ms(),
            "policy": self.policy.id(),
            "detail": detail,
            "consecutive": self.policy_errors,
        }));
        eprintln!(
            "supervisor policy error: {detail} (consecutive {})",
            self.policy_errors
        );
    }

    fn policy_error_decision(&self) -> Decision {
        if self.policy_errors >= self.config.policy.policy_error_fail_after.max(1) {
            Decision {
                decision: DecisionKind::Fail,
                reason_code: "policy_error_fail_after".to_owned(),
                reason: format!(
                    "policy failed {} consecutive times; stopping automatic recovery",
                    self.policy_errors
                ),
                delay_ms: None,
            }
        } else {
            Decision {
                decision: DecisionKind::Hold,
                reason_code: "policy_error".to_owned(),
                reason: "policy failed; holding current version".to_owned(),
                delay_ms: Some(backoff_ms(
                    &self.config.policy.options,
                    self.consecutive_holds,
                )),
            }
        }
    }

    fn snapshot(&self, now: i64) -> Snapshot {
        let grace_ms = self
            .config
            .policy
            .options
            .startup_grace_secs
            .saturating_mul(1000) as i64;
        Snapshot {
            observed_at_ms: now,
            startup_at_ms: Some(self.startup_at_ms),
            grace_elapsed: Some(now.saturating_sub(self.startup_at_ms) >= grace_ms),
            consecutive_failures: self.consecutive_failures,
            failures: self.failures.iter().cloned().collect(),
            consecutive_unhealthy_probes: self.consecutive_unhealthy,
            unhealthy_since_ms: self.unhealthy_since_ms,
            lkg: self.lkg.as_ref().map(|spec| VersionInfo { id: spec.id() }),
            consecutive_holds: self.consecutive_holds,
        }
    }

    fn record_failure(&mut self, now: i64, class: &str) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.failures.push_back(FailureRecord {
            at_ms: now,
            class: class.to_owned(),
        });
        while self.failures.len() > 64 {
            self.failures.pop_front();
        }
        self.audit(json!({
            "event": "failure",
            "at_ms": now,
            "class": class,
            "consecutive": self.consecutive_failures,
        }));
    }

    fn maybe_promote(&mut self, now: i64) {
        // `never` 连读取/比较都不做：已有 lkg.json 必须逐字节保持不变。
        if self.config.lkg_update == LkgUpdate::Never {
            return;
        }
        let Some(since) = self.healthy_since_ms else {
            return;
        };
        let window_ms = self.config.stable_window_secs.saturating_mul(1000) as i64;
        if now.saturating_sub(since) < window_ms {
            return;
        }
        if self.lkg.as_ref() == Some(&self.current) {
            return;
        }
        if let Err(error) = save_lkg(&PathBuf::from(&self.config.state_dir), &self.current, now) {
            self.audit(json!({"event": "lkg_write_failed", "at_ms": now, "error": error}));
            return;
        }
        self.lkg = Some(self.current.clone());
        self.audit(json!({
            "event": "lkg_promoted",
            "at_ms": now,
            "target": self.current,
        }));
        eprintln!("supervisor promoted LKG -> {}", self.current.id());
    }

    fn execute(&mut self, trigger: &str, decision: Decision) {
        let mut now = self.clock.now_ms();
        self.audit(json!({
            "event": "decision",
            "at_ms": now,
            "trigger": trigger,
            "decision": decision.decision,
            "reason_code": decision.reason_code,
            "reason": decision.reason,
            "delay_ms": decision.delay_ms,
            "current": self.current,
            "lkg": self.lkg,
        }));
        eprintln!(
            "supervisor decision trigger={trigger} decision={} reason={} ({})",
            decision.decision.as_str(),
            decision.reason_code,
            decision.reason
        );

        if decision
            .delay_ms
            .is_some_and(|delay| delay > MAX_DECISION_DELAY_MS)
        {
            self.note_policy_error("decision delay_ms exceeds supervisor cap");
            let decision = self.policy_error_decision();
            self.execute(trigger, decision);
            return;
        }
        if decision.delay_ms.unwrap_or(0) > 0 {
            self.sleeper.sleep_ms(decision.delay_ms.unwrap_or(0));
            now = self.clock.now_ms();
        }

        match decision.decision {
            DecisionKind::Hold => {
                self.consecutive_holds = self.consecutive_holds.saturating_add(1);
                if !self.runner.running() {
                    // 决策延迟期间子进程退出时，tick 开始时看到的“还在运行”已经失效。
                    // 这一次退出必须计入失败，不能只悄悄重启；否则连续失败永远不涨，
                    // 回滚阈值永远达不到。
                    let class = self
                        .runner
                        .last_exit()
                        .map(|exit| {
                            if exit.code == Some(101) {
                                "panic".to_owned()
                            } else {
                                "exit".to_owned()
                            }
                        })
                        .unwrap_or_else(|| "exit".to_owned());
                    self.record_failure(now, &class);
                    let snapshot = self.snapshot(now);
                    if let Ok(decision) = self
                        .builtin
                        .evaluate(&snapshot, &self.config.policy.options)
                        && decision.decision != DecisionKind::Hold
                    {
                        self.execute("process_exit", decision);
                        return;
                    }
                    let spec = self.current.clone();
                    match self.runner.start(&spec) {
                        Ok(()) => {
                            self.audit(json!({
                                "event": "execute",
                                "at_ms": now,
                                "decision": "hold",
                                "result": "restarted",
                                "target": spec,
                            }));
                        }
                        Err(error) => {
                            self.failed = Some(format!("restart failed: {error}"));
                            if self.config.rollback_mode == RollbackMode::External {
                                self.exit_code = Some(EXIT_GIVE_UP);
                            }
                            self.audit(json!({
                                "event": "execute",
                                "at_ms": now,
                                "decision": "hold",
                                "result": "restart_failed",
                                "error": error,
                            }));
                        }
                    }
                } else {
                    self.audit(json!({
                        "event": "execute",
                        "at_ms": now,
                        "decision": "hold",
                        "result": "kept_running",
                    }));
                }
            }
            DecisionKind::Fail => {
                self.failed = Some(decision.reason.clone());
                if self.config.rollback_mode == RollbackMode::External {
                    let _ = self.runner.stop();
                    self.exit_code = Some(EXIT_GIVE_UP);
                }
                self.audit(json!({
                    "event": "execute",
                    "at_ms": now,
                    "decision": "fail",
                    "result": "stopped_auto_recovery",
                    "reason": decision.reason,
                }));
            }
            DecisionKind::Rollback => {
                if self.config.rollback == RollbackPolicy::None {
                    // `none`：不写 current.json、不退出 75，直接按放弃处理。
                    // 先停子进程，避免 launcher 退出后留下孤儿。
                    if self.runner.running() {
                        let _ = self.runner.stop();
                    }
                    let reason = format!(
                        "{}; rollback disabled by supervisor.rollback = \"none\"",
                        decision.reason
                    );
                    self.failed = Some(reason.clone());
                    self.exit_code = Some(EXIT_GIVE_UP);
                    self.audit(json!({
                        "event": "execute",
                        "at_ms": now,
                        "decision": "rollback",
                        "result": "rollback_disabled",
                        "reason": reason,
                        "target": self.lkg,
                    }));
                    eprintln!(
                        "supervisor rollback disabled by config; giving up (exit {EXIT_GIVE_UP})"
                    );
                    return;
                }
                let Some(target) = self.lkg.clone() else {
                    self.note_policy_error("rollback without LKG");
                    let decision = self.policy_error_decision();
                    self.execute(trigger, decision);
                    return;
                };
                if self.config.rollback_mode == RollbackMode::External {
                    if self.runner.running() {
                        if let Err(error) = self.runner.stop() {
                            self.failed = Some(format!("rollback stop failed: {error}"));
                            self.exit_code = Some(EXIT_GIVE_UP);
                            self.audit(json!({
                                "event": "execute",
                                "at_ms": now,
                                "decision": "rollback",
                                "result": "stop_failed",
                                "target": target,
                                "error": error,
                            }));
                            return;
                        }
                    }
                    if let Err(error) = commit_current(
                        &PathBuf::from(&self.config.state_dir),
                        &target,
                        "acb-supervisor",
                        now,
                    ) {
                        // 写不了 current.json 就必须停下：systemd 重新拉起仍会读旧指针，
                        // 变成无限回滚循环。
                        self.failed = Some(format!("rollback pointer write failed: {error}"));
                        self.exit_code = Some(EXIT_GIVE_UP);
                        self.audit(json!({
                            "event": "execute",
                            "at_ms": now,
                            "decision": "rollback",
                            "result": "current_write_failed",
                            "target": target,
                            "error": error,
                        }));
                        return;
                    }
                    self.current = target.clone();
                    self.lkg = None;
                    self.startup_at_ms = now;
                    self.healthy_since_ms = None;
                    self.ever_healthy = false;
                    self.consecutive_unhealthy = 0;
                    self.unhealthy_since_ms = None;
                    self.consecutive_failures = 0;
                    self.consecutive_holds = 0;
                    self.failures.clear();
                    self.exit_code = Some(EXIT_ROLLBACK);
                    self.audit(json!({
                        "event": "execute",
                        "at_ms": now,
                        "decision": "rollback",
                        "result": "rollback_requested",
                        "target": self.current,
                    }));
                    eprintln!(
                        "supervisor rollback requested -> {} (exit {EXIT_ROLLBACK})",
                        self.current.id()
                    );
                    return;
                }
                if self.runner.running() {
                    if let Err(error) = self.runner.stop() {
                        self.failed = Some(format!("rollback stop failed: {error}"));
                        self.audit(json!({
                            "event": "execute",
                            "at_ms": now,
                            "decision": "rollback",
                            "result": "stop_failed",
                            "target": target,
                            "error": error,
                        }));
                        return;
                    }
                }
                match self.runner.start(&target) {
                    Ok(()) => {
                        self.current = target.clone();
                        self.lkg = None;
                        self.startup_at_ms = now;
                        self.healthy_since_ms = None;
                        self.ever_healthy = false;
                        self.consecutive_unhealthy = 0;
                        self.unhealthy_since_ms = None;
                        self.consecutive_failures = 0;
                        self.consecutive_holds = 0;
                        self.failures.clear();
                        self.audit(json!({
                            "event": "execute",
                            "at_ms": now,
                            "decision": "rollback",
                            "result": "rolled_back",
                            "target": self.current,
                        }));
                        eprintln!("supervisor rollback executed -> {}", self.current.id());
                    }
                    Err(error) => {
                        self.failed = Some(format!("rollback start failed: {error}"));
                        self.audit(json!({
                            "event": "execute",
                            "at_ms": now,
                            "decision": "rollback",
                            "result": "rollback_failed",
                            "target": target,
                            "error": error,
                        }));
                        eprintln!("supervisor rollback failed -> {}: {error}", target.id());
                    }
                }
            }
        }
    }

    fn audit(&self, record: Value) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit)
        {
            let _ = writeln!(file, "{record}");
        }
        eprintln!("supervisor audit: {record}");
    }
}

fn valid_decision(decision: &Decision, has_lkg: bool) -> bool {
    if decision
        .delay_ms
        .is_some_and(|delay| delay > MAX_DECISION_DELAY_MS)
    {
        return false;
    }
    if decision.decision == DecisionKind::Rollback && !has_lkg {
        return false;
    }
    true
}

fn backoff_ms(options: &PolicyConfig, holds: u32) -> u64 {
    let ladder = if options.restart_backoff_secs.is_empty() {
        &DEFAULT_BACKOFF[..]
    } else {
        &options.restart_backoff_secs[..]
    };
    let index = (holds as usize).min(ladder.len().saturating_sub(1));
    ladder
        .get(index)
        .copied()
        .unwrap_or(30)
        .saturating_mul(1000)
}
const DEFAULT_BACKOFF: [u64; 5] = [1, 2, 4, 8, 30];

fn evaluate_with_timeout(
    policy: Arc<dyn Policy>,
    snapshot: Snapshot,
    options: PolicyConfig,
    timeout: Duration,
) -> Result<Decision, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(policy.evaluate(&snapshot, &options));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            Err(format!("policy timed out after {}ms", timeout.as_millis()))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("policy worker disconnected".to_owned()),
    }
}

fn lkg_path(state_dir: &Path) -> PathBuf {
    state_dir.join("lkg.json")
}

/// 按统一写入器提交 `current.json`，回滚也走同一条路径（锁 + journal + 投影）。
fn commit_current(
    state_dir: &Path,
    command: &CommandSpec,
    updated_by: &str,
    now: i64,
) -> Result<(), String> {
    let writer = ActivationWriter::acquire(state_dir)?;
    let projection = command.generation.as_ref().and_then(Projection::discover);
    writer.commit(command, projection, updated_by, now)
}

fn load_lkg(state_dir: &Path) -> Option<CommandSpec> {
    let text = std::fs::read_to_string(lkg_path(state_dir)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("command")?.clone()).ok()
}

fn save_lkg(state_dir: &Path, command: &CommandSpec, now: i64) -> Result<(), String> {
    let record = json!({"command": command, "promoted_at_ms": now});
    let path = lkg_path(state_dir);
    let temp = path.with_extension("json.tmp");
    std::fs::write(
        &temp,
        serde_json::to_vec_pretty(&record).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, &path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

/// 按配置构造策略实现。`acb-supervisor` 与 `acb-launcher` 共用这一份选择逻辑。
pub fn build_policy(config: &Config) -> Result<Arc<dyn Policy>, String> {
    match config.policy.provider.as_str() {
        "acb.supervisor.default" | "default" => Ok(Arc::new(BuiltinPolicy)),
        "command" => {
            let command = config
                .policy
                .command
                .as_deref()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| "policy.provider=command requires policy.command".to_owned())?;
            Ok(Arc::new(CommandPolicy::new(command)))
        }
        other => Err(format!("unknown supervisor policy provider: {other}")),
    }
}

/// 没配 `health_url` 时的探针：进程活着就算健康，判断交给进程退出路径。
pub struct AlwaysHealthy;

impl Probe for AlwaysHealthy {
    fn healthy(&mut self) -> bool {
        true
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// 给 launcher 用的当前时间。
pub fn now_ms() -> i64 {
    now_millis()
}
