//! 插件注册表：库里的插件清单，含命令指纹、策略、存储与能力声明。
//!
//! 为什么进库：`[[tool_plugins]]` 写在 `config.d` 里，改一条要改文件 + 重启，
//! 控制台看不见、审计不了，运行时也判断不出「这条命令现在还能不能执行」。
//! schema 17 起插件是业务对象：命令、参数、超时、启用状态、存储位置全在库里，
//! 文件只在库为空时做一次 seed。
//!
//! 能力分两层：core 永远保留 `plugin` 这道粗门禁；插件可以额外声明
//! `plugin:<id>:<name>` 细能力，但**不能**铸造 core 能力名。求值时取
//! `max(core 下限, 插件声明)`，未知/缺省按 high + 票据处理。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 命令怎么解析。`nix_store` 是历史事实，不是特权：store 路径会被 GC 掉，
/// 所以要指纹 + 可执行检查，失败时显式 degraded，而不是启动期崩掉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    /// 绝对路径，`/usr/bin/foo`。
    Absolute,
    /// 名字，靠 `PATH` 找。
    Profile,
    /// `/nix/store/...` 下的构建产物；可被 GC。
    NixStore,
    /// 相对配置文件目录的路径。
    Relative,
}

impl CommandKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absolute => "absolute",
            Self::Profile => "profile",
            Self::NixStore => "nix_store",
            Self::Relative => "relative",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "absolute" => Self::Absolute,
            "profile" => Self::Profile,
            "nix_store" => Self::NixStore,
            "relative" => Self::Relative,
            _ => return None,
        })
    }

    /// 按命令形状推断。配置里不写就靠这个。
    pub fn infer(command: &str) -> Self {
        if command.starts_with("/nix/store/") {
            Self::NixStore
        } else if command.starts_with('/') {
            Self::Absolute
        } else if command.contains('/') {
            Self::Relative
        } else {
            Self::Profile
        }
    }
}

/// 命令当前能不能跑。`Missing` / `NotExecutable` 都算 degraded，不阻塞启动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandStatus {
    Ok(PathBuf),
    Missing,
    NotExecutable(PathBuf),
}

impl CommandStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Ok(path) => format!("ok: {}", path.display()),
            Self::Missing => "missing".to_owned(),
            Self::NotExecutable(path) => format!("not executable: {}", path.display()),
        }
    }
}

/// 检查命令是否存在且可执行。`Relative` 需要基准目录（配置目录）。
pub fn check_command(command: &str, kind: CommandKind, base: Option<&Path>) -> CommandStatus {
    let path = match kind {
        CommandKind::Profile => match which(command) {
            Some(path) => path,
            None => return CommandStatus::Missing,
        },
        CommandKind::Relative => match base {
            Some(base) => base.join(command),
            None => return CommandStatus::Missing,
        },
        CommandKind::Absolute | CommandKind::NixStore => PathBuf::from(command),
    };
    if !path.exists() {
        return CommandStatus::Missing;
    }
    if !is_executable(&path) {
        return CommandStatus::NotExecutable(path);
    }
    CommandStatus::Ok(path)
}

fn which(command: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file() && is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// 命令指纹：`sha256:<hex>`。命令或参数变了指纹就变，用来发现「库里的记录
/// 和实际部署对不上」——尤其是 nix store 路径被 GC 之后重建的产物。
pub fn command_fingerprint(command: &str, args: &[String]) -> String {
    use ring::digest::{SHA256, digest};
    let mut material = String::with_capacity(command.len() + 16 * args.len());
    material.push_str(command);
    for arg in args {
        material.push('\0');
        material.push_str(arg);
    }
    let hash = digest(&SHA256, material.as_bytes());
    format!("sha256:{}", hex(hash.as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 插件种类。策略插件是 P6 才真正启用的占位，先让 schema 认得。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    Tool,
    Policy,
}

impl PluginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Policy => "policy",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "tool" => Self::Tool,
            "policy" => Self::Policy,
            _ => return None,
        })
    }
}

/// 存储模式：`builtin` 用 ACB 的通用 KV/资源表；`external` 是插件自己的库，
/// ACB 只保存位置与凭据，不转发 SQL、不读数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Builtin,
    External,
}

impl StorageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::External => "external",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "builtin" => Self::Builtin,
            "external" => Self::External,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocationKind {
    None,
    Path,
    Dsn,
}

