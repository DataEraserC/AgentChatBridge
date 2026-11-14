//! 访问授权的内置实现：四元组 + 目标。
//!
//! 这一层只做**授权叠加**：命中配置里的常态授予时给出 `allow`；没命中时退回
//! 现有的订阅边界。风险等级只是操作属性（默认映射 / 审计标注 / 票据门槛），
//! 不参与鉴权。
//! 不做旧 provider 的兼容翻译：没有用户需要平滑迁移，多一层翻译只会让旧配置
//! 悄悄多出权限。旧键留在配置里会在启动期直接报错。
//!
//! 票据/审批流不在本模块：阶段二单独实现。

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::RoomId;

/// 授权目标种类。规则匹配只看这个枚举，不再对字符串做前缀猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessTargetKind {
    Room,
    Message,
    Member,
    File,
    Account,
    Connection,
    PluginTool,
    /// 单条平台方法，如 `group.mute`；`sub` 是连接名。
    PlatformOp,
}

impl AccessTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Room => "room",
            Self::Message => "message",
            Self::Member => "member",
            Self::File => "file",
            Self::Account => "account",
            Self::Connection => "connection",
            Self::PluginTool => "plugin_tool",
            Self::PlatformOp => "platform_op",
        }
    }
}

/// 一个类型化的授权目标。
///
/// `id` 是目标自己的标识（房间全名、消息 id、成员 id、账号、连接名……）；
/// `sub` 是可选从属范围（成员/文件/消息所属房间，账号所属连接）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccessTarget {
    pub kind: AccessTargetKind,
    pub id: String,
    #[serde(default)]
    pub sub: Option<String>,
}

impl AccessTarget {
    pub fn room(room: &RoomId) -> Self {
        Self {
            kind: AccessTargetKind::Room,
            id: room.to_string(),
            sub: None,
        }
    }

    pub fn connection(connection: &str) -> Self {
        Self {
            kind: AccessTargetKind::Connection,
            id: connection.to_owned(),
            sub: None,
        }
    }

    pub fn account(connection: &str, account: &str) -> Self {
        Self {
            kind: AccessTargetKind::Account,
            id: account.to_owned(),
            sub: Some(connection.to_owned()),
        }
    }

    pub fn member(room: &RoomId, member: &str) -> Self {
        Self {
            kind: AccessTargetKind::Member,
            id: member.to_owned(),
            sub: Some(room.to_string()),
        }
    }

    /// 单条平台方法：`id` 是方法名，`sub` 是连接名。
    pub fn platform_op(connection: &str, method: &str) -> Self {
        Self {
            kind: AccessTargetKind::PlatformOp,
            id: method.to_owned(),
            sub: Some(connection.to_owned()),
        }
    }

    /// 从 `room / member / message / file` 这类带房间从属的目标里取回房间。
    pub fn room_ref(&self) -> Option<&str> {
        match self.kind {
            AccessTargetKind::Room => Some(&self.id),
            AccessTargetKind::Member | AccessTargetKind::Message | AccessTargetKind::File => {
                self.sub.as_deref()
            }
            _ => None,
        }
    }

    pub fn connection_ref(&self) -> Option<&str> {
        match self.kind {
            AccessTargetKind::Connection => Some(&self.id),
            AccessTargetKind::Account | AccessTargetKind::PlatformOp => self.sub.as_deref(),
            _ => self
                .room_ref()
                .and_then(|room| room.split_once(':').map(|(connection, _)| connection)),
        }
    }

    pub fn display(&self) -> String {
        match self.kind {
            AccessTargetKind::Room => format!("房间 {}", self.id),
            AccessTargetKind::Account => format!(
                "账号 {}{}",
                self.id,
                self.sub
                    .as_deref()
                    .map(|connection| format!("（连接 {connection}）"))
                    .unwrap_or_default()
            ),
            AccessTargetKind::Connection => format!("连接 {}", self.id),
            AccessTargetKind::Member => format!(
                "成员 {}{}",
                self.id,
                self.sub
                    .as_deref()
                    .map(|room| format!("（房间 {room}）"))
                    .unwrap_or_default()
            ),
            AccessTargetKind::Message => format!("消息 {}", self.id),
            AccessTargetKind::File => format!("文件 {}", self.id),
            AccessTargetKind::PluginTool => format!("插件工具 {}", self.id),
            AccessTargetKind::PlatformOp => format!(
                "平台方法 {}{}",
                self.id,
                self.sub
                    .as_deref()
                    .map(|connection| format!("（连接 {connection}）"))
                    .unwrap_or_default()
            ),
        }
    }
}

impl fmt::Display for AccessTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind.as_str(), self.id)
    }
}

/// 授权主体。`System` 表示没有人类触发者的轮次（clock/自发唤醒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantSubject {
    System,
    SystemAdmin,
    BotAdmin,
    User,
}

impl GrantSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::SystemAdmin => "system_admin",
            Self::BotAdmin => "bot_admin",
            Self::User => "user",
        }
    }
}

/// 房间的结构事实：一对一还是多人。**由适配器声明，core 只消费**。
///
/// 缺省值必须是 `MultiParty`：旧适配器/旧配置没有这个字段时，宁可多问几次，
/// 也不能因为缺字段把多人房间误当成私信而放宽。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RoomScope {
    /// 一对一（私信、单聊）。
    Direct,
    /// 多人（群组、频道、多人会话）。
    MultiParty,
}

impl RoomScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::MultiParty => "multi_party",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "direct" => Some(Self::Direct),
            "multi_party" | "multiparty" => Some(Self::MultiParty),
            _ => None,
        }
    }
}

/// 本轮发生的场所。结构事实，不含政策判断。
///
/// `Room` 只带 `RoomScope`（一对一/多人）；Trust 是配置轴，不折进这里。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    Console,
    Room(RoomScope),
    None,
}

impl Place {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Console => "console",
            Self::Room(RoomScope::Direct) => "private",
            Self::Room(RoomScope::MultiParty) => "group",
            Self::None => "none",
        }
    }

    pub fn room_scope(self) -> Option<RoomScope> {
        match self {
            Self::Room(scope) => Some(scope),
            _ => None,
        }
    }
}

impl Serialize for Place {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Place {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.trim() {
            "private" | "direct" => Ok(Self::Room(RoomScope::Direct)),
            "group" | "multi_party" | "multiparty" => Ok(Self::Room(RoomScope::MultiParty)),
            "console" => Ok(Self::Console),
            "none" => Ok(Self::None),
            other => Err(serde::de::Error::custom(format!(
                "unknown place {other:?}; expected private/group/console/none"
            ))),
        }
    }
}

/// 配置轴：这个房间是否可信。默认 `Untrusted`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    #[default]
    Untrusted,
    Trusted,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Trusted => "trusted",
        }
    }
}

/// 能力范围。
///
/// `subscribed` 只覆盖本会话可达目标；跨目标要靠 `room` / `connection` 的显式
/// typed target。旧值 `any` 与 `subscribed` 语义完全相同，已移除；配置里再写
/// `any` 会在解析期报错，不做静默映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    Subscribed,
    Room,
    Connection,
    /// 单条平台方法；target 必须是 `kind = "platform_op"`。
    PlatformOp,
}

impl<'de> Deserialize<'de> for GrantScope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.trim() {
            "subscribed" => Ok(Self::Subscribed),
            "room" => Ok(Self::Room),
            "connection" => Ok(Self::Connection),
            "platform_op" => Ok(Self::PlatformOp),
            "any" => Err(serde::de::Error::custom(
                "scope \"any\" has been removed; use \"subscribed\" instead \
                 (the two have identical reachability semantics)",
            )),
            other => Err(serde::de::Error::custom(format!(
                "unknown scope {other:?}; expected one of \"subscribed\", \"room\", \"connection\", \"platform_op\""
            ))),
        }
    }
}

