//! `[[component]]` 组件表的解析与校验（P1 契约的 P0 范围）。
//!
//! 边界：只解析配置、校验声明关系，**不 spawn 任何进程**、不写
//! `state/components/`、不参与 bridge 健康判定与 LKG。没有 `[[component]]`
//! 时行为与今天完全一致。
//!
//! 所有清单结构都 `deny_unknown_fields`：写错字段会拒绝启动，而不是静默忽略。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Config;

/// 组件探针默认间隔（契约 §5.3：5s × 5）。
pub const DEFAULT_COMPONENT_PROBE_INTERVAL_SECS: u64 = 5;
/// 组件探针默认失败阈值（契约 §5.3：5s × 5）。
pub const DEFAULT_COMPONENT_FAILURES_THRESHOLD: u32 = 5;
/// 组件探针默认成功阈值（契约 §5.3）。
pub const DEFAULT_COMPONENT_SUCCESS_THRESHOLD: u32 = 3;
/// 组件探针默认启动宽限（契约 §5.3）。
pub const DEFAULT_COMPONENT_STARTUP_GRACE_SECS: u64 = 60;
/// 组件默认停止超时（契约 §5.2 / §5.4）。
pub const DEFAULT_COMPONENT_STOP_TIMEOUT_SECS: u64 = 45;

/// 定位预设清单的环境变量；未设置时取 supervisor.toml 同目录的
/// `presets.toml`，两者都没有就视为空预设集合。
pub const PRESETS_FILE_ENV: &str = "ACB_PRESETS_FILE";

/// `supervisor.toml` 的完整内容：`[supervisor]` + 可选的 `[[component]]`。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    pub supervisor: Config,
    /// TOML 顶层数组表键是单数 `component`。
    #[serde(default, rename = "component")]
    pub components: Vec<Component>,
}

/// 组件类型。`notify` 是契约预留值，v1 不实现，因此不进入枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentType {
    Simple,
    Oneshot,
}

impl Default for ComponentType {
    fn default() -> Self {
        Self::Simple
    }
}

impl fmt::Display for ComponentType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Simple => "simple",
            Self::Oneshot => "oneshot",
        })
    }
}

/// 预设来源词表（契约 §5.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresetSource {
    Nix,
    Npm,
    Guix,
    Scoop,
    Homebrew,
    Distro,
    Container,
    Acb,
}

impl fmt::Display for PresetSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Nix => "nix",
            Self::Npm => "npm",
            Self::Guix => "guix",
            Self::Scoop => "scoop",
            Self::Homebrew => "homebrew",
            Self::Distro => "distro",
            Self::Container => "container",
            Self::Acb => "acb",
        })
    }
}

/// 组件来源：预设来源，或自定义 `command` 的 `custom`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentSource {
    Preset(PresetSource),
    Custom,
}

impl fmt::Display for ComponentSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preset(source) => source.fmt(formatter),
            Self::Custom => formatter.write_str("custom"),
        }
    }
}

/// 探针类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessKind {
    Tcp,
    Http,
    Process,
    /// `oneshot` 专用：只看退出码，零即完成、非零按 `restart` 策略处理。
    Exit,
    /// 问 bridge「这个 harness 现在有没有连着」。
    ///
    /// 为什么需要它：`tcp` 只看端口在不在监听。opencode 那种「进程活着、端口在听、
    /// 但插件已经卸载」的状态下，tcp 探针恒为健康，supervisor 永远不会重启它
    /// （2026-11-18 实测：opencode 掉线 7 小时无人发现，`openConnections = 0`）。
    ///
    /// 判据用 bridge 依赖表的 `openConnections`：它是**应用层**事实，不是端口。
    Bridge,
}

impl fmt::Display for ReadinessKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Tcp => "tcp",
            Self::Http => "http",
            Self::Process => "process",
            Self::Exit => "exit",
            Self::Bridge => "bridge",
        })
    }
}

/// 重启策略（契约 §5.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    Never,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::OnFailure
    }
}

impl fmt::Display for RestartPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Always => "always",
            Self::OnFailure => "on-failure",
            Self::Never => "never",
        })
    }
}

/// 停止时如何收尾组件进程组。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillMode {
    Process,
    Group,
}

impl Default for KillMode {
    fn default() -> Self {
        Self::Group
    }
}

impl fmt::Display for KillMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Process => "process",
            Self::Group => "group",
        })
    }
}

/// `oneshot` 组件的停止语义（契约 §5.10.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopAction {
    Down,
    LeaveRunning,
}

impl Default for StopAction {
    fn default() -> Self {
        Self::LeaveRunning
    }
}

impl fmt::Display for StopAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Down => "down",
            Self::LeaveRunning => "leave_running",
        })
    }
}

/// 日志去向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogTarget {
    File,
    Journald,
}

