//! 访问授权的内置实现：四元组 + 目标。
//!
//! 这一层只做**授权叠加**：命中配置里的常态授予时给出 `allow`；没命中时退回
//! 现有的订阅边界。它不是第二套房间策略，也不替代
//! `MethodSpec.risk = Destructive` + `allow_destructive` 闸门。
//!
//! 不做旧 provider 的兼容翻译：没有用户需要平滑迁移，多一层翻译只会让旧配置
//! 悄悄多出权限。旧键留在配置里会在启动期直接报错。
//!
//! 票据/审批流不在本模块：阶段二单独实现。

use std::fmt;

use serde::Deserialize;

use crate::RoomId;

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

/// 本轮发生的场所。它是独立维度：同一个账号在私信和群里的权限不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantPlace {
    Private,
    Group,
    Console,
    None,
}

impl GrantPlace {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
            Self::Console => "console",
            Self::None => "none",
        }
    }
}

/// 能力范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    Any,
    Subscribed,
    Room,
    Connection,
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
            Self::Plugin => "plugin",
        }
    }
}

/// 一个常态授予规则。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRule {
    pub subject: GrantSubject,
    pub place: GrantPlace,
    pub scope: GrantScope,
    #[serde(default)]
    pub target: Option<String>,
    pub capability: Vec<GrantCapability>,
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
                if !seen.insert(*capability) {
                    return Err(format!(
                        "grants.rules[{index}] capability declared twice: {capability:?}"
                    ));
                }
            }
            match rule.scope {
                GrantScope::Any | GrantScope::Subscribed => {
                    if rule.target.is_some() {
                        return Err(format!(
                            "grants.rules[{index}] target is only allowed for scope room/connection"
                        ));
                    }
                }
                GrantScope::Room => {
                    let target = rule.target.as_deref().unwrap_or("").trim();
                    if target.is_empty() {
                        return Err(format!("grants.rules[{index}] scope room requires target"));
                    }
                    RoomId::parse(target).map_err(|error| {
                        format!("grants.rules[{index}] target is not a room: {error}")
                    })?;
                }
                GrantScope::Connection => {
                    if rule
                        .target
                        .as_deref()
                        .map(str::trim)
                        .is_none_or(str::is_empty)
                    {
                        return Err(format!(
                            "grants.rules[{index}] scope connection requires target"
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn subject_for(
        &self,
        connection: &str,
        sender: Option<&str>,
        place: GrantPlace,
    ) -> GrantSubject {
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
        let _ = place;
        GrantSubject::User
    }

    pub fn place_of(origin: &RoomId, has_human: bool) -> GrantPlace {
        if !has_human {
            return GrantPlace::None;
        }
        match origin.path.first().map(String::as_str) {
            Some("private") => GrantPlace::Private,
            Some("group") => GrantPlace::Group,
            _ => GrantPlace::Console,
        }
    }
}

/// 一次评估的目标。
#[derive(Debug, Clone, Copy)]
pub enum GrantTarget<'a> {
    Room(&'a RoomId),
    Connection(&'a str),
}

impl GrantTarget<'_> {
    pub fn connection(&self) -> &str {
        match self {
            Self::Room(room) => &room.connection,
            Self::Connection(connection) => connection,
        }
    }
}

/// 一次授权评估的全部输入。把参数收成一个结构，避免 evaluator 接口继续膨胀。
#[derive(Debug, Clone, Copy)]
pub struct GrantQuery<'a> {
    pub subject: GrantSubject,
    pub place: GrantPlace,
    pub capability: GrantCapability,
    pub target: GrantTarget<'a>,
    pub subscriptions: &'a [RoomId],
}

/// 评估结果。`basis` 非 `base:` 时调用方应写审计日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantDecision {
    pub allow: bool,
    pub basis: String,
    pub reason: String,
}

impl GrantDecision {
    pub fn audited(&self) -> bool {
        !self.basis.starts_with("base:")
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
    let allowed = match query.target {
        GrantTarget::Room(room) => query.subscriptions.iter().any(|listed| listed == room),
        GrantTarget::Connection(connection) => query
            .subscriptions
            .iter()
            .any(|listed| listed.connection == connection),
    };
    if allowed {
        GrantDecision::allow(
            "base:subscription",
            format!("subscribed target for {:?}", query.capability),
        )
    } else {
        GrantDecision::deny(
            "base:not_subscribed",
            format!("target not in subscriptions for {:?}", query.capability),
        )
    }
}

fn scope_matches(rule: &GrantRule, target: GrantTarget<'_>, subscriptions: &[RoomId]) -> bool {
    match rule.scope {
        GrantScope::Any => true,
        GrantScope::Subscribed => match target {
            GrantTarget::Room(room) => subscriptions.iter().any(|listed| listed == room),
            GrantTarget::Connection(connection) => subscriptions
                .iter()
                .any(|listed| listed.connection == connection),
        },
        GrantScope::Room => match target {
            GrantTarget::Room(room) => rule
                .target
                .as_deref()
                .is_some_and(|target| target == room.to_string()),
            GrantTarget::Connection(_) => false,
        },
        GrantScope::Connection => {
            let connection = target.connection();
            rule.target
                .as_deref()
                .is_some_and(|target| target == connection)
        }
    }
}

impl GrantRule {
    fn scope_name(&self) -> &'static str {
        match self.scope {
            GrantScope::Any => "any",
            GrantScope::Subscribed => "subscribed",
            GrantScope::Room => "room",
            GrantScope::Connection => "connection",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_access(
        grants: &GrantsConfig,
        subject: GrantSubject,
        place: GrantPlace,
        capability: GrantCapability,
        target: GrantTarget<'_>,
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
        place: GrantPlace,
        target: GrantTarget<'_>,
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
        place: GrantPlace,
        scope: GrantScope,
        target: Option<&str>,
        capability: &[GrantCapability],
    ) -> GrantRule {
        GrantRule {
            subject,
            place,
            scope,
            target: target.map(ToOwned::to_owned),
            capability: capability.to_vec(),
        }
    }

    fn config(rules: Vec<GrantRule>) -> GrantsConfig {
        GrantsConfig {
            rules,
            bot_admin: None,
            system_admin: None,
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
            GrantPlace::Group,
            GrantCapability::RoomRead,
            GrantTarget::Room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow);
        assert!(!decision.audited(), "普通订阅评估不审计");

        let decision = eval_access(
            &grants,
            GrantSubject::User,
            GrantPlace::Group,
            GrantCapability::RoomRead,
            GrantTarget::Room(&outside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow);
        assert!(!decision.audited());
    }

    #[test]
    fn rule_grants_only_listed_capability_and_audits() {
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let grants = config(vec![rule(
            GrantSubject::BotAdmin,
            GrantPlace::Private,
            GrantScope::Subscribed,
            None,
            &[GrantCapability::RoomWrite],
        )]);
        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            GrantPlace::Private,
            GrantCapability::RoomWrite,
            GrantTarget::Room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(decision.allow);
        assert_eq!(decision.basis, "rule:0");
        assert!(decision.audited());

        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            GrantPlace::Private,
            GrantCapability::RoomRead,
            GrantTarget::Room(&inside),
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
                GrantPlace::Private,
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
        };
        assert_eq!(
            grants.subject_for("qq", Some("10001"), GrantPlace::Private),
            GrantSubject::BotAdmin
        );
        assert_eq!(
            grants.subject_for("qq", Some("10001"), GrantPlace::Group),
            GrantSubject::BotAdmin,
            "主体不变；场所维度会阻止私信规则命中群聊"
        );
        let inside = RoomId::parse("qq:group/1").expect("房间");
        let decision = eval_access(
            &grants,
            GrantSubject::BotAdmin,
            GrantPlace::Group,
            GrantCapability::RoomWrite,
            GrantTarget::Room(&inside),
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
        assert_eq!(
            grants.subject_for("qq", None, GrantPlace::None),
            GrantSubject::System
        );
        let decision = eval_access(
            &grants,
            GrantSubject::System,
            GrantPlace::None,
            GrantCapability::RoomRead,
            GrantTarget::Room(&outside),
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
            GrantPlace::Private,
            GrantTarget::Room(&inside),
            std::slice::from_ref(&inside),
        );
        assert!(!decision.allow);
        assert_eq!(decision.basis, "admin:no_plugin_grant");
        assert!(decision.audited());
    }
}
