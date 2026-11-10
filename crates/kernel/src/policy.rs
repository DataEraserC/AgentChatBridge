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
    AssignmentScopeKind, AssignmentSubjectKind, GrantCapability, GrantDecision, GrantQuery,
    GrantSubject, Place, PolicyAssignment, RoomScope, base_access, origin_access, scope_matches,
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
            // 管理员在私信/控制台可以申请任意目标；其余只看发起位置。
            //
            // 申请这一刻判的是「能不能发起申请」，不是「能不能碰目标」：
            // 目标可达性是票据想突破的东西，留给审批人判（2026-11-09 裁定）。
            GrantCapability::AccessRequest => {
                if is_admin(request.subject) && is_private_place(request.place) {
                    Some("base:admin_private")
                } else if origin_access(request).allow {
                    Some("base:request_origin")
                } else {
                    None
                }
            }
            // 凭证、破坏性、最高危能力不能从 floor 来；只能显式规则或票据。
            GrantCapability::AccountSecret
            | GrantCapability::Destructive
            | GrantCapability::Critical
            | GrantCapability::ConfigRead
            | GrantCapability::ConfigWrite
            | GrantCapability::PolicyWrite
            | GrantCapability::PluginAdmin => None,
            // 管理员本身就有审批权；普通主体必须显式拿 access_approve。
            GrantCapability::AccessApprove if is_admin(request.subject) => {
                Some("base:admin_approve")
            }
            GrantCapability::AccessApprove => None,
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
            GrantCapability::AccessRequest => origin_access(request),
            GrantCapability::AccountSecret => GrantDecision::deny(
                "account_secret_ticket_only",
                "account_secret requires an access_request ticket",
            ),
            GrantCapability::Destructive | GrantCapability::Critical => GrantDecision::deny(
                "base:no_high_risk_grant",
                "high-risk capability requires an explicit grant or an access_request ticket",
            ),
            GrantCapability::AccessApprove
            | GrantCapability::ConfigRead
            | GrantCapability::ConfigWrite
            | GrantCapability::PolicyWrite
            | GrantCapability::PluginAdmin => GrantDecision::deny(
                "base:no_management_grant",
                "management capability requires an explicit role/rule grant",
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

        for assignment in &request.grants.assignments {
            if !assignment_matches(assignment, request) {
                continue;
            }
            if assignment.capability_snapshot.contains(&request.capability) {
                allowed = true;
                sources.push(format!("role:{}#{}", assignment.role_id, assignment.id));
            }
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

fn assignment_matches(assignment: &PolicyAssignment, request: &GrantQuery<'_>) -> bool {
    let subject_matches = match assignment.subject_kind {
        AssignmentSubjectKind::Subject => assignment.subject_id == request.subject.as_str(),
        AssignmentSubjectKind::Account => request.actor.is_some_and(|actor| {
            actor.account.is_some_and(|account| {
                format!("{}:{account}", actor.connection) == assignment.subject_id
            })
        }),
        AssignmentSubjectKind::Member => request.actor.is_some_and(|actor| {
            actor.room.is_some_and(|room| {
                actor
                    .member
                    .is_some_and(|member| format!("{room}#{member}") == assignment.subject_id)
            })
        }),
    };
    if !subject_matches {
        return false;
    }
    match assignment.scope_kind {
        AssignmentScopeKind::Global => true,
        AssignmentScopeKind::Connection => request
            .actor
            .is_some_and(|actor| assignment.scope_id.as_deref() == Some(actor.connection)),
        AssignmentScopeKind::Room => request.actor.is_some_and(|actor| match actor.room {
            Some(room) => assignment.scope_id.as_deref() == Some(room.to_string().as_str()),
            None => false,
        }),
    }
}

fn is_admin(subject: GrantSubject) -> bool {
    matches!(subject, GrantSubject::SystemAdmin | GrantSubject::BotAdmin)
}

fn is_private_place(place: Place) -> bool {
    matches!(place, Place::Room(RoomScope::Direct) | Place::Console)
}