impl Default for LogTarget {
    fn default() -> Self {
        Self::File
    }
}

impl fmt::Display for LogTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::File => "file",
            Self::Journald => "journald",
        })
    }
}

/// `[component.process]`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env_file: Option<String>,
    #[serde(default)]
    pub kill_mode: KillMode,
}

/// `[component.readiness]`（原始声明；`kind` 在解析期才按 type 定默认）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    #[serde(default)]
    pub kind: Option<ReadinessKind>,
    #[serde(default = "default_probe_host")]
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default = "default_probe_path")]
    pub path: String,
    #[serde(default = "default_probe_interval")]
    pub interval_secs: u64,
    /// `kind = "bridge"` 时要查哪个 harness（bridge 依赖表里的 id）。
    #[serde(default)]
    pub harness: Option<String>,
    /// `kind = "bridge"` 时 bridge 的 console 地址与 token 环境变量名。
    ///
    /// token 只从**环境变量**读，不写进配置文件：组件声明文件是可提交的，
    /// 放进去就等于把控制台凭据写进版本库。
    #[serde(default = "default_bridge_url")]
    pub bridge_url: String,
    #[serde(default = "default_bridge_token_env")]
    pub bridge_token_env: String,
    #[serde(default = "default_failures_threshold")]
    pub failures_threshold: u32,
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,
    #[serde(default = "default_startup_grace")]
    pub startup_grace_secs: u64,
}

impl Default for Readiness {
    fn default() -> Self {
        Self {
            kind: None,
            host: default_probe_host(),
            port: None,
            path: default_probe_path(),
            interval_secs: default_probe_interval(),
            harness: None,
            bridge_url: default_bridge_url(),
            bridge_token_env: default_bridge_token_env(),
            failures_threshold: default_failures_threshold(),
            success_threshold: default_success_threshold(),
            startup_grace_secs: default_startup_grace(),
        }
    }
}

/// `[component.restart]`（契约 §5.3：默认 `on-failure` 有界重试）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restart {
    #[serde(default)]
    pub policy: RestartPolicy,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: Vec<u64>,
    #[serde(default = "default_backoff_cap_secs")]
    pub backoff_cap_secs: u64,
}

impl Default for Restart {
    fn default() -> Self {
        Self {
            policy: RestartPolicy::default(),
            max_attempts: default_max_attempts(),
            backoff_secs: default_backoff_secs(),
            backoff_cap_secs: default_backoff_cap_secs(),
        }
    }
}

/// `[component.depends]`（契约 §5.4）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Depends {
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub wants: Vec<String>,
    #[serde(default)]
    pub part_of: Vec<String>,
}

/// `[component.log]`（契约 §5.6）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    #[serde(default)]
    pub target: LogTarget,
    #[serde(default)]
    pub path: Option<String>,
}

/// 一条 `[[component]]` 声明。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    pub name: String,
    /// 省略时继承预设的 `default_enabled`，自定义组件默认 false。
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(rename = "type", default)]
    pub kind: Option<ComponentType>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 仅 `type = "oneshot"`：compose 文件绝对路径；空 = 不启用。
    #[serde(default)]
    pub file: Option<String>,
    /// 仅 `type = "oneshot"`：目标机实际可用的 compose 工具。
    #[serde(default)]
    pub compose_bin: Vec<String>,
    /// 仅 `type = "oneshot"`：每次拉起都重建容器（`--force-recreate`），卷保留。
    ///
    /// 默认 false：容器配置没变时 `up -d` 是幂等的，没必要每次换一个新的。
    /// 对「镜像是 latest、要跟上上游」或「配置改了但 compose 认为没变」的组件
    /// 才需要打开——重建会丢容器内的临时状态，但具名卷不受影响。
    #[serde(default)]
    pub compose_recreate: bool,
    #[serde(default)]
    pub stop_action: StopAction,
    #[serde(default)]
    pub process: Option<Process>,
    #[serde(default)]
    pub readiness: Option<Readiness>,
    #[serde(default)]
    pub restart: Option<Restart>,
    #[serde(default)]
    pub depends: Depends,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub stop_timeout_secs: Option<u64>,
}

/// `presets.toml`。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetFile {
    #[serde(default = "default_preset_schema")]
    pub schema: u64,
    #[serde(default, rename = "preset")]
    pub presets: Vec<Preset>,
}

/// `presets.toml` 的一条预设条目（契约 §5.2）。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    pub id: String,
    pub component: String,
    #[serde(rename = "type")]
    pub kind: ComponentType,
    pub source: PresetSource,
    pub script: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default_enabled: bool,
    #[serde(default)]
    pub process: Option<Process>,
    #[serde(default)]
    pub readiness: Option<Readiness>,
    #[serde(default)]
    pub restart: Option<Restart>,
    /// 该预设的组件默认排在哪些组件之后；与 `[component.depends].after` 取并集。
    #[serde(default)]
    pub after: Vec<String>,
}

