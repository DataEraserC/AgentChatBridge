//! 访问授权的内置实现：四元组 + 目标。
//!
//! 这一层只做**授权叠加**：命中配置里的常态授予时给出 `allow`；没命中时退回
//! 现有的订阅边界。它不是第二套房间策略，也不替代
//! `MethodSpec.risk` + `[connections.policy.risk]` 风险策略。
//!
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
            AccessTargetKind::Account => self.sub.as_deref(),
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
        }
    }
}

impl fmt::Display for AccessTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.kind.as_str(), self.id)
    }
}

/// 授权主体。`System` 表示没有人类触发者的轮次（clock/自发唤醒）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
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
}

impl<'de> Deserialize<'de> for GrantScope {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.trim() {
            "subscribed" => Ok(Self::Subscribed),
            "room" => Ok(Self::Room),
            "connection" => Ok(Self::Connection),
            "any" => Err(serde::de::Error::custom(
                "scope \"any\" has been removed; use \"subscribed\" instead \
                 (the two have identical reachability semantics)",
            )),
            other => Err(serde::de::Error::custom(format!(
                "unknown scope {other:?}; expected one of \"subscribed\", \"room\", \"connection\""
            ))),
        }
    }
}

/// 能力枚举。封闭值域，未知值在配置解析期报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantCapability {
    RoomRead,
    RoomWrite,
    ConnectionRead,
    Platform,
    Destructive,
    /// 最高风险档。只由 `access_request` 票据携带，配置里的常驻授予一律拒绝。
    Critical,
    /// 读账号资料。与取账号凭证的 `account_secret` 分开。
    AccountRead,
    /// 取账号凭证。与 `critical` 同级，不可常驻授予，只能走票据。
    AccountSecret,
    /// 能不能发起 `access_request`。与房间读写能力正交。
    AccessRequest,
    Plugin,
}

impl GrantCapability {
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
        }
    }
}