impl Serialize for GrantScope {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Subscribed => "subscribed",
            Self::Room => "room",
            Self::Connection => "connection",
            Self::PlatformOp => "platform_op",
        })
    }
}

/// 能力枚举。封闭值域，未知值在配置解析期报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantCapability {
    RoomRead,
    RoomWrite,
    ConnectionRead,
    Platform,
    Destructive,
    /// 最高风险档。默认不授予；规则显式授予才直调（强烈不建议）。
    Critical,
    /// 读账号资料。与取账号凭证的 `account_secret` 分开。
    AccountRead,
    /// 取账号凭证。与 `critical` 同级，不可常驻授予，只能走票据。
    AccountSecret,
    /// 能不能发起 `access_request`。与房间读写能力正交。
    AccessRequest,
    Plugin,
    /// 能不能批准票据。批准还要另行校验「审批人有票据申请的能力 + 范围覆盖」。
    AccessApprove,
    /// 读配置对象（会话/人格/插件配置等）。给 agent 配置工具用。
    ConfigRead,
    /// 写配置对象。高危，默认走票据。
    ConfigWrite,
    /// 改权限规则/角色/指派。最高危，默认走票据。
    PolicyWrite,
    /// 注册/卸载插件、改插件 command 或存储类型。
    PluginAdmin,
}

impl GrantCapability {
    /// 只能由票据本身持有、不能通过规则/角色授予的能力。
    ///
    /// `account_secret` 是取账号凭据：一旦某主体长期持有，票据审批就形同虚设。
    /// 所以它只能随一张已批准的票短暂生效；批准人固定是 system_admin（break-glass）。
    pub fn is_ticket_only(self) -> bool {
        matches!(self, Self::AccountSecret)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RoomRead => "room_read",
            Self::RoomWrite => "room_write",
            Self::ConnectionRead => "connection_read",
            Self::Platform => "platform",
            Self::Destructive => "destructive",
            Self::Critical => "critical",
            Self::AccountRead => "account_read",
            Self::AccountSecret => "account_secret",
            Self::AccessRequest => "access_request",
            Self::Plugin => "plugin",
            Self::AccessApprove => "access_approve",
            Self::ConfigRead => "config_read",
            Self::ConfigWrite => "config_write",
            Self::PolicyWrite => "policy_write",
            Self::PluginAdmin => "plugin_admin",
        }
    }
}

/// 角色：一组能力的命名集合。内置角色只用于展示/迁移，求值仍走 floor。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyRole {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub capabilities: Vec<GrantCapability>,
}

/// 指派的主体种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentSubjectKind {
    /// 固定主体名：`system_admin` / `bot_admin` / `user` / `system`。
    Subject,
    /// `连接:账号`，如 `qq:323622442`。
    Account,
    /// `连接:房间#成员`，如 `qq:group/123#456`。
    Member,
}

/// 指派的范围。`global` 对所有资源生效；`connection` / `room` 只覆盖对应范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignmentScopeKind {
    Global,
    Connection,
    Room,
}

/// 指派时的角色快照。角色日后扩容不追认旧指派，审计能看到当时的能力集合。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSnapshot {
    pub id: String,
    pub name: String,
    pub capabilities: Vec<GrantCapability>,
}

/// 一条角色指派。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyAssignment {
    pub id: String,
    pub subject_kind: AssignmentSubjectKind,
    pub subject_id: String,
    pub role_id: String,
    pub scope_kind: AssignmentScopeKind,
    #[serde(default)]
    pub scope_id: Option<String>,
    /// 判定实际使用的能力快照；角色日后扩容不影响已发出的指派。
    pub capability_snapshot: Vec<GrantCapability>,
    pub role_snapshot: RoleSnapshot,
    #[serde(default)]
    pub needs_review: bool,
}

/// 内置角色的展示定义。求值仍走 floor；它们只让控制台能看懂「系统里本来有哪几类主体」。
pub fn builtin_roles() -> Vec<PolicyRole> {
    vec![
        PolicyRole {
            id: "builtin-system".to_owned(),
            name: "system".to_owned(),
            description: "桥自身/无人类发送者".to_owned(),
            builtin: true,
            capabilities: vec![GrantCapability::RoomRead, GrantCapability::RoomWrite],
        },
        PolicyRole {
            id: "builtin-system-admin".to_owned(),
            name: "system_admin".to_owned(),
            description: "最高权限持有者".to_owned(),
            builtin: true,
            capabilities: vec![
                GrantCapability::RoomRead,
                GrantCapability::RoomWrite,
                GrantCapability::ConnectionRead,
                GrantCapability::AccessApprove,
                GrantCapability::AccessRequest,
            ],
        },
        PolicyRole {
            id: "builtin-bot-admin".to_owned(),
            name: "bot_admin".to_owned(),
            description: "机器人管理员".to_owned(),
            builtin: true,
            capabilities: vec![
                GrantCapability::RoomRead,
                GrantCapability::RoomWrite,
                GrantCapability::ConnectionRead,
                GrantCapability::AccessApprove,
                GrantCapability::AccessRequest,
            ],
        },
        PolicyRole {
            id: "builtin-user".to_owned(),
            name: "user".to_owned(),
            description: "普通用户".to_owned(),
            builtin: true,
            capabilities: vec![GrantCapability::RoomRead, GrantCapability::RoomWrite],
        },
    ]
}

/// 一次判定里的主体上下文。用于把 `account` / `member` 指派匹配到具体的人。
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    pub connection: &'a str,
    pub account: Option<&'a str>,
    pub room: Option<&'a RoomId>,
    pub member: Option<&'a str>,
}

/// 一个常态授予规则。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRule {
    /// 稳定标识；Decision 与审计用它追溯来源。
    pub id: String,
    pub subject: GrantSubject,
    pub place: Place,
    pub scope: GrantScope,
    #[serde(default)]
    pub target: Option<AccessTarget>,
    pub capability: Vec<GrantCapability>,
    /// 细粒度权限字符；空 = 不过滤（沿用粗粒度 scope 判定）。
    ///
    /// 每项形如 `qq:group.mute`，末段可用 `*` 通配一层（`qq:group.*` 匹配
    /// `qq:group.mute` 与 `qq:group.file.list`，但不匹配 `qq:app.close`）。
    /// 裸 `*` 或居中 `*` 无效。
    #[serde(default)]
    pub permissions: Vec<String>,
}

/// 房间级授权政策。结构事实（`RoomScope`）由适配器声明，这里只放政策判断。
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RoomGrant {
    #[serde(default)]
    pub trust: Trust,
    /// 可信成员名单。空 = 按 `trust` 生效（风险更高，文档里明说）。
    #[serde(default)]
    pub members: Vec<String>,
    /// 覆盖默认审批目标；空 = 走账号级，再走全局默认。
    #[serde(default)]
    pub approval_route: Vec<String>,
}