/// 已加载的预设集合。
#[derive(Debug, Clone, Default)]
pub struct PresetRegistry {
    presets: Vec<Preset>,
    /// 预设清单所在目录；脚本路径相对它解析。
    root: Option<PathBuf>,
}

impl PresetRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_toml(text: &str) -> Result<Self, ComponentError> {
        let file: PresetFile = toml::from_str(text).map_err(map_toml_error)?;
        Ok(Self {
            presets: file.presets,
            root: None,
        })
    }

    pub fn load(path: &Path) -> Result<Self, ComponentError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            ComponentError::new(
                "component_preset_file_invalid",
                format!("cannot read preset manifest {}: {error}", path.display()),
            )
        })?;
        let mut registry = Self::from_toml(&text)?;
        registry.root = path.parent().map(Path::to_path_buf);
        Ok(registry)
    }

    pub fn get(&self, id: &str) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Preset> {
        self.presets.iter()
    }

    /// 预设脚本的真实路径。清单里写相对路径时相对清单目录解析。
    pub fn script_path(&self, script: &str) -> PathBuf {
        let path = Path::new(script);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root
                .as_deref()
                .map(|root| root.join(path))
                .unwrap_or_else(|| path.to_path_buf())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.presets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.presets.len()
    }
}

/// 载入 supervisor.toml 时配套的预设集合，按顺序取第一个可用的来源：
///
/// 1. `ACB_PRESETS_FILE`（显式覆盖）；
/// 2. 配置文件同目录的 `presets.toml`（自管配置的覆盖层）；
/// 3. 当前可执行文件同级推导的 `../share/acb/presets/presets.toml`
///    （acbl 安装包自带的默认清单，`acb-launcher` 与 `acbctl` 都适用）；
/// 4. 空集合（引用预设会报 `component_preset_unknown`）。
pub fn load_presets_for(config_path: &Path) -> Result<PresetRegistry, ComponentError> {
    if let Some(path) = std::env::var_os(PRESETS_FILE_ENV).filter(|value| !value.is_empty()) {
        return PresetRegistry::load(Path::new(&path));
    }
    let sibling = sibling_presets_path(config_path);
    if sibling.exists() {
        return PresetRegistry::load(&sibling);
    }
    if let Some(path) = installed_presets_path() {
        return PresetRegistry::load(&path);
    }
    Ok(PresetRegistry::empty())
}

fn sibling_presets_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("presets.toml")
}

/// acbl 包内预设清单相对安装前缀的位置。
const INSTALLED_PRESETS_REL: &str = "share/acb/presets/presets.toml";

fn installed_presets_path() -> Option<PathBuf> {
    installed_presets_path_from(&std::env::current_exe().ok()?)
}

/// 从可执行文件路径推导同包预设清单。先 `canonicalize` 解析软链：
/// `/run/current-system/sw/bin/acbctl` 这类路径的父目录是符号链接目录，
/// 不解析就会推导到 `/run/current-system/sw/share/...`。解析后
/// `<prefix>/bin/<exe>` → `<prefix>/share/acb/presets/presets.toml`。
fn installed_presets_path_from(exe: &Path) -> Option<PathBuf> {
    let exe = std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf());
    let prefix = exe.parent()?.parent()?;
    let path = prefix.join(INSTALLED_PRESETS_REL);
    path.is_file().then_some(path)
}

/// 解析期确定下来的探针。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedReadiness {
    pub kind: ReadinessKind,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub interval_secs: u64,
    /// `kind = "bridge"` 时要查的 harness id。
    pub harness: Option<String>,
    pub bridge_url: String,
    pub bridge_token_env: String,
    pub failures_threshold: u32,
    pub success_threshold: u32,
    pub startup_grace_secs: u64,
}

impl ResolvedReadiness {
    pub fn endpoint(&self) -> String {
        match self.kind {
            ReadinessKind::Tcp | ReadinessKind::Http => {
                format!("{}:{}", self.host, self.port.unwrap_or_default())
            }
            ReadinessKind::Process => "process".to_owned(),
            ReadinessKind::Exit => "exit".to_owned(),
            ReadinessKind::Bridge => format!(
                "bridge:{}",
                self.harness.as_deref().unwrap_or("(未配置 harness)")
            ),
        }
    }
}

/// 校验通过的组件声明（只读视图；进程态在 P1 一律为未运行）。
#[derive(Debug, Clone)]
pub struct ResolvedComponent {
    pub name: String,
    pub entry_index: usize,
    pub kind: ComponentType,
    pub source: ComponentSource,
    pub preset: Option<String>,
    pub enabled: bool,
    pub command: Option<Vec<String>>,
    /// 预设脚本解析后的路径；自定义 command 组件为 None。
    pub script: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub file: Option<String>,
    pub compose_bin: Vec<String>,
    pub compose_recreate: bool,
    pub stop_action: StopAction,
    pub process: Process,
    pub readiness: ResolvedReadiness,
    pub restart: Restart,
    pub depends: Depends,
    pub log: LogConfig,
    pub stop_timeout_secs: u64,
}