impl LocationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Path => "path",
            Self::Dsn => "dsn",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "none" => Self::None,
            "path" => Self::Path,
            "dsn" => Self::Dsn,
            _ => return None,
        })
    }
}

/// 数据分类：`policy` 类数据由 ACB 管（备份/迁移/降级都覆盖），`domain` 是插件自己的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    Domain,
    Policy,
}

impl DataClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Policy => "policy",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "domain" => Self::Domain,
            "policy" => Self::Policy,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStorageConfig {
    pub kind: StorageKind,
    pub location_kind: LocationKind,
    pub location: Option<String>,
    pub data_class: DataClass,
    pub backup: bool,
    pub retention: Option<String>,
}

impl Default for PluginStorageConfig {
    fn default() -> Self {
        Self {
            kind: StorageKind::Builtin,
            location_kind: LocationKind::None,
            location: None,
            data_class: DataClass::Domain,
            backup: false,
            retention: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeKind {
    Room,
    Account,
    Global,
}

impl ScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Room => "room",
            Self::Account => "account",
            Self::Global => "global",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "room" => Self::Room,
            "account" => Self::Account,
            "global" => Self::Global,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Resident,
    OnDemand,
}

impl Lifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::OnDemand => "on_demand",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "resident" => Self::Resident,
            "on_demand" => Self::OnDemand,
            _ => return None,
        })
    }
}

/// 插件运行策略。和 `[tool_plugins]` 时代的差别：作用域、能力、限流、生命周期都显式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginPolicyConfig {
    pub enabled: bool,
    pub scope_kind: ScopeKind,
    pub scope_id: Option<String>,
    pub capabilities: Vec<String>,
    pub rate_limit: Option<serde_json::Value>,
    pub lifecycle: Lifecycle,
    pub allow_unknown: bool,
}

impl Default for PluginPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scope_kind: ScopeKind::Global,
            scope_id: None,
            capabilities: vec!["tool_call".to_owned()],
            rate_limit: None,
            lifecycle: Lifecycle::Resident,
            allow_unknown: false,
        }
    }
}

/// 插件声明的细能力。`capability_id` 必须是 `plugin:<plugin_id>:<name>`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCapabilityDecl {
    pub capability_id: String,
    pub risk: String,
    pub requires_ticket: bool,
    pub scopes: Vec<String>,
    pub description: String,
}

impl PluginCapabilityDecl {
    pub fn is_valid_id(plugin_id: &str, capability_id: &str) -> bool {
        let Some(rest) = capability_id.strip_prefix("plugin:") else {
            return false;
        };
        let mut parts = rest.splitn(2, ':');
        let owner = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        !owner.is_empty() && !name.is_empty() && owner == plugin_id
    }
}

/// 库里的一条插件注册记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryPlugin {
    pub id: String,
    pub kind: PluginKind,
    pub trust_tier: String,
    pub command: String,
    pub args: Vec<String>,
    pub timeout_ms: u64,
    pub command_kind: CommandKind,
    pub command_fingerprint: Option<String>,
    /// manifest 原样 JSON；解析/校验在 P4 做，这里只保证形状。
    pub manifest_json: String,
    pub schema_version: u32,
    pub source_seed: Option<String>,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub version: i64,
    #[serde(default)]
    pub policy: PluginPolicyConfig,
    #[serde(default)]
    pub storage: PluginStorageConfig,
    #[serde(default)]
    pub capabilities: Vec<PluginCapabilityDecl>,
}

impl RegistryPlugin {
    /// 校验必须成立的不变量：id 合法、命令非空、超时在范围内、能力名带自己前缀。
    pub fn validate(&self) -> Result<(), String> {
        if self.id.starts_with("acb.") {
            return Err(format!("插件 id 不能用保留前缀 acb.：{}", self.id));
        }
        if self.id.trim().is_empty() || !self.id.contains('.') {
            return Err(format!("插件 id 必须形如 namespace.name：{}", self.id));
        }
        if self.command.trim().is_empty() {
            return Err(format!("插件 {} 的 command 为空", self.id));
        }
        if !(100..=60_000).contains(&self.timeout_ms) {
            return Err(format!(
                "插件 {} 的 timeout_ms 越界（100..=60000）：{}",
                self.id, self.timeout_ms
            ));
        }
        for decl in &self.capabilities {
            if !PluginCapabilityDecl::is_valid_id(&self.id, &decl.capability_id) {
                return Err(format!(
                    "插件 {} 声明了不属于自己的能力：{}（必须是 plugin:{}:<name>）",
                    self.id, decl.capability_id, self.id
                ));
            }
            if !matches!(decl.risk.as_str(), "low" | "medium" | "high") {
                return Err(format!(
                    "插件 {} 的能力 {} risk 不合法：{}",
                    self.id, decl.capability_id, decl.risk
                ));
            }
        }
        if self.storage.kind == StorageKind::External && self.storage.location.is_none() {
            return Err(format!("插件 {} 用外部存储但没给 location", self.id));
        }
        Ok(())
    }