/// 账号级审批路由；键形如 `qq:10001`（连接:账号）。
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountGrant {
    #[serde(default)]
    pub approval_route: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantAccount {
    pub connection: String,
    pub account: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BotAdminGrant {
    pub accounts: Vec<GrantAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemAdminGrant {
    pub connection: String,
    pub account: String,
}

/// `[grants]` 配置。未知字段由 `deny_unknown_fields` 在解析期拒绝。
///
/// 这一份既是从 `config.toml` 读进来的形状，也是落库的形状（schema 13 起
/// `policy_*` 四张表按对象拆开存）。可序列化是为了能整份写进库里、也能在
/// 控制台原样回显。
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsConfig {
    #[serde(default)]
    pub rules: Vec<GrantRule>,
    /// 显式拒绝；只做减法，和 allow 规则顺序无关。
    #[serde(default)]
    pub deny: Vec<GrantRule>,
    #[serde(default)]
    pub bot_admin: Option<BotAdminGrant>,
    #[serde(default)]
    pub system_admin: Option<SystemAdminGrant>,
    /// 房间级政策：`[grants.rooms."qq:group/1"]`。
    #[serde(default)]
    pub rooms: std::collections::BTreeMap<String, RoomGrant>,
    /// 账号级审批路由：`[grants.accounts."qq:10001"]`。
    #[serde(default)]
    pub accounts: std::collections::BTreeMap<String, AccountGrant>,
    /// 自定义角色。内置角色由迁移写入，求值仍走 floor。
    #[serde(default)]
    pub roles: Vec<PolicyRole>,
    /// 角色指派。按 `subject_kind/subject_id` + scope 匹配。
    #[serde(default)]
    pub assignments: Vec<PolicyAssignment>,
}

impl GrantsConfig {
    /// 解析期校验：权限配置写错必须启动就炸。
    pub fn validate(&self) -> Result<(), String> {
        if let Some(system) = &self.system_admin {
            if system.connection.trim().is_empty() || system.account.trim().is_empty() {
                return Err("system_admin connection/account must not be empty".to_owned());
            }
        }
        if let Some(bot) = &self.bot_admin {
            if bot.accounts.is_empty() {
                return Err("bot_admin.accounts must not be empty".to_owned());
            }
            let mut seen = std::collections::BTreeSet::new();
            for account in &bot.accounts {
                if account.connection.trim().is_empty() || account.account.trim().is_empty() {
                    return Err("bot_admin account connection/account must not be empty".to_owned());
                }
                if !seen.insert((account.connection.as_str(), account.account.as_str())) {
                    return Err(format!(
                        "bot_admin account declared twice: {}:{}",
                        account.connection, account.account
                    ));
                }
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for (label, index, rule) in self
            .rules
            .iter()
            .enumerate()
            .map(|(index, rule)| ("rules", index, rule))
            .chain(
                self.deny
                    .iter()
                    .enumerate()
                    .map(|(index, rule)| ("deny", index, rule)),
            )
        {
            let id = rule.id.trim();
            if id.is_empty() {
                return Err(format!("grants.{label}[{index}] id must not be empty"));
            }
            if !ids.insert(id.to_owned()) {
                return Err(format!("duplicate grant rule id {id}"));
            }
            if rule.capability.is_empty() {
                return Err(format!(
                    "grants.{label}[{index}] capability must not be empty"
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            let mut seen_permissions = std::collections::BTreeSet::new();
            for capability in &rule.capability {
                if matches!(*capability, GrantCapability::AccountSecret) {
                    return Err(format!(
                        "grants.{label}[{index}] capability {} cannot be granted by config; \
                         it requires an access_request ticket approved in private",
                        capability.as_str()
                    ));
                }
                if !seen.insert(*capability) {
                    return Err(format!(
                        "grants.{label}[{index}] capability declared twice: {capability:?}"
                    ));
                }
            }
            for permission in &rule.permissions {
                if let Err(reason) = validate_permission_string(permission) {
                    return Err(format!(
                        "grants.{label}[{index}] permissions entry {permission:?}: {reason}"
                    ));
                }
                if !seen_permissions.insert(permission.clone()) {
                    return Err(format!(
                        "grants.{label}[{index}] permissions entry declared twice: {permission:?}"
                    ));
                }
            }
            match rule.scope {
                GrantScope::Subscribed => {
                    if rule.target.is_some() {
                        return Err(format!(
                            "grants.{label}[{index}] target is only allowed for scope room/connection"
                        ));
                    }
                }
                GrantScope::Room => {
                    let Some(target) = rule.target.as_ref() else {
                        return Err(format!(
                            "grants.{label}[{index}] scope room requires target"
                        ));
                    };
                    if target.kind != AccessTargetKind::Room || target.id.trim().is_empty() {
                        return Err(format!(
                            "grants.{label}[{index}] scope room target must be kind = \"room\" with a room id"
                        ));
                    }
                    RoomId::parse(target.id.trim()).map_err(|error| {
                        format!("grants.{label}[{index}] target is not a room: {error}")
                    })?;
                }
                GrantScope::Connection => {
                    let Some(target) = rule.target.as_ref() else {
                        return Err(format!(
                            "grants.{label}[{index}] scope connection requires target"
                        ));
                    };
                    if target.kind != AccessTargetKind::Connection || target.id.trim().is_empty() {
                        return Err(format!(
                            "grants.{label}[{index}] scope connection target must be kind = \"connection\" with a connection id"
                        ));
                    }
                }
                GrantScope::PlatformOp => {
                    let Some(target) = rule.target.as_ref() else {
                        return Err(format!(
                            "grants.{label}[{index}] scope platform_op requires target"
                        ));
                    };
                    if target.kind != AccessTargetKind::PlatformOp
                        || target.id.trim().is_empty()
                        || target
                            .sub
                            .as_deref()
                            .is_none_or(|sub| sub.trim().is_empty())
                    {
                        return Err(format!(
                            "grants.{label}[{index}] scope platform_op target must be \
                             {{ kind = \"platform_op\", id = \"<method>\", sub = \"<connection>\" }}"
                        ));
                    }
                }
            }
        }
        for (room, grant) in &self.rooms {
            RoomId::parse(room)
                .map_err(|error| format!("grants.rooms key {room:?} is not a room id: {error}"))?;
            for member in &grant.members {
                if member.trim().is_empty() {
                    return Err(format!(
                        "grants.rooms.{room}.members must not contain empty ids"
                    ));
                }
            }
            for target in &grant.approval_route {
                RoomId::parse(target).map_err(|error| {
                    format!(
                        "grants.rooms.{room}.approval_route target {target:?} is not a room id: {error}"
                    )
                })?;
            }
        }
        for (account, grant) in &self.accounts {
            let Some((connection, id)) = account.split_once(':') else {
                return Err(format!(
                    "grants.accounts key {account:?} must be connection:account"
                ));
            };
            if connection.trim().is_empty() || id.trim().is_empty() {
                return Err(format!(
                    "grants.accounts key {account:?} must be connection:account"
                ));
            }
            for target in &grant.approval_route {
                RoomId::parse(target).map_err(|error| {
                    format!(
                        "grants.accounts.{account}.approval_route target {target:?} is not a room id: {error}"
                    )
                })?;
            }
        }
        let mut role_ids = std::collections::BTreeSet::new();
        for (index, role) in self.roles.iter().enumerate() {
            let id = role.id.trim();
            if id.is_empty() {
                return Err(format!("grants.roles[{index}] id must not be empty"));
            }
            if !role_ids.insert(id.to_owned()) {
                return Err(format!("duplicate role id {id}"));
            }
            if role.name.trim().is_empty() {
                return Err(format!("grants.roles[{index}] name must not be empty"));
            }
            if role.capabilities.is_empty() {
                return Err(format!(
                    "grants.roles[{index}] capabilities must not be empty"
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for capability in &role.capabilities {
                if !seen.insert(*capability) {
                    return Err(format!(
                        "grants.roles[{index}] capability declared twice: {}",
                        capability.as_str()
                    ));
                }
            }
        }
        let mut assignment_ids = std::collections::BTreeSet::new();
        for (index, assignment) in self.assignments.iter().enumerate() {
            let id = assignment.id.trim();
            if id.is_empty() {
                return Err(format!("grants.assignments[{index}] id must not be empty"));
            }
            if !assignment_ids.insert(id.to_owned()) {
                return Err(format!("duplicate assignment id {id}"));
            }
            if assignment.subject_id.trim().is_empty() {
                return Err(format!(
                    "grants.assignments[{index}] subject_id must not be empty"
                ));
            }
            match assignment.subject_kind {
                AssignmentSubjectKind::Account if !assignment.subject_id.contains(':') => {
                    return Err(format!(
                        "grants.assignments[{index}] account subject_id must be connection:account"
                    ));
                }
                AssignmentSubjectKind::Member if !assignment.subject_id.contains('#') => {
                    return Err(format!(
                        "grants.assignments[{index}] member subject_id must be connection:room#member"
                    ));
                }
                _ => {}
            }
            if !role_ids.contains(assignment.role_id.trim()) {
                return Err(format!(
                    "grants.assignments[{index}] references unknown role {}",
                    assignment.role_id
                ));
            }
            if assignment.role_snapshot.id != assignment.role_id {
                return Err(format!(
                    "grants.assignments[{index}] role_snapshot.id must equal role_id"
                ));
            }
            if assignment.capability_snapshot.is_empty() {
                return Err(format!(
                    "grants.assignments[{index}] capability_snapshot must not be empty"
                ));
            }
            match assignment.scope_kind {
                AssignmentScopeKind::Global => {
                    if assignment.scope_id.is_some() {
                        return Err(format!(
                            "grants.assignments[{index}] global scope must not have scope_id"
                        ));
                    }
                }
                AssignmentScopeKind::Connection => {
                    if assignment
                        .scope_id
                        .as_deref()
                        .is_none_or(|id| id.trim().is_empty())
                    {
                        return Err(format!(
                            "grants.assignments[{index}] connection scope requires scope_id"
                        ));
                    }
                }
                AssignmentScopeKind::Room => {
                    let Some(room) = assignment.scope_id.as_deref() else {
                        return Err(format!(
                            "grants.assignments[{index}] room scope requires scope_id"
                        ));
                    };
                    RoomId::parse(room).map_err(|error| {
                        format!(
                            "grants.assignments[{index}] scope_id {room:?} is not a room: {error}"
                        )
                    })?;
                }
            }
        }
        Ok(())
    }

    pub fn subject_for(&self, connection: &str, sender: Option<&str>) -> GrantSubject {
        let Some(sender) = sender else {
            return GrantSubject::System;
        };
        if self
            .system_admin
            .as_ref()
            .is_some_and(|admin| admin.connection == connection && admin.account == sender)
        {
            return GrantSubject::SystemAdmin;
        }
        if self.bot_admin.as_ref().is_some_and(|bot| {
            bot.accounts
                .iter()
                .any(|account| account.connection == connection && account.account == sender)
        }) {
            return GrantSubject::BotAdmin;
        }
        GrantSubject::User
    }

    /// 房间级政策；没有配置就是 None（由调用方按 fail-safe 处理）。
    pub fn room(&self, room: &RoomId) -> Option<&RoomGrant> {
        self.rooms.get(&room.to_string())
    }

    pub fn room_trust(&self, room: &RoomId) -> Trust {
        self.room(room).map(|grant| grant.trust).unwrap_or_default()
    }

    /// 空名单 = 未设置；调用方据此决定是否只按 trust。
    pub fn room_members(&self, room: &RoomId) -> Option<&[String]> {
        self.room(room)
            .map(|grant| grant.members.as_slice())
            .filter(|members| !members.is_empty())
    }

    pub fn room_approval_route(&self, room: &RoomId) -> &[String] {
        self.room(room)
            .map(|grant| grant.approval_route.as_slice())
            .unwrap_or(&[])
    }

    pub fn account_approval_route(&self, connection: &str, account: &str) -> &[String] {
        self.accounts
            .get(&format!("{connection}:{account}"))
            .map(|grant| grant.approval_route.as_slice())
            .unwrap_or(&[])
    }
}

/// 一次授权评估的全部输入。把参数收成一个结构，避免 evaluator 接口继续膨胀。
#[derive(Debug, Clone, Copy)]
pub struct GrantQuery<'a> {
    pub subject: GrantSubject,
    pub place: Place,
    pub capability: GrantCapability,
    /// 具体的人：账号/成员上下文。没有 actor 时只有 `subject` 类指派能命中。
    pub actor: Option<&'a Actor<'a>>,
    /// 被评估的目标。除 `access_request` 外，目标可达性靠它判。
    pub target: &'a AccessTarget,
    /// 发起这次判定的房间（turn 所在房间）。控制台等无房间场景为 `None`。
    ///
    /// 只有 `access_request` 用它：申请能不能发，取决于「在哪申请」，
    /// 不取决于「要申请谁」——后者正是申请想打开的东西。
    pub origin: Option<&'a RoomId>,
    pub subscriptions: &'a [RoomId],
    /// 当前评估的平台方法名；只有 `platform_call` 的能力检查会填。
    pub platform_op: Option<&'a str>,
    /// `platform_call` 细粒度权限字符（形如 `qq:group.mute`）。
    /// 规则声明了 `permissions` 时按精确匹配 + 末段单层通配判定；
    /// 未声明 `permissions` 的规则不在此过滤。
    pub permissions: &'a [String],
    /// 判定所依据的策略版本；Decision 必须带回同一个值。
    pub policy_version: i64,
    /// 这一轮冻结的策略快照；Policy 实现只读它，不读全局可变状态。
    pub grants: &'a GrantsConfig,
}

/// 评估结果。豁免（basis 非 `base:`）与任何拒绝都要写审计；普通 base allow 不写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantDecision {
    pub allow: bool,
    /// 命中来源列表，逗号分隔：`base:...` / `floor:...` / `rule:<id>` / `deny:<id>`。
    pub basis: String,
    pub reason: String,
    /// 主来源 id；必须非空，便于审计追溯。
    pub rule_id: String,
    /// 判定所依据的策略版本。
    pub policy_version: i64,
}

/// 审计行里 target 的最大字符数；房间/连接 id 再长也只截到这里。
const AUDIT_TARGET_MAX_CHARS: usize = 120;

impl GrantDecision {
    /// 豁免（命中规则的 allow/deny）和所有 deny 都要审计；普通 base allow 不审计。
    pub fn should_audit(&self) -> bool {
        self.audited() || !self.allow
    }

    pub fn audited(&self) -> bool {
        self.basis
            .split(',')
            .any(|source| !source.starts_with("base:"))
    }

    /// 一行结构化审计。target 超长时截断，避免日志被超长 id 撑爆。
    pub fn audit_line(
        &self,
        subject: GrantSubject,
        place: Place,
        capability: GrantCapability,
        target: &str,
        session: &str,
    ) -> String {
        format!(
            "[access] subject={} place={} capability={} target={} result={} basis={} reason=\"{}\" session={}",
            subject.as_str(),
            place.as_str(),
            capability.as_str(),
            truncate_target(target),
            if self.allow { "allow" } else { "deny" },
            self.basis,
            self.reason.replace('"', "'"),
            session,
        )
    }

    pub(crate) fn allow(basis: impl Into<String>, reason: impl Into<String>) -> Self {
        let basis = basis.into();
        Self {
            allow: true,
            rule_id: basis
                .split(',')
                .find(|source| !source.starts_with("base:"))
                .unwrap_or(&basis)
                .to_owned(),
            basis,
            reason: reason.into(),
            policy_version: 0,
        }
    }

    pub(crate) fn deny(basis: impl Into<String>, reason: impl Into<String>) -> Self {
        let basis = basis.into();
        Self {
            allow: false,
            rule_id: basis
                .split(',')
                .find(|source| !source.starts_with("base:"))
                .unwrap_or(&basis)
                .to_owned(),
            basis,
            reason: reason.into(),
            policy_version: 0,
        }
    }

    pub(crate) fn at_version(mut self, policy_version: i64) -> Self {
        self.policy_version = policy_version;
        if self.rule_id.is_empty() {
            self.rule_id = self.basis.clone();
        }
        self
    }
}

pub(crate) fn truncate_target(target: &str) -> String {
    let mut chars = target.chars();
    let head: String = chars.by_ref().take(AUDIT_TARGET_MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

impl fmt::Display for GrantDecision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} basis={} reason={}",
            if self.allow { "allow" } else { "deny" },
            self.basis,
            self.reason
        )
    }
}

/// `access_request` 的默认判定：只看**发起位置**是否在会话订阅内。
///
/// 目标可达性刻意不在这里判。票据的用途就是打开一个会话还没订阅的目标；
/// 若申请阶段先要求目标可达，票据就永远用不上（2026-11-09 裁定）。
/// 目标该不该给，由审批人看过目标后决定，不由发起方预先证明。
pub(crate) fn origin_access(query: &GrantQuery<'_>) -> GrantDecision {
    match query.origin {
        Some(origin) if query.subscriptions.iter().any(|listed| listed == origin) => {
            GrantDecision::allow(
                "base:request_origin",
                format!("request originates from subscribed room {origin}"),
            )
        }
        Some(origin) => GrantDecision::deny(
            "base:origin_not_subscribed",
            format!("request originates from {origin}, which the session does not subscribe"),
        ),
        None => GrantDecision::deny(
            "base:no_origin",
            "access_request has no originating room; only console/admin paths may omit it",
        ),
    }
}

pub(crate) fn base_access(query: &GrantQuery<'_>) -> GrantDecision {
    if target_is_reachable(query.target, query.subscriptions) {
        GrantDecision::allow(
            "base:subscription",
            format!("reachable target for {:?}", query.capability),
        )
    } else {
        GrantDecision::deny(
            "base:not_subscribed",
            format!("target not reachable for {:?}", query.capability),
        )
    }
}

/// `subscribed` 的可达范围：房间与连接来自订阅；账号来自订阅连接的
/// 账号；成员/消息/文件必须挂在可达房间上。插件工具不靠订阅判定，交给上层。
fn target_is_reachable(target: &AccessTarget, subscriptions: &[RoomId]) -> bool {
    match target.kind {
        AccessTargetKind::Room => subscriptions
            .iter()
            .any(|listed| listed.to_string() == target.id),
        AccessTargetKind::Connection => subscriptions
            .iter()
            .any(|listed| listed.connection == target.id),
        AccessTargetKind::Account => target.sub.as_deref().is_some_and(|connection| {
            subscriptions
                .iter()
                .any(|listed| listed.connection == connection)
        }),
        AccessTargetKind::PlatformOp => target.sub.as_deref().is_some_and(|connection| {
            subscriptions
                .iter()
                .any(|listed| listed.connection == connection)
        }),
        AccessTargetKind::Member | AccessTargetKind::Message | AccessTargetKind::File => {
            target.sub.as_deref().is_some_and(|room| {
                subscriptions
                    .iter()
                    .any(|listed| listed.to_string() == room)
            })
        }
        AccessTargetKind::PluginTool => !subscriptions.is_empty(),
    }
}

/// 细粒度权限串匹配语义（用户裁定）：精确匹配，或末段单层通配。
///
/// `qq:group.mute` 只匹配自身；`qq:group.*` 匹配 `qq:group.mute` 与
/// `qq:group.file.list`（通配整段结尾），但不匹配 `qq:app.close`。
/// 裸 `*`、居中 `*`、空段（`qq:`）一律不匹配——配置侧由
/// [`validate_permission_string`] 拒掉。
pub(crate) fn permission_matches(pattern: &str, required: &str) -> bool {
    if pattern == required {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) if !prefix.is_empty() => required.starts_with(prefix),
        _ => false,
    }
}

/// 校验规则里声明的权限串。合法形态：`<平台>:<op>[.<op>...]`，`*` 只允许
/// 作为最后一段且前面有非空前缀（即 `xxx.*`，不允许裸 `*` / `xxx*`）。
pub(crate) fn validate_permission_string(permission: &str) -> Result<(), String> {
    let trimmed = permission.trim();
    if trimmed.is_empty() || trimmed != permission {
        return Err("must be a non-empty permission string without surrounding whitespace".into());
    }
    let Some((platform, op)) = permission.split_once(':') else {
        return Err("must be <platform>:<op> (missing ':')".into());
    };
    if platform.is_empty() || platform.contains('*') || platform.contains('.') {
        return Err("platform segment must be non-empty without '*' or '.'".into());
    }
    if op.is_empty() {
        return Err("op segment must not be empty".into());
    }
    match op.find('*') {
        Some(pos) => {
            if pos != op.len() - 1 || pos == 0 {
                return Err("'*' is only allowed as the final segment with a non-empty prefix (e.g. group.*)".into());
            }
            // `group.*` 合法；`group*.list`（前缀缺段边界）非法。
            if !op[..pos].ends_with('.') && op[..pos].contains('.') {
                return Err("'*' must cover a whole trailing segment (e.g. group.*)".into());
            }
        }
        None => {}
    }
    Ok(())
}

pub(crate) fn scope_matches(rule: &GrantRule, query: &GrantQuery<'_>) -> bool {
    let target = query.target;
    let subscriptions = query.subscriptions;
    match rule.scope {
        GrantScope::Subscribed => target_is_reachable(target, subscriptions),
        GrantScope::Room => match rule.target.as_ref() {
            Some(wanted) if wanted.kind == AccessTargetKind::Room => match target.kind {
                AccessTargetKind::Room => target.id == wanted.id,
                AccessTargetKind::Member | AccessTargetKind::Message | AccessTargetKind::File => {
                    target.sub.as_deref().is_some_and(|room| room == wanted.id)
                }
                _ => false,
            },
            _ => false,
        },
        GrantScope::Connection => match rule.target.as_ref() {
            Some(wanted) if wanted.kind == AccessTargetKind::Connection => {
                target.connection_ref() == Some(wanted.id.as_str())
            }
            _ => false,
        },
        GrantScope::PlatformOp => match rule.target.as_ref() {
            Some(wanted) if wanted.kind == AccessTargetKind::PlatformOp => {
                let method_matches = query.platform_op == Some(wanted.id.as_str())
                    || (query
                        .platform_op
                        .is_some_and(|op| permission_matches(&wanted.id, op)));
                method_matches
                    && target.connection_ref() == wanted.connection_ref()
                    && rule_permissions_cover(rule, query.permissions)
            }
            _ => false,
        },
    }
}

/// 规则声明了 `permissions` 时，至少要覆盖一项本次要求的权限串；
/// 未声明（空）= 不过滤，粗粒度 method/connection 判定即生效。
fn rule_permissions_cover(rule: &GrantRule, required: &[String]) -> bool {
    if rule.permissions.is_empty() {
        return true;
    }
    required.iter().any(|needed| {
        rule.permissions
            .iter()
            .any(|p| permission_matches(p, needed))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CorePolicy, Policy};

    fn decide_policy(
        grants: &GrantsConfig,
        subject: GrantSubject,
        place: Place,
        capability: GrantCapability,
        target: &AccessTarget,
        subscriptions: &[RoomId],
    ) -> GrantDecision {
        decide_policy_from(
            grants,
            subject,
            place,
            capability,
            target,
            None,
            subscriptions,
        )
    }

    fn decide_policy_from(
        grants: &GrantsConfig,
        subject: GrantSubject,
        place: Place,
        capability: GrantCapability,
        target: &AccessTarget,
        origin: Option<&RoomId>,
        subscriptions: &[RoomId],
    ) -> GrantDecision {
        CorePolicy::new()
            .decide(&GrantQuery {
                subject,
                place,
                capability,
                actor: None,
                target,
                origin,
                subscriptions,
                platform_op: None,
                permissions: &[],
                policy_version: 7,
                grants,
            })
            .expect("CorePolicy 不应失败")
    }

    #[test]
    fn access_request_depends_on_origin_not_target() {
        let grants = GrantsConfig::default();
        let origin = RoomId::parse("qq:group/123").expect("房间");
        let target = RoomId::parse("qq:group/999").expect("房间");
        let subscribed = vec![origin.clone()];
        let decide = |subject, place, from: Option<&RoomId>, subs: &[RoomId]| {
            decide_policy_from(
                &grants,
                subject,
                place,
                GrantCapability::AccessRequest,
                &AccessTarget::room(&target),
                from,
                subs,
            )
        };
        let group = Place::Room(RoomScope::MultiParty);

        assert!(
            decide(GrantSubject::User, group, Some(&origin), &subscribed).allow,
            "群友可以从自己订阅的房间申请未订阅目标——目标可达性交给审批判"
        );
        assert!(
            !decide(GrantSubject::User, group, None, &subscribed).allow,
            "没有发起房间时 fail closed"
        );
        let outside = RoomId::parse("qq:group/777").expect("房间");
        assert!(
            !decide(GrantSubject::User, group, Some(&outside), &subscribed).allow,
            "发起房间不在本会话订阅内时拒绝"
        );
        assert!(
            !decide(GrantSubject::User, group, Some(&origin), &[]).allow,
            "会话没订阅任何房间时拒绝"
        );
        assert!(
            decide(
                GrantSubject::SystemAdmin,
                Place::Room(RoomScope::Direct),
                None,
                &subscribed
            )
            .allow,
            "system_admin 私信可以申请任意目标"
        );
        assert!(
            decide(
                GrantSubject::BotAdmin,
                Place::Room(RoomScope::Direct),
                None,
                &subscribed
            )
            .allow,
            "bot_admin 私信可以申请任意目标"
        );
    }

    #[test]
    fn subscribed_does_not_escape_the_sessions_reachable_targets() {
        let grants = config(vec![rule(
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantScope::Subscribed,
            None,
            &[GrantCapability::RoomRead],
        )]);
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let outside = RoomId::parse("qq:group/2").expect("房间");
        let decision = eval_access(
            &grants,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&outside),
            std::slice::from_ref(&inside),
        );
        assert!(
            !decision.allow,
            "scope=subscribed 不能越过会话可达范围：{decision:?}"
        );
        let allowed = eval_access(
            &grants,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(allowed.allow, "可达目标应被 scope=subscribed 放行");
    }

    #[test]
    fn scope_any_is_rejected_with_a_migration_hint() {
        let text = r#"
            id = "test-scope-any"
            subject = "user"
            place = "group"
            scope = "any"
            capability = ["room_read"]
        "#;
        let error = toml::from_str::<GrantRule>(text).expect_err("any 必须被拒绝");
        let message = error.to_string();
        eprintln!("scope=any 启动报错：{message}");
        assert!(message.contains("any"), "{message}");
        assert!(message.contains("subscribed"), "{message}");
        assert!(message.contains("removed"), "{message}");
    }

    fn eval_access(
        grants: &GrantsConfig,
        subject: GrantSubject,
        place: Place,
        capability: GrantCapability,
        target: &AccessTarget,
        subscriptions: &[RoomId],
    ) -> GrantDecision {
        decide_policy(grants, subject, place, capability, target, subscriptions)
    }

    fn eval_tool(
        grants: &GrantsConfig,
        subject: GrantSubject,
        place: Place,
        target: &AccessTarget,
        subscriptions: &[RoomId],
    ) -> GrantDecision {
        decide_policy(
            grants,
            subject,
            place,
            GrantCapability::Plugin,
            target,
            subscriptions,
        )
    }

    fn rule(
        subject: GrantSubject,
        place: Place,
        scope: GrantScope,
        target: Option<&str>,
        capability: &[GrantCapability],
    ) -> GrantRule {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = format!(
            "test-rule-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let target = target.map(|text| match scope {
            GrantScope::Connection => AccessTarget::connection(text),
            _ => AccessTarget::room(&RoomId::parse(text).expect("房间")),
        });
        GrantRule {
            id,
            subject,
            place,
            scope,
            target,
            capability: capability.to_vec(),
            permissions: Vec::new(),
        }
    }

    fn config(rules: Vec<GrantRule>) -> GrantsConfig {
        GrantsConfig {
            rules,
            deny: Vec::new(),
            bot_admin: None,
            system_admin: None,
            rooms: Default::default(),
            accounts: Default::default(),
            ..Default::default()
        }
    }

    #[test]
    fn default_is_subscribed_and_denies_outside() {
        let grants = GrantsConfig::default();
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let outside = RoomId::parse("qq:group/2").expect("房间");
        let decision = eval_access(
            &grants,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow);
        assert!(!decision.audited(), "普通订阅评估不审计");

        let decision = eval_access(
            &grants,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&outside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow);
        assert!(!decision.audited());
    }

    #[test]
    fn deny_is_audited_and_plain_allow_is_not() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let outside = RoomId::parse("qq:group/2").expect("房间");
        let grants = GrantsConfig::default();

        let deny = eval_access(
            &grants,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&outside),
            std::slice::from_ref(&inside),
        );
        assert!(!deny.allow);
        assert!(deny.should_audit(), "普通主体越权必须留一条 deny 审计");
        let line = deny.audit_line(
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &outside.to_string(),
            "main",
        );
        assert_eq!(line.matches("[access]").count(), 1);
        assert!(line.contains("result=deny"));
        assert!(line.contains("basis=base:not_subscribed"));
        assert!(line.contains("target=qq:group/2"));
        assert!(line.contains("session=main"));

        let allow = eval_access(
            &grants,
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(allow.allow);
        assert!(!allow.should_audit(), "订阅内普通读写不产生审计");
    }

    #[test]
    fn audit_target_is_truncated() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let target = format!("qq:group/{}", "x".repeat(200));
        let decision = eval_access(
            &GrantsConfig::default(),
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        let line = decision.audit_line(
            GrantSubject::User,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomRead,
            &target,
            "main",
        );
        let truncated: String = target.chars().take(AUDIT_TARGET_MAX_CHARS).collect();
        assert!(line.contains(&format!("target={truncated}…")));
    }

    #[test]
    fn partial_match_does_not_deny_floor() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let grants = config(vec![rule(
            GrantSubject::BotAdmin,
            Place::Room(RoomScope::Direct),
            GrantScope::Subscribed,
            None,
            &[GrantCapability::RoomWrite],
        )]);
        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            Place::Room(RoomScope::Direct),
            GrantCapability::RoomWrite,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow);
        assert!(decision.basis.starts_with("base:subscription"));
        assert!(decision.basis.contains("rule:"));
        assert!(decision.audited());

        // 这就是 rule:6 事故的回归：规则匹配但没列出 RoomRead，不能把 floor 压掉。
        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            Place::Room(RoomScope::Direct),
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow, "部分匹配不得构成 deny");
        assert_eq!(decision.basis, "base:subscription");
        assert!(!decision.audited(), "纯 floor allow 不审计");
    }

    #[test]
    fn bot_admin_is_private_and_rule_scoped() {
        let grants = GrantsConfig {
            rules: vec![rule(
                GrantSubject::BotAdmin,
                Place::Room(RoomScope::Direct),
                GrantScope::Subscribed,
                None,
                &[GrantCapability::RoomWrite, GrantCapability::Plugin],
            )],
            deny: Vec::new(),
            bot_admin: Some(BotAdminGrant {
                accounts: vec![GrantAccount {
                    connection: "qq".to_owned(),
                    account: "10001".to_owned(),
                }],
            }),
            system_admin: None,
            rooms: Default::default(),
            accounts: Default::default(),
            ..Default::default()
        };
        assert_eq!(
            grants.subject_for("qq", Some("10001")),
            GrantSubject::BotAdmin
        );
        assert_eq!(
            grants.subject_for("qq", Some("10001")),
            GrantSubject::BotAdmin,
            "主体不变；场所维度会阻止私信规则命中群聊"
        );
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            Place::Room(RoomScope::MultiParty),
            GrantCapability::RoomWrite,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow, "群聊里没有 bot_admin 规则时退回订阅边界");
        assert!(!decision.audited());
    }

    #[test]
    fn system_subject_has_no_human_exemption() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let outside = RoomId::parse("qq:group/2").expect("房间");
        let grants = GrantsConfig::default();
        assert_eq!(grants.subject_for("qq", None), GrantSubject::System);
        let decision = eval_access(
            &grants,
            GrantSubject::System,
            Place::None,
            GrantCapability::RoomRead,
            &AccessTarget::room(&outside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow, "无主体轮次不继承任何人类豁免");
        assert!(!decision.audited());
    }

    #[test]
    fn admin_without_plugin_grant_fails_closed() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let grants = GrantsConfig::default();
        let decision = eval_tool(
            &grants,
            GrantSubject::BotAdmin,
            Place::Room(RoomScope::Direct),
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow);
        assert_eq!(decision.basis, "admin:no_plugin_grant");
        assert!(decision.audited());
    }

    #[test]
    fn rule_order_is_irrelevant_and_partial_matches_do_not_deny() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let place = Place::Room(RoomScope::MultiParty);
        let grants = config(vec![
            rule(
                GrantSubject::User,
                place,
                GrantScope::Subscribed,
                None,
                &[GrantCapability::RoomRead],
            ),
            rule(
                GrantSubject::User,
                place,
                GrantScope::Subscribed,
                None,
                &[GrantCapability::RoomWrite],
            ),
        ]);
        let decision = eval_access(
            &grants,
            GrantSubject::User,
            place,
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow, "并集语义：RoomRead 仍被授予");
        assert!(decision.basis.contains("rule:"));
        assert!(
            !decision.basis.contains("deny:"),
            "缺能力的匹配规则不得构成 deny: {}",
            decision.basis
        );
    }

    #[test]
    fn explicit_deny_subtracts_floor_and_wins() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let place = Place::Room(RoomScope::MultiParty);
        let mut grants = config(vec![rule(
            GrantSubject::User,
            place,
            GrantScope::Subscribed,
            None,
            &[GrantCapability::RoomRead],
        )]);
        grants.deny.push(rule(
            GrantSubject::User,
            place,
            GrantScope::Subscribed,
            None,
            &[GrantCapability::RoomRead],
        ));
        let decision = eval_access(
            &grants,
            GrantSubject::User,
            place,
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow, "显式 deny 必须减掉 floor 和 rule");
        assert!(decision.basis.contains("deny:"), "{}", decision.basis);
        assert!(decision.audited());
    }

    #[test]
    fn decision_carries_rule_id_and_policy_version() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let place = Place::Room(RoomScope::MultiParty);
        let grants = config(vec![rule(
            GrantSubject::User,
            place,
            GrantScope::Subscribed,
            None,
            &[GrantCapability::RoomRead],
        )]);
        let decision = eval_access(
            &grants,
            GrantSubject::User,
            place,
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(
            decision.rule_id.starts_with("rule:"),
            "{}",
            decision.rule_id
        );
        assert_eq!(decision.policy_version, 7);
    }

    #[test]
    fn grant_rule_id_is_required_and_unique() {
        let missing = r#"
            subject = "user"
            place = "group"
            scope = "subscribed"
            capability = ["room_read"]
        "#;
        let error = toml::from_str::<GrantRule>(missing).expect_err("规则必须带 id");
        assert!(error.to_string().contains("id"), "{error}");

        let duplicate = r#"
            [[rules]]
            id = "same"
            subject = "user"
            place = "group"
            scope = "subscribed"
            capability = ["room_read"]
            [[deny]]
            id = "same"
            subject = "user"
            place = "group"
            scope = "subscribed"
            capability = ["room_write"]
        "#;
        let grants: GrantsConfig = toml::from_str(duplicate).expect("应能解析");
        let error = grants.validate().expect_err("重复 id 必须拒绝");
        assert!(error.contains("duplicate grant rule id"), "{error}");
    }

    fn role(id: &str, capabilities: Vec<GrantCapability>) -> PolicyRole {
        PolicyRole {
            id: id.to_owned(),
            name: id.to_owned(),
            description: String::new(),
            builtin: false,
            capabilities,
        }
    }

    fn query<'a>(
        grants: &'a GrantsConfig,
        subject: GrantSubject,
        capability: GrantCapability,
        target: &'a AccessTarget,
        actor: Option<&'a Actor<'a>>,
    ) -> GrantQuery<'a> {
        GrantQuery {
            subject,
            place: Place::Room(RoomScope::MultiParty),
            capability,
            actor,
            target,
            origin: None,
            subscriptions: &[],
            platform_op: None,
            permissions: &[],
            policy_version: 1,
            grants,
        }
    }

    #[test]
    fn role_assignment_grants_capability_by_subject() {
        let mut grants = GrantsConfig::default();
        grants
            .roles
            .push(role("moderator", vec![GrantCapability::RoomWrite]));
        grants.assignments.push(PolicyAssignment {
            id: "a1".to_owned(),
            subject_kind: AssignmentSubjectKind::Subject,
            subject_id: "user".to_owned(),
            role_id: "moderator".to_owned(),
            scope_kind: AssignmentScopeKind::Global,
            scope_id: None,
            capability_snapshot: vec![GrantCapability::RoomWrite],
            role_snapshot: RoleSnapshot {
                id: "moderator".to_owned(),
                name: "moderator".to_owned(),
                capabilities: vec![GrantCapability::RoomWrite],
            },
            needs_review: false,
        });
        let room = RoomId::parse("qq:group/1").unwrap();
        let target = AccessTarget::room(&room);
        let decision = CorePolicy::new()
            .decide(&query(
                &grants,
                GrantSubject::User,
                GrantCapability::RoomWrite,
                &target,
                None,
            ))
            .unwrap();
        assert!(decision.allow, "{decision:?}");
        assert!(decision.basis.contains("role:moderator#a1"), "{decision:?}");
    }

    #[test]
    fn account_assignment_is_scoped_to_connection_and_room() {
        let mut grants = GrantsConfig::default();
        grants
            .roles
            .push(role("reader", vec![GrantCapability::RoomRead]));
        grants.assignments.push(PolicyAssignment {
            id: "a2".to_owned(),
            subject_kind: AssignmentSubjectKind::Account,
            subject_id: "qq:42".to_owned(),
            role_id: "reader".to_owned(),
            scope_kind: AssignmentScopeKind::Room,
            scope_id: Some("qq:group/1".to_owned()),
            capability_snapshot: vec![GrantCapability::RoomRead],
            role_snapshot: RoleSnapshot {
                id: "reader".to_owned(),
                name: "reader".to_owned(),
                capabilities: vec![GrantCapability::RoomRead],
            },
            needs_review: false,
        });
        let room = RoomId::parse("qq:group/1").unwrap();
        let target = AccessTarget::room(&room);
        let actor = Actor {
            connection: "qq",
            account: Some("42"),
            room: Some(&room),
            member: Some("42"),
        };
        assert!(
            CorePolicy::new()
                .decide(&query(
                    &grants,
                    GrantSubject::User,
                    GrantCapability::RoomRead,
                    &target,
                    Some(&actor)
                ))
                .unwrap()
                .allow
        );

        let other = RoomId::parse("qq:group/2").unwrap();
        let other_target = AccessTarget::room(&other);
        let other_actor = Actor {
            connection: "qq",
            account: Some("42"),
            room: Some(&other),
            member: Some("42"),
        };
        assert!(
            !CorePolicy::new()
                .decide(&query(
                    &grants,
                    GrantSubject::User,
                    GrantCapability::RoomRead,
                    &other_target,
                    Some(&other_actor)
                ))
                .unwrap()
                .allow,
            "同一个账号在别的房间不应命中"
        );
    }

    #[test]
    fn assignment_uses_snapshot_not_live_role_capabilities() {
        let mut grants = GrantsConfig::default();
        grants
            .roles
            .push(role("reader", vec![GrantCapability::RoomRead]));
        grants.assignments.push(PolicyAssignment {
            id: "a3".to_owned(),
            subject_kind: AssignmentSubjectKind::Subject,
            subject_id: "user".to_owned(),
            role_id: "reader".to_owned(),
            scope_kind: AssignmentScopeKind::Global,
            scope_id: None,
            capability_snapshot: vec![GrantCapability::RoomRead],
            role_snapshot: RoleSnapshot {
                id: "reader".to_owned(),
                name: "reader".to_owned(),
                capabilities: vec![GrantCapability::RoomRead],
            },
            needs_review: false,
        });
        // 角色后来扩容，但旧指派仍按快照。
        grants.roles[0]
            .capabilities
            .push(GrantCapability::RoomWrite);
        let room = RoomId::parse("qq:group/1").unwrap();
        let target = AccessTarget::room(&room);
        assert!(
            !CorePolicy::new()
                .decide(&query(
                    &grants,
                    GrantSubject::User,
                    GrantCapability::RoomWrite,
                    &target,
                    None
                ))
                .unwrap()
                .allow,
            "角色扩容不能追认旧指派"
        );
    }

    fn platform_rule(
        id: &str,
        target_id: &str,
        permissions: &[&str],
        capability: GrantCapability,
    ) -> GrantRule {
        GrantRule {
            id: id.to_owned(),
            subject: GrantSubject::User,
            place: Place::Room(RoomScope::MultiParty),
            scope: GrantScope::PlatformOp,
            target: Some(AccessTarget::platform_op("qq", target_id)),
            capability: vec![capability],
            permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn platform_query<'a>(
        grants: &'a GrantsConfig,
        capability: GrantCapability,
        target: &'a AccessTarget,
        op: &'a str,
        permissions: &'a [String],
    ) -> GrantQuery<'a> {
        GrantQuery {
            subject: GrantSubject::User,
            place: Place::Room(RoomScope::MultiParty),
            capability,
            actor: None,
            target,
            origin: None,
            subscriptions: &[],
            platform_op: Some(op),
            permissions,
            policy_version: 1,
            grants,
        }
    }

    #[test]
    fn permission_matches_is_exact_or_trailing_wildcard() {
        // 精确匹配
        assert!(permission_matches("qq:group.mute", "qq:group.mute"));
        // 末段通配：覆盖结尾所有层级（用户裁定「精确匹配 + 单层尾通配」）
        assert!(permission_matches("qq:group.*", "qq:group.mute"));
        assert!(permission_matches("qq:group.*", "qq:group.file.list"));
        // 平台/前缀不同不匹配
        assert!(!permission_matches("qq:group.*", "qq:app.close"));
        assert!(!permission_matches("qq:group.*", "wecom:group.mute"));
        // 裸 `*`、居中 `*` 不构成通配（配置侧 validate 也会拒）
        assert!(!permission_matches("*", "qq:group.mute"));
        assert!(!permission_matches("qq:*.mute", "qq:group.mute"));
        assert!(!permission_matches("qq:group.mute", "qq:group.file.list"));
    }

    #[test]
    fn validate_permission_string_accepts_only_wellformed() {
        for ok in ["qq:group.mute", "qq:group.*", "wecom:corp.send"] {
            assert!(validate_permission_string(ok).is_ok(), "{ok} 应合法");
        }
        for bad in [
            "",               // 空
            " qq:x ",         // 首尾空白
            "*",              // 裸 *
            "qq:",            // 空 op
            ":group.mute",    // 空平台
            "qq.group.mute",  // 缺冒号
            "q*q:x",          // 平台段带 *
            "qq:*",           // op 整段是 *（等价裸 *）
            "qq:gr*oup",      // 居中 *
            "qq:group*.list", // * 不在末段
            "qq:group.*x",    // * 之后还有内容
        ] {
            assert!(validate_permission_string(bad).is_err(), "{bad} 应非法");
        }
    }

    #[test]
    fn rule_permissions_cover_requires_declared_grant() {
        // 未声明 permissions（空）= 不过滤：required 空与非空都放行。
        let no_filter = platform_rule("r0", "group.mute", &[], GrantCapability::Destructive);
        assert!(rule_permissions_cover(&no_filter, &[]));
        assert!(rule_permissions_cover(
            &no_filter,
            &["qq:group.mute".to_owned()]
        ));
        // 声明了 permissions：required 里至少要有一项被覆盖。
        let mute = platform_rule(
            "r1",
            "group.mute",
            &["qq:group.*"],
            GrantCapability::Destructive,
        );
        assert!(rule_permissions_cover(&mute, &["qq:group.mute".to_owned()]));
        assert!(!rule_permissions_cover(&mute, &["qq:app.close".to_owned()]));
        // required 为空时，声明过的规则没有可覆盖的对象，不能放行。
        assert!(!rule_permissions_cover(&mute, &[]));
    }

    #[test]
    fn platform_op_rule_grants_by_wildcard_target() {
        let grants = config(vec![platform_rule(
            "qq-ops",
            "group.*",
            &[],
            GrantCapability::Destructive,
        )]);
        let target = AccessTarget::platform_op("qq", "group.mute");
        let required = vec!["qq:group.mute".to_owned()];
        let decision = CorePolicy::new()
            .decide(&platform_query(
                &grants,
                GrantCapability::Destructive,
                &target,
                "group.mute",
                &required,
            ))
            .expect("CorePolicy 不应失败");
        assert!(decision.allow, "{decision:?}");
        // 目标通配不跨段：app.close 不命中 group.*，落回 deny。
        let other = AccessTarget::platform_op("qq", "app.close");
        let other_required = vec!["qq:app.close".to_owned()];
        let decision = CorePolicy::new()
            .decide(&platform_query(
                &grants,
                GrantCapability::Destructive,
                &other,
                "app.close",
                &other_required,
            ))
            .expect("CorePolicy 不应失败");
        assert!(!decision.allow, "{decision:?}");
    }

    #[test]
    fn platform_op_rule_permissions_gate_filters_unrelated_ops() {
        // 目标 id 命中（group.mute），但规则声明的权限串覆盖不了 required，
        // 不能放行——permissions 是目标匹配之外的第二道过滤。
        let grants = config(vec![platform_rule(
            "qq-mute",
            "group.mute",
            &["qq:other.*"],
            GrantCapability::Destructive,
        )]);
        let target = AccessTarget::platform_op("qq", "group.mute");
        let denied = vec!["qq:group.mute".to_owned()];
        let decision = CorePolicy::new()
            .decide(&platform_query(
                &grants,
                GrantCapability::Destructive,
                &target,
                "group.mute",
                &denied,
            ))
            .expect("CorePolicy 不应失败");
        assert!(!decision.allow, "{decision:?}");
        // 换成声明权限串能覆盖的 required 才放行。
        let allowed = vec!["qq:other.export".to_owned()];
        let decision = CorePolicy::new()
            .decide(&platform_query(
                &grants,
                GrantCapability::Destructive,
                &target,
                "group.mute",
                &allowed,
            ))
            .expect("CorePolicy 不应失败");
        assert!(decision.allow, "{decision:?}");
    }

    #[test]
    fn grants_validate_rejects_malformed_permission_strings() {
        let grants = config(vec![platform_rule(
            "bad-perm",
            "group.mute",
            &["*"],
            GrantCapability::Destructive,
        )]);
        let error = grants.validate().expect_err("裸 * 必须被配置校验拒绝");
        assert!(error.contains("permissions entry"), "{error}");
    }
}