/// 校验通过的组件表。
#[derive(Debug, Clone, Default)]
pub struct ComponentTable {
    pub components: Vec<ResolvedComponent>,
}

impl ComponentTable {
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }

    pub fn get(&self, name: &str) -> impl Iterator<Item = &ResolvedComponent> {
        self.components
            .iter()
            .filter(move |component| component.name == name)
    }
}

/// 稳定的原因码 + 可读消息。启动 / 重载 / `acbctl component` 共用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentError {
    code: &'static str,
    message: String,
}

impl ComponentError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ComponentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// 把组件里声明的路径解析成绝对路径。
///
/// **相对路径按配置文件的父目录解析**（不是进程 cwd）：配置和它引用的脚本
/// 通常在一起，按 cwd 解析会让「从哪个目录启动」变成隐含依赖——测试里能过、
/// 线上换个启动目录就找不到文件。
///
/// 绝对路径原样返回：不强行改写用户明确写下的东西。
///
/// 为什么需要这个：`supervisor.toml` 是要能跟着仓库走的（不绑 Nix、不绑
/// systemd 部署），写死 `/home/<user>/...` 的绝对路径一换位置就失效。
fn resolve_path(base: &Path, declared: &str) -> String {
    if declared.is_empty() {
        return String::new();
    }
    let path = Path::new(declared);
    if path.is_absolute() {
        return declared.to_owned();
    }
    base.join(path).to_string_lossy().into_owned()
}

/// 解析 `supervisor.toml` 并校验组件表。
pub fn parse_config_file(
    text: &str,
    presets: &PresetRegistry,
    base: &Path,
) -> Result<(ConfigFile, ComponentTable), ComponentError> {
    let file: ConfigFile = toml::from_str(text).map_err(map_toml_error)?;
    let table = resolve_components_at(&file.components, presets, base)?;
    Ok((file, table))
}

/// 只做组件表校验（不读文件）。
pub fn resolve_components(
    components: &[Component],
    presets: &PresetRegistry,
) -> Result<ComponentTable, ComponentError> {
    // 没有配置文件上下文时按进程 cwd 解析：这是 `resolve_components` 的老行为，
    // 保留给「只做校验、不读文件」的调用方（测试与 CLI 检查）。
    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_components_at(components, presets, &base)
}

/// 同 [`resolve_components`]，但相对路径按 `base` 解析（通常是配置文件的父目录）。
pub fn resolve_components_at(
    components: &[Component],
    presets: &PresetRegistry,
    base: &Path,
) -> Result<ComponentTable, ComponentError> {
    let mut resolved = Vec::with_capacity(components.len());
    for (index, component) in components.iter().enumerate() {
        resolved.push(resolve_component(index, component, presets, base)?);
    }

    check_duplicates_and_sources(&resolved)?;
    check_dependencies(&resolved)?;
    check_cycles(&resolved)?;

    Ok(ComponentTable {
        components: resolved,
    })
}