    /// 转成启动 tool provider 用的形状。`callable` 由调用方用 [`check_command`] 得到。
    pub fn tool_config(&self) -> crate::config::ToolPluginConfig {
        crate::config::ToolPluginConfig {
            id: self.id.clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            timeout_ms: Some(self.timeout_ms),
            env: Vec::new(),
        }
    }

    /// 指纹对不上 = 库里记录和实际部署漂移了。
    pub fn fingerprint_matches(&self) -> bool {
        match &self.command_fingerprint {
            Some(expected) => *expected == command_fingerprint(&self.command, &self.args),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RegistryPlugin {
        RegistryPlugin {
            id: "issue.tools".to_owned(),
            kind: PluginKind::Tool,
            trust_tier: "tool".to_owned(),
            command: "/nix/store/x-acb-issue-tools/bin/acb-issue-tools".to_owned(),
            args: vec![],
            timeout_ms: 10_000,
            command_kind: CommandKind::NixStore,
            command_fingerprint: None,
            manifest_json: "{}".to_owned(),
            schema_version: 0,
            source_seed: None,
            enabled: true,
            created_at: 0,
            updated_at: 0,
            version: 1,
            policy: PluginPolicyConfig::default(),
            storage: PluginStorageConfig::default(),
            capabilities: vec![],
        }
    }

    #[test]
    fn kind_inference_matches_command_shape() {
        assert_eq!(
            CommandKind::infer("/nix/store/x/bin/y"),
            CommandKind::NixStore
        );
        assert_eq!(CommandKind::infer("/usr/bin/y"), CommandKind::Absolute);
        assert_eq!(CommandKind::infer("./bin/y"), CommandKind::Relative);
        assert_eq!(CommandKind::infer("y"), CommandKind::Profile);
    }

    #[test]
    fn fingerprint_changes_with_args() {
        let a = command_fingerprint("/bin/x", &[]);
        let b = command_fingerprint("/bin/x", &["--flag".to_owned()]);
        assert_ne!(a, b);
        assert!(a.starts_with("sha256:"));
        assert_eq!(a.len(), "sha256:".len() + 64);
    }

    #[test]
    fn validation_rejects_foreign_capability_names() {
        let mut plugin = sample();
        plugin.capabilities.push(PluginCapabilityDecl {
            capability_id: "plugin:other.tools:run".to_owned(),
            risk: "high".to_owned(),
            requires_ticket: true,
            scopes: vec![],
            description: String::new(),
        });
        let error = plugin.validate().unwrap_err();
        assert!(error.contains("不属于自己"));
    }

    #[test]
    fn validation_requires_location_for_external_storage() {
        let mut plugin = sample();
        plugin.storage.kind = StorageKind::External;
        assert!(plugin.validate().is_err());
        plugin.storage.location = Some("/var/lib/acb/x.db".to_owned());
        assert!(plugin.validate().is_ok());
    }

    #[test]
    fn reserved_prefix_is_rejected() {
        let mut plugin = sample();
        plugin.id = "acb.tools".to_owned();
        assert!(plugin.validate().is_err());
    }

    #[test]
    fn missing_command_is_missing() {
        assert_eq!(
            check_command("/nonexistent/xyz", CommandKind::Absolute, None),
            CommandStatus::Missing
        );
    }

    #[test]
    fn fingerprint_matches_when_recorded() {
        let mut plugin = sample();
        plugin.command_fingerprint = Some(command_fingerprint(&plugin.command, &plugin.args));
        assert!(plugin.fingerprint_matches());
        plugin.args = vec!["--extra".to_owned()];
        assert!(!plugin.fingerprint_matches());
    }
}
