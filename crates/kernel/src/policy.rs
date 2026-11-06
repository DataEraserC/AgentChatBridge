//! 授权判定接口。
//!
//! core 只保留单一收口点、票据有效性、审计与 fail-closed；具体“怎么判”由
//! [`Policy`] 实现负责。内置 [`CorePolicy`] 是默认可替换实现，不是 engine 里的
//! 业务分支：
//!
//! `effective = (floor ∪ grants) − deny`
//!
//! - floor 始终参与；
//! - 规则匹配但没列出该 capability，不构成 deny；
//! - 减法只来自独立的 `[grants].deny` 列表。

use crate::grants::{
    GrantCapability, GrantDecision, GrantQuery, GrantSubject, Place, RoomScope, base_access,
    scope_matches,
};

/// 判定实现失败。core 收到任何错误都必须 deny（fail-closed）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    Unavailable,
    Timeout,
    InvalidDecision,
    VersionMismatch,
    Panic,
}

/// 唯一判定入口。实现可以是内置 `CorePolicy`，也可以是后续的插件实现。
pub trait Policy: Send + Sync {
    fn decide(&self, request: &GrantQuery<'_>) -> Result<GrantDecision, PolicyError>;
}

/// 默认实现：今天的 RBAC，但按新语义求值。
///
/// 无状态：策略快照随 `GrantQuery` 进来，保证一轮内冻结；替换成插件实现时
/// 也不需要在这里持有全局配置。
#[derive(Debug, Clone, Copy, Default)]
pub struct CorePolicy;

impl CorePolicy {
    pub fn new() -> Self {
        Self
    }

    fn floor(&self, request: &GrantQuery<'_>) -> Option<&'static str> {
        match request.capability {
            // 管理员插件能力没有 base；普通主体沿用上游 scope 检查。
            GrantCapability::Plugin => {
                if is_admin(request.subject) {
                    None
                } else {
                    Some("base:plugin")
                }
            }
            // 管理员在私信/控制台可以申请任意目标；其余按订阅边界。
            GrantCapability::AccessRequest => {
                if is_admin(request.subject) && is_private_place(request.place) {
                    Some("base:admin_private")
                } else if base_access(request).allow {
                    Some("base:subscription")
                } else {
                    None
                }
            }
            // 凭证、破坏性、最高危能力不能从 floor 来；只能显式规则或票据。
            GrantCapability::AccountSecret
            | GrantCapability::Destructive
            | GrantCapability::Critical => None,
            _ => base_access(request).allow.then_some("base:subscription"),
        }
    }

    fn rule_matches(&self, rule: &crate::grants::GrantRule, request: &GrantQuery<'_>) -> bool {
        rule.subject == request.subject
            && rule.place == request.place
            && scope_matches(rule, request)
    }

    fn no_grant(&self, request: &GrantQuery<'_>) -> GrantDecision {
        match request.capability {
            GrantCapability::Plugin if is_admin(request.subject) => GrantDecision::deny(
                "admin:no_plugin_grant",
                "admin subject has no matching plugin grant",
            ),
            GrantCapability::AccountSecret => GrantDecision::deny(
                "account_secret_ticket_only",
                "account_secret requires an access_request ticket",
            ),
            GrantCapability::Destructive | GrantCapability::Critical => GrantDecision::deny(
                "base:no_high_risk_grant",
                "high-risk capability requires an explicit grant or an access_request ticket",
            ),
            _ => base_access(request),
        }
    }
}

impl Policy for CorePolicy {
    fn decide(&self, request: &GrantQuery<'_>) -> Result<GrantDecision, PolicyError> {
        let mut sources: Vec<String> = Vec::new();
        let mut allowed = false;

        if let Some(floor) = self.floor(request) {
            allowed = true;
            sources.push(floor.to_owned());
        }

        for rule in &request.grants.rules {
            if !self.rule_matches(rule, request) {
                continue;
            }
            if rule.capability.contains(&request.capability) {
                allowed = true;
                sources.push(format!("rule:{}", rule.id));
            }
            // 匹配但没列出该能力：不构成 deny，继续看其它规则。
        }

        let mut denied: Vec<String> = Vec::new();
        for rule in &request.grants.deny {
            if !self.rule_matches(rule, request) {
                continue;
            }
            if rule.capability.contains(&request.capability) {
                denied.push(format!("deny:{}", rule.id));
            }
        }

        if !denied.is_empty() {
            sources.extend(denied);
            let basis = sources.join(",");
            return Ok(GrantDecision::deny(
                basis.clone(),
                format!(
                    "explicit deny for {:?} on {} {}",
                    request.capability,
                    request.subject.as_str(),
                    request.place.as_str()
                ),
            )
            .at_version(request.policy_version));
        }

        if allowed {
            let basis = sources.join(",");
            return Ok(GrantDecision::allow(
                basis.clone(),
                format!(
                    "{} {} {}",
                    request.subject.as_str(),
                    request.place.as_str(),
                    request.capability.as_str()
                ),
            )
            .at_version(request.policy_version));
        }

        Ok(self.no_grant(request).at_version(request.policy_version))
    }
}

fn is_admin(subject: GrantSubject) -> bool {
    matches!(subject, GrantSubject::SystemAdmin | GrantSubject::BotAdmin)
}

fn is_private_place(place: Place) -> bool {
    matches!(place, Place::Room(RoomScope::Direct) | Place::Console)
}