fn resolve_component(
    index: usize,
    component: &Component,
    presets: &PresetRegistry,
    base: &Path,
) -> Result<ResolvedComponent, ComponentError> {
    validate_name(index, &component.name)?;

    if component.preset.is_some() && component.command.is_some() {
        return Err(field_error(
            index,
            "preset",
            "component_multiple_sources",
            "preset 与 command 只能二选一",
        ));
    }
    let preset = match &component.preset {
        Some(id) => Some(presets.get(id).ok_or_else(|| {
            field_error(
                index,
                "preset",
                "component_preset_unknown",
                format!("预设 `{id}` 未在 presets.toml 中登记"),
            )
        })?),
        None => None,
    };
    if component.preset.is_none() && component.command.is_none() {
        return Err(field_error(
            index,
            "preset",
            "component_missing_preset_or_command",
            "组件必须声明 preset 或 command",
        ));
    }
    if let Some(command) = &component.command {
        if command.is_empty() || command[0].trim().is_empty() {
            return Err(field_error(
                index,
                "command",
                "component_missing_preset_or_command",
                "command 不能为空",
            ));
        }
    }
    if let Some(preset) = preset {
        if preset.component != component.name {
            return Err(field_error(
                index,
                "preset",
                "component_preset_component_mismatch",
                format!(
                    "预设 `{}` 属于组件 `{}`，不是 `{}`",
                    preset.id, preset.component, component.name
                ),
            ));
        }
        if let Some(kind) = component.kind {
            if kind != preset.kind {
                return Err(field_error(
                    index,
                    "type",
                    "component_preset_component_mismatch",
                    format!(
                        "显式 type = `{kind}` 与预设 `{}` 的 `{}` 不一致",
                        preset.id, preset.kind
                    ),
                ));
            }
        }
    }

    let kind = component
        .kind
        .or_else(|| preset.map(|preset| preset.kind))
        .unwrap_or_default();

    let mut enabled = component
        .enabled
        .unwrap_or_else(|| preset.map(|preset| preset.default_enabled).unwrap_or(false));
    let readiness = resolve_readiness(
        index,
        kind,
        component.readiness.as_ref(),
        preset.and_then(|preset| preset.readiness.as_ref()),
    )?;

    // compose 驱动的 oneshot 必须声明 file 与 compose_bin；`kind = "exit"` 的
    // oneshot 跑的是脚本、只看退出码（如 dsh-presets），不需要 compose。
    let compose_driven = kind == ComponentType::Oneshot
        && readiness.kind != ReadinessKind::Exit
        && (component.command.is_some()
            || !component.compose_bin.is_empty()
            || preset.is_some_and(|preset| preset.source == PresetSource::Container));
    if compose_driven && component.file.as_deref().unwrap_or("").trim().is_empty() {
        if enabled {
            return Err(field_error(
                index,
                "file",
                "component_oneshot_file_missing",
                "oneshot 组件启用时 file 不能为空",
            ));
        }
        // 契约 §5.1：compose 驱动的 oneshot 且 file 为空一律不启用。
        enabled = false;
    }
    if compose_driven
        && enabled
        && (component.compose_bin.is_empty() || component.compose_bin[0].trim().is_empty())
    {
        return Err(field_error(
            index,
            "compose_bin",
            "component_oneshot_compose_bin_missing",
            "oneshot 组件启用时必须声明 compose_bin",
        ));
    }

    let restart = component
        .restart
        .clone()
        .or_else(|| preset.and_then(|preset| preset.restart.clone()))
        .unwrap_or_default();
    let mut process = component
        .process
        .clone()
        .or_else(|| preset.and_then(|preset| preset.process.clone()))
        .unwrap_or_default();
    // `cwd` 与 `env_file` 同样按配置文件目录解析：配置可跟着仓库走。
    if let Some(cwd) = process.cwd.take() {
        let resolved = resolve_path(base, &cwd);
        process.cwd = (!resolved.is_empty()).then_some(resolved);
    }
    if let Some(env_file) = process.env_file.take() {
        let resolved = resolve_path(base, &env_file);
        process.env_file = (!resolved.is_empty()).then_some(resolved);
    }
    let source = preset
        .map(|preset| ComponentSource::Preset(preset.source))
        .unwrap_or(ComponentSource::Custom);
    let script = preset.map(|preset| {
        presets
            .script_path(&preset.script)
            .to_string_lossy()
            .into_owned()
    });

    // 预设可以携带 `after`，与组件自身声明的 `after` 合并（去重、保留声明顺序）。
    let mut depends = component.depends.clone();
    if let Some(preset) = preset {
        for name in &preset.after {
            if !depends.after.iter().any(|existing| existing == name) {
                depends.after.push(name.clone());
            }
        }
    }

    Ok(ResolvedComponent {
        name: component.name.clone(),
        entry_index: index,
        kind,
        source,
        preset: component.preset.clone(),
        enabled,
        command: component.command.clone(),
        script,
        args: component.args.clone(),
        env: component.env.clone(),
        // 相对路径按配置文件目录解析，配置可跟着仓库走。
        file: component
            .file
            .as_deref()
            .map(|path| resolve_path(base, path))
            .filter(|path| !path.is_empty()),
        compose_bin: component.compose_bin.clone(),
        compose_recreate: component.compose_recreate,
        stop_action: component.stop_action,
        process,
        readiness,
        restart,
        depends,
        log: component.log.clone(),
        stop_timeout_secs: component
            .stop_timeout_secs
            .unwrap_or(DEFAULT_COMPONENT_STOP_TIMEOUT_SECS),
    })
}