/// 一个常态授予规则。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRule {
    pub subject: GrantSubject,
    pub place: Place,
    pub scope: GrantScope,
    #[serde(default)]
    pub target: Option<AccessTarget>,
    pub capability: Vec<GrantCapability>,
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

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantAccount {
    pub connection: String,
    pub account: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotAdminGrant {
    pub accounts: Vec<GrantAccount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemAdminGrant {
    pub connection: String,
    pub account: String,
}

/// `[grants]` 配置。未知字段由 `deny_unknown_fields` 在解析期拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsConfig {
    #[serde(default)]
    pub rules: Vec<GrantRule>,
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
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.capability.is_empty() {
                return Err(format!(
                    "grants.rules[{index}] capability must not be empty"
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for capability in &rule.capability {
                if matches!(
                    *capability,
                    GrantCapability::Critical | GrantCapability::AccountSecret
                ) {
                    return Err(format!(
                        "grants.rules[{index}] capability {} cannot be granted by config; \
                         it requires an access_request ticket approved in private",
                        capability.as_str()
                    ));
                }
                if !seen.insert(*capability) {
                    return Err(format!(
                        "grants.rules[{index}] capability declared twice: {capability:?}"
                    ));
                }
            }
            match rule.scope {
                GrantScope::Subscribed => {
                    if rule.target.is_some() {
                        return Err(format!(
                            "grants.rules[{index}] target is only allowed for scope room/connection"
                        ));
                    }
                }
                GrantScope::Room => {
                    let Some(target) = rule.target.as_ref() else {
                        return Err(format!("grants.rules[{index}] scope room requires target"));
                    };
                    if target.kind != AccessTargetKind::Room || target.id.trim().is_empty() {
                        return Err(format!(
                            "grants.rules[{index}] scope room target must be kind = \"room\" with a room id"
                        ));
                    }
                    RoomId::parse(target.id.trim()).map_err(|error| {
                        format!("grants.rules[{index}] target is not a room: {error}")
                    })?;
                }
                GrantScope::Connection => {
                    let Some(target) = rule.target.as_ref() else {
                        return Err(format!(
                            "grants.rules[{index}] scope connection requires target"
                        ));
                    };
                    if target.kind != AccessTargetKind::Connection || target.id.trim().is_empty() {
                        return Err(format!(
                            "grants.rules[{index}] scope connection target must be kind = \"connection\" with a connection id"
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

/// 一次评估的目标。
/// 一次授权评估的全部输入。把参数收成一个结构，避免 evaluator 接口继续膨胀。
#[derive(Debug, Clone, Copy)]
pub struct GrantQuery<'a> {
    pub subject: GrantSubject,
    pub place: Place,
    pub capability: GrantCapability,
    pub target: &'a AccessTarget,
    pub subscriptions: &'a [RoomId],
}

/// 评估结果。豁免（basis 非 `base:`）与任何拒绝都要写审计；普通 base allow 不写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantDecision {
    pub allow: bool,
    pub basis: String,
    pub reason: String,
}

/// 审计行里 target 的最大字符数；房间/连接 id 再长也只截到这里。
const AUDIT_TARGET_MAX_CHARS: usize = 120;

impl GrantDecision {
    /// 豁免（命中规则的 allow/deny）和所有 deny 都要审计；普通 base allow 不审计。
    pub fn should_audit(&self) -> bool {
        self.audited() || !self.allow
    }

    pub fn audited(&self) -> bool {
        !self.basis.starts_with("base:")
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

    fn allow(basis: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            allow: true,
            basis: basis.into(),
            reason: reason.into(),
        }
    }

    fn deny(basis: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            basis: basis.into(),
            reason: reason.into(),
        }
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

/// 房间/连接访问评估。调用方已经排除了外部 provider。
pub fn evaluate_access(grants: &GrantsConfig, query: &GrantQuery<'_>) -> GrantDecision {
    evaluate_rules(grants, query).unwrap_or_else(|| base_access(query))
}

/// 平台动作的额外能力检查（`platform` / `destructive`）。
/// 返回 `None` = 没有任何授予，沿用已通过的基础访问。
pub fn evaluate_platform(grants: &GrantsConfig, query: &GrantQuery<'_>) -> Option<GrantDecision> {
    evaluate_rules(grants, query)
}

/// 票据申请能力的评估。默认口径：
/// - `system_admin` / `bot_admin` 在私信或控制台可以申请任意目标；
/// - 其它主体只能申请本会话已订阅的目标（`base:subscription`）；
/// - 显式规则可以放开跨目标（`scope = room` / `connection` 写 typed target）；
///   `scope = subscribed` 只覆盖本会话可达目标，不跨范围。
pub fn evaluate_access_request(grants: &GrantsConfig, query: &GrantQuery<'_>) -> GrantDecision {
    if let Some(decision) = evaluate_rules(grants, query) {
        return decision;
    }
    match (query.subject, query.place) {
        (GrantSubject::SystemAdmin | GrantSubject::BotAdmin, Place::Room(RoomScope::Direct))
        | (GrantSubject::SystemAdmin | GrantSubject::BotAdmin, Place::Console) => {
            GrantDecision::allow(
                "base:admin_private",
                "admin may request tickets for any target from private/console",
            )
        }
        _ => base_access(query),
    }
}

/// 插件工具能力检查。普通主体沿用基础放行；管理员主体若配置了匹配规则，
/// 规则里的 capability 决定是否允许 `plugin`。
pub fn evaluate_tool(grants: &GrantsConfig, query: &GrantQuery<'_>) -> GrantDecision {
    if matches!(
        query.subject,
        GrantSubject::SystemAdmin | GrantSubject::BotAdmin
    ) {
        if let Some(decision) = evaluate_rules(grants, query) {
            return decision;
        }
        return GrantDecision::deny(
            "admin:no_plugin_grant",
            "admin subject has no matching plugin grant",
        );
    }
    GrantDecision::allow("base:plugin", "ordinary subject uses scope check")
}

fn evaluate_rules(grants: &GrantsConfig, query: &GrantQuery<'_>) -> Option<GrantDecision> {
    let mut denied: Option<(usize, String)> = None;
    for (index, rule) in grants.rules.iter().enumerate() {
        if rule.subject != query.subject || rule.place != query.place {
            continue;
        }
        if !scope_matches(rule, query.target, query.subscriptions) {
            continue;
        }
        if rule.capability.contains(&query.capability) {
            return Some(GrantDecision::allow(
                format!("rule:{index}"),
                format!(
                    "{} {} {}",
                    query.subject.as_str(),
                    query.place.as_str(),
                    rule.scope_name()
                ),
            ));
        }
        denied = Some((
            index,
            format!(
                "rule:{index} does not grant {:?} for {} {}",
                query.capability,
                query.subject.as_str(),
                query.place.as_str()
            ),
        ));
    }
    denied.map(|(index, reason)| GrantDecision::deny(format!("rule:{index}"), reason))
}

fn base_access(query: &GrantQuery<'_>) -> GrantDecision {
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

fn scope_matches(rule: &GrantRule, target: &AccessTarget, subscriptions: &[RoomId]) -> bool {
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
    }
}

impl GrantRule {
    fn scope_name(&self) -> &'static str {
        match self.scope {
            GrantScope::Subscribed => "subscribed",
            GrantScope::Room => "room",
            GrantScope::Connection => "connection",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_request_defaults_to_subscriptions_except_admin_private() {
        let grants = GrantsConfig::default();
        let target = RoomId::parse("qq:group/999").expect("房间");
        let subscriptions = Vec::new();
        let decide = |subject, place, subscriptions: &[RoomId]| {
            evaluate_access_request(
                &grants,
                &GrantQuery {
                    subject,
                    place,
                    capability: GrantCapability::AccessRequest,
                    target: &AccessTarget::room(&target),
                    subscriptions,
                },
            )
        };
        assert!(
            !decide(
                GrantSubject::User,
                Place::Room(RoomScope::MultiParty),
                &subscriptions
            )
            .allow,
            "群友默认不能申请未订阅目标"
        );
        assert!(
            decide(
                GrantSubject::SystemAdmin,
                Place::Room(RoomScope::Direct),
                &subscriptions
            )
            .allow,
            "system_admin 私信可以申请任意目标"
        );
        assert!(
            decide(
                GrantSubject::BotAdmin,
                Place::Room(RoomScope::Direct),
                &subscriptions
            )
            .allow,
            "bot_admin 私信可以申请任意目标"
        );
        let subscribed = vec![target.clone()];
        assert!(
            decide(
                GrantSubject::User,
                Place::Room(RoomScope::MultiParty),
                &subscribed
            )
            .allow,
            "已订阅目标默认可申请"
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
        evaluate_access(
            grants,
            &GrantQuery {
                subject,
                place,
                capability,
                target,
                subscriptions,
            },
        )
    }

    fn eval_tool(
        grants: &GrantsConfig,
        subject: GrantSubject,
        place: Place,
        target: &AccessTarget,
        subscriptions: &[RoomId],
    ) -> GrantDecision {
        evaluate_tool(
            grants,
            &GrantQuery {
                subject,
                place,
                capability: GrantCapability::Plugin,
                target,
                subscriptions,
            },
        )
    }

    fn rule(
        subject: GrantSubject,
        place: Place,
        scope: GrantScope,
        target: Option<&str>,
        capability: &[GrantCapability],
    ) -> GrantRule {
        let target = target.map(|text| match scope {
            GrantScope::Connection => AccessTarget::connection(text),
            _ => AccessTarget::room(&RoomId::parse(text).expect("房间")),
        });
        GrantRule {
            subject,
            place,
            scope,
            target,
            capability: capability.to_vec(),
        }
    }

    fn config(rules: Vec<GrantRule>) -> GrantsConfig {
        GrantsConfig {
            rules,
            bot_admin: None,
            system_admin: None,
            rooms: Default::default(),
            accounts: Default::default(),
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
    fn rule_grants_only_listed_capability_and_audits() {
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
        assert_eq!(decision.basis, "rule:0");
        assert!(decision.audited());

        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            Place::Room(RoomScope::Direct),
            GrantCapability::RoomRead,
            &AccessTarget::room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow, "同一规则缺能力要拒绝");
        assert_eq!(decision.basis, "rule:0");
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
            bot_admin: Some(BotAdminGrant {
                accounts: vec![GrantAccount {
                    connection: "qq".to_owned(),
                    account: "10001".to_owned(),
                }],
            }),
            system_admin: None,
            rooms: Default::default(),
            accounts: Default::default(),
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
}