fn resolve_readiness(
    index: usize,
    kind: ComponentType,
    declared: Option<&Readiness>,
    preset: Option<&Readiness>,
) -> Result<ResolvedReadiness, ComponentError> {
    // oneshot 的探针必须显式声明；唯一例外是预设显式声明了 `kind = "exit"`
    // （dsh-presets 这种「跑一次就结束」的脚本预设）。
    let inherited = match kind {
        ComponentType::Oneshot => {
            preset.filter(|readiness| readiness.kind == Some(ReadinessKind::Exit))
        }
        ComponentType::Simple => preset,
    };
    let raw = declared.or(inherited).cloned().unwrap_or_default();
    let resolved_kind = match declared
        .and_then(|readiness| readiness.kind)
        .or_else(|| inherited.and_then(|readiness| readiness.kind))
    {
        Some(kind) => kind,
        None if kind == ComponentType::Oneshot => {
            return Err(field_error(
                index,
                "readiness.kind",
                "component_readiness_required",
                "type = \"oneshot\" 必须显式声明探针：exit、tcp 或 http",
            ));
        }
        None => ReadinessKind::Process,
    };
    if resolved_kind == ReadinessKind::Exit && kind != ComponentType::Oneshot {
        return Err(field_error(
            index,
            "readiness.kind",
            "component_readiness_invalid",
            "kind = \"exit\" 只能用于 type = \"oneshot\"",
        ));
    }
    if kind == ComponentType::Oneshot && resolved_kind == ReadinessKind::Process {
        return Err(field_error(
            index,
            "readiness.kind",
            "component_readiness_required",
            "type = \"oneshot\" 的探针不能是 process（用 exit 或 tcp/http）",
        ));
    }

    if matches!(resolved_kind, ReadinessKind::Tcp | ReadinessKind::Http) && raw.port.is_none() {
        return Err(field_error(
            index,
            "readiness.port",
            "component_readiness_invalid",
            format!("kind = `{resolved_kind}` 必须声明 port"),
        ));
    }
    // `bridge` 探针必须知道问哪个 harness；没有它探针无从判断。
    if resolved_kind == ReadinessKind::Bridge && raw.harness.as_deref().unwrap_or("").is_empty() {
        return Err(field_error(
            index,
            "readiness.harness",
            "component_readiness_invalid",
            "kind = \"bridge\" 必须声明 harness（要查哪个 harness 的连接）",
        ));
    }

    Ok(ResolvedReadiness {
        kind: resolved_kind,
        host: raw.host,
        port: raw.port,
        path: raw.path,
        interval_secs: raw.interval_secs,
        harness: raw.harness,
        bridge_url: raw.bridge_url,
        bridge_token_env: raw.bridge_token_env,
        failures_threshold: raw.failures_threshold,
        success_threshold: raw.success_threshold,
        startup_grace_secs: raw.startup_grace_secs,
    })
}

fn validate_name(index: usize, name: &str) -> Result<(), ComponentError> {
    if name == "bridge" {
        return Err(field_error(
            index,
            "name",
            "component_reserved_name",
            "`bridge` 保留给 [supervisor]，不能作为组件名",
        ));
    }
    if !valid_component_name(name) {
        return Err(field_error(
            index,
            "name",
            "component_name_invalid",
            format!("组件名 `{name}` 不匹配 [a-z][a-z0-9-]*"),
        ));
    }
    Ok(())
}

fn valid_component_name(name: &str) -> bool {
    let mut chars = name.chars();
    if !matches!(chars.next(), Some(first) if first.is_ascii_lowercase()) {
        return false;
    }
    chars.all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
    })
}

fn check_duplicates_and_sources(components: &[ResolvedComponent]) -> Result<(), ComponentError> {
    let mut by_name: HashMap<&str, Vec<&ResolvedComponent>> = HashMap::new();
    for component in components {
        by_name
            .entry(component.name.as_str())
            .or_default()
            .push(component);
    }

    for (name, entries) in by_name {
        let mut enabled = entries.iter().filter(|entry| entry.enabled);
        if enabled.clone().count() > 1 {
            let first = enabled.next().expect("count > 1");
            let second = enabled.next().expect("count > 1");
            return Err(field_error(
                second.entry_index,
                "enabled",
                "component_multiple_sources",
                format!(
                    "组件 `{name}` 同时启用了多个来源（entries {} 与 {}）",
                    first.entry_index, second.entry_index
                ),
            ));
        }

        let mut identities = BTreeSet::new();
        for entry in &entries {
            let identity = entry
                .preset
                .clone()
                .or_else(|| entry.command.clone().map(|command| command.join(" ")))
                .unwrap_or_default();
            if !identities.insert(identity) {
                return Err(field_error(
                    entry.entry_index,
                    "name",
                    "component_duplicate_name",
                    format!("组件 `{name}` 的同一来源被重复声明"),
                ));
            }
        }
    }
    Ok(())
}

fn check_dependencies(components: &[ResolvedComponent]) -> Result<(), ComponentError> {
    let names: BTreeSet<&str> = components.iter().map(|entry| entry.name.as_str()).collect();
    for component in components {
        for (field, list) in dependency_lists(component) {
            for target in list {
                if is_implicit_target(target) || names.contains(target.as_str()) {
                    continue;
                }
                return Err(field_error(
                    component.entry_index,
                    field,
                    "component_dependency_unknown",
                    format!("依赖目标 `{target}` 既不是已声明组件，也不是 stack 伪目标"),
                ));
            }
        }
    }
    Ok(())
}

fn check_cycles(components: &[ResolvedComponent]) -> Result<(), ComponentError> {
    let names: BTreeSet<&str> = components.iter().map(|entry| entry.name.as_str()).collect();
    let mut edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for component in components {
        let targets = edges.entry(component.name.as_str()).or_default();
        for list in ordering_dependency_lists(component) {
            for target in list {
                if names.contains(target.as_str()) {
                    targets.push(target);
                }
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Visiting,
        Visited,
    }

    fn visit<'a>(
        node: &'a str,
        edges: &BTreeMap<&'a str, Vec<&'a str>>,
        state: &mut HashMap<&'a str, State>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), Vec<String>> {
        match state.get(node) {
            Some(State::Visited) => return Ok(()),
            Some(State::Visiting) => {
                let mut cycle: Vec<String> = path
                    .iter()
                    .skip_while(|entry| **entry != node)
                    .map(|entry| (*entry).to_owned())
                    .collect();
                cycle.push(node.to_owned());
                return Err(cycle);
            }
            None => {}
        }
        state.insert(node, State::Visiting);
        path.push(node);
        for target in edges.get(node).into_iter().flatten() {
            visit(target, edges, state, path)?;
        }
        path.pop();
        state.insert(node, State::Visited);
        Ok(())
    }

    let mut state = HashMap::new();
    let mut path = Vec::new();
    for name in &names {
        if let Err(cycle) = visit(name, &edges, &mut state, &mut path) {
            let cycle = cycle.join(" -> ");
            return Err(ComponentError::new(
                "component_dependency_cycle",
                format!("依赖存在环：{cycle}"),
            ));
        }
    }
    Ok(())
}

fn dependency_lists<'a>(component: &'a ResolvedComponent) -> [(&'static str, &'a Vec<String>); 4] {
    [
        ("depends.after", &component.depends.after),
        ("depends.requires", &component.depends.requires),
        ("depends.wants", &component.depends.wants),
        ("depends.part_of", &component.depends.part_of),
    ]
}

fn ordering_dependency_lists<'a>(component: &'a ResolvedComponent) -> [&'a Vec<String>; 3] {
    [
        &component.depends.after,
        &component.depends.requires,
        &component.depends.wants,
    ]
}

fn is_implicit_target(target: &str) -> bool {
    matches!(target, "bridge" | "stack")
}

fn field_error(
    index: usize,
    field: &str,
    code: &'static str,
    message: impl Into<String>,
) -> ComponentError {
    ComponentError::new(
        code,
        format!("components[{index}].{field}: {}", message.into()),
    )
}

fn map_toml_error(error: toml::de::Error) -> ComponentError {
    let message = error.to_string();
    let code = if message.contains("unknown field") {
        "component_unknown_field"
    } else if message.contains("unknown variant") {
        "component_invalid_value"
    } else {
        "component_config_invalid"
    };
    ComponentError::new(code, message)
}

fn default_probe_host() -> String {
    "127.0.0.1".to_owned()
}
fn default_probe_path() -> String {
    "/healthz".to_owned()
}
fn default_bridge_url() -> String {
    "http://127.0.0.1:3100".to_owned()
}

fn default_bridge_token_env() -> String {
    "ACB_CONSOLE_TOKEN".to_owned()
}

fn default_probe_interval() -> u64 {
    DEFAULT_COMPONENT_PROBE_INTERVAL_SECS
}
fn default_failures_threshold() -> u32 {
    DEFAULT_COMPONENT_FAILURES_THRESHOLD
}
fn default_success_threshold() -> u32 {
    DEFAULT_COMPONENT_SUCCESS_THRESHOLD
}
fn default_startup_grace() -> u64 {
    DEFAULT_COMPONENT_STARTUP_GRACE_SECS
}
fn default_max_attempts() -> u32 {
    5
}
fn default_backoff_secs() -> Vec<u64> {
    vec![1, 2, 4, 8, 30]
}
fn default_backoff_cap_secs() -> u64 {
    60
}
fn default_preset_schema() -> u64 {
    1
}

#[cfg(test)]
mod tests {
    use super::installed_presets_path_from;
    use super::{ComponentType, resolve_readiness};
    use super::{Readiness, ReadinessKind};
    use std::fs;
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(label: &str) -> TempDir {
        let path = std::env::temp_dir().join(format!(
            "acb-presets-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn write_presets(prefix: &Path) -> PathBuf {
        let path = prefix.join("share/acb/presets/presets.toml");
        fs::create_dir_all(path.parent().expect("parent")).expect("create presets dir");
        fs::write(&path, "[preset]\nschema = 1\n").expect("write presets");
        path
    }

    #[test]
    fn installed_presets_follow_bin_parent() {
        let dir = temp_dir("layout");
        let prefix = dir.0.join("acbl");
        let bin = prefix.join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        let exe = bin.join("acbctl");
        fs::write(&exe, "").expect("write exe");
        let expected = write_presets(&prefix);
        assert_eq!(installed_presets_path_from(&exe), Some(expected));
    }

    #[test]
    fn installed_presets_resolve_symlinked_bin_directory() {
        let dir = temp_dir("symlink");
        let prefix = dir.0.join("acbl");
        let bin = prefix.join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        let exe = bin.join("acbctl");
        fs::write(&exe, "").expect("write exe");
        let expected = write_presets(&prefix);

        // 模拟 /run/current-system/sw/bin：目录本身是软链，指向 store 里的 bin。
        let link_root = dir.0.join("sw");
        fs::create_dir_all(&link_root).expect("create link root");
        std::os::unix::fs::symlink(&bin, link_root.join("bin")).expect("symlink bin");

        assert_eq!(
            installed_presets_path_from(&link_root.join("bin/acbctl")),
            Some(expected)
        );
    }

    #[test]
    fn installed_presets_absent_returns_none() {
        let dir = temp_dir("absent");
        let bin = dir.0.join("prefix/bin");
        fs::create_dir_all(&bin).expect("create bin");
        let exe = bin.join("acbctl");
        fs::write(&exe, "").expect("write exe");
        assert_eq!(installed_presets_path_from(&exe), None);
    }
    /// `kind = "bridge"` 必须带 `harness`——没有它探针无从判断问谁。
    #[test]
    fn bridge_probe_requires_harness() {
        let raw = Readiness {
            kind: Some(ReadinessKind::Bridge),
            harness: None,
            ..Readiness::default()
        };
        let error = resolve_readiness(0, ComponentType::Simple, Some(&raw), None)
            .expect_err("缺 harness 必须拒绝");
        assert!(
            format!("{error}").contains("harness"),
            "拒绝原因要指出缺的是 harness，实际：{error}"
        );
    }

    /// 带 `harness` 的 bridge 探针能解析，且 endpoint 说清楚查的是谁。
    #[test]
    fn bridge_probe_resolves_with_harness() {
        let raw = Readiness {
            kind: Some(ReadinessKind::Bridge),
            harness: Some("opencode".to_owned()),
            ..Readiness::default()
        };
        let resolved =
            resolve_readiness(0, ComponentType::Simple, Some(&raw), None).expect("能解析");
        assert_eq!(resolved.kind, ReadinessKind::Bridge);
        assert_eq!(resolved.harness.as_deref(), Some("opencode"));
        assert!(
            resolved.endpoint().contains("opencode"),
            "endpoint 要能看出查的是哪个 harness，实际：{}",
            resolved.endpoint()
        );
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    fn presets() -> PresetRegistry {
        PresetRegistry::from_toml(
            r#"
[[preset]]
id = "c"
component = "snow"
type = "oneshot"
source = "container"
script = "/bin/true"
"#,
        )
        .expect("presets")
    }

    fn parse(base: &Path) -> ComponentTable {
        let text = r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "snow"
type = "oneshot"
preset = "c"
enabled = true
file = "compose/x.yml"
compose_bin = ["podman-compose"]
[component.process]
cwd = "work"
env_file = "secrets/dev.env"
[component.readiness]
kind = "tcp"
port = 1
"#;
        parse_config_file(text, &presets(), base)
            .expect("配置合法")
            .1
    }

    /// 相对路径按**配置文件的父目录**解析，不是进程 cwd。
    ///
    /// 这是「配置可跟着仓库走」的前提：写死 `/home/<user>/...` 一换位置就失效，
    /// 而按 cwd 解析会让「从哪个目录启动」变成隐含依赖。
    #[test]
    fn relative_paths_resolve_against_the_config_dir() {
        let table = parse(Path::new("/srv/acb"));
        let entry = &table.components[0];
        assert_eq!(entry.file.as_deref(), Some("/srv/acb/compose/x.yml"));
        assert_eq!(entry.process.cwd.as_deref(), Some("/srv/acb/work"));
        assert_eq!(
            entry.process.env_file.as_deref(),
            Some("/srv/acb/secrets/dev.env")
        );
    }

    /// 绝对路径原样保留：不强行改写用户明确写下的东西。
    #[test]
    fn absolute_paths_are_left_alone() {
        let text = r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "snow"
type = "oneshot"
preset = "c"
enabled = true
file = "/abs/x.yml"
compose_bin = ["podman-compose"]
[component.readiness]
kind = "tcp"
port = 1
"#;
        let table = parse_config_file(text, &presets(), Path::new("/srv/acb"))
            .expect("配置合法")
            .1;
        assert_eq!(table.components[0].file.as_deref(), Some("/abs/x.yml"));
    }
}
