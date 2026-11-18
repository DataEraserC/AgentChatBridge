//! 票据审批的判定接口。
//!
//! 与 [`crate::policy::Policy`] 同一个思路：core 保留**状态机与防伪**，具体「谁能批」
//! 交给 [`TicketPolicy`] 实现。默认为 [`CoreTicketPolicy`]（= 今天这套 RBAC），
//! 第三方可以换实现（比如接 OA 审批、走 OAuth 授权）。
//!
//! ## 什么留在 core，什么交出去
//!
//! | 留在 core | 理由 |
//! |---|---|
//! | 票据 id / nonce / 过期 | 防伪是安全基础，不能由插件决定 |
//! | `pending → active → used` 流转 | 「用一次就失效」是票据的定义 |
//! | 批量聚合 | 状态一致性 |
//! | fail-closed | 插件出错/超时必须**拒绝**，不能放行 |
//!
//! 交出去的只有一件事：**某个人在某个房间，能不能批这张票**。

use crate::config::Config;
use crate::grants::{Actor, GrantCapability, GrantQuery, GrantSubject, Place};
use crate::policy::{Policy, PolicyError};
use crate::tickets::AccessTicket;
use acb_proto::RoomId;

/// 判定失败。core 收到任何错误都必须**拒绝**（fail-closed）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketPolicyError {
    /// 实现不可用（插件没加载、连接断了）。
    Unavailable,
    Timeout,
    /// 实现返回了 core 不认识的判定。
    InvalidVerdict,
    /// 实现自己 panic 了。
    Panic,
}

impl From<PolicyError> for TicketPolicyError {
    fn from(error: PolicyError) -> Self {
        match error {
            PolicyError::Unavailable => Self::Unavailable,
            PolicyError::Timeout => Self::Timeout,
            PolicyError::InvalidDecision | PolicyError::VersionMismatch => Self::InvalidVerdict,
            PolicyError::Panic => Self::Panic,
        }
    }
}

/// 一次审批判定的全部输入。**只读**：不含 Store、不含票的状态、不改任何东西。
pub struct ApproveQuery<'a> {
    /// 谁在批（房间里的发送者 id）。
    pub approver: &'a str,
    /// 在哪个房间批的。
    pub room: &'a RoomId,
    /// 审批人在这条连接上的主体身份（`system_admin` / `user` / …）。
    pub subject: GrantSubject,
    /// 审批发生的场所（私聊 / 群 / 控制台）。
    pub place: Place,
    /// 待批的票（只读快照）。
    pub ticket: &'a AccessTicket,
    /// 全部授权配置。实现可以据此自己解释规则。
    pub grants: &'a crate::grants::GrantsConfig,
    /// 配置版本，供实现做缓存失效。
    pub policy_version: i64,
}

/// 审批判定。
pub enum ApproveVerdict {
    Allow,
    Deny { reason: String },
}

/// 「谁能批这张票」的可替换实现。
///
/// 与 [`Policy`] 分离而不是合并：`Policy` 回答的是「此刻能不能做某个动作」，
/// 高频、无状态；审批判定只在 `/approve` 时跑一次，输入里还要带整张票。
/// 合并会让 `Policy` 的实现被迫处理它不关心的票据结构。
pub trait TicketPolicy: Send + Sync {
    fn can_approve(&self, query: &ApproveQuery<'_>) -> Result<ApproveVerdict, TicketPolicyError>;
}

/// 默认实现：今天这套规则，原样搬过来。
pub struct CoreTicketPolicy {
    /// 内层用它跑能力判定（`effective = (floor ∪ grants) − deny`）。
    policy: std::sync::Arc<dyn Policy>,
}

impl CoreTicketPolicy {
    pub fn new(policy: std::sync::Arc<dyn Policy>) -> Self {
        Self { policy }
    }
}

impl TicketPolicy for CoreTicketPolicy {
    fn can_approve(&self, query: &ApproveQuery<'_>) -> Result<ApproveVerdict, TicketPolicyError> {
        let ticket = query.ticket;
        let actor = Actor {
            connection: &query.room.connection,
            account: Some(query.approver),
            room: Some(query.room),
            member: Some(query.approver),
        };
        let subscriptions: Vec<RoomId> = Vec::new();
        let run = |capability| {
            let request = GrantQuery {
                subject: query.subject,
                place: query.place,
                capability,
                actor: Some(&actor),
                target: &ticket.target,
                subscriptions: &subscriptions,
                origin: Some(query.room),
                platform_op: None,
                permissions: &[],
                policy_version: query.policy_version,
                grants: query.grants,
            };
            crate::policy::policy_decision(self.policy.as_ref(), &request)
        };

        if !run(GrantCapability::AccessApprove).allow {
            return Ok(ApproveVerdict::Deny {
                reason: "审批人没有 access_approve 权限".to_owned(),
            });
        }
        for capability in &ticket.capabilities {
            // 票据专属能力（account_secret）无法被任何人长期持有：只允许 system_admin
            // 以 break-glass 方式批准，不再要求「持有该能力」。
            if capability.is_ticket_only() {
                if query.subject != GrantSubject::SystemAdmin {
                    return Ok(ApproveVerdict::Deny {
                        reason: format!("{} 只能由 system_admin 批准", capability.as_str()),
                    });
                }
                continue;
            }
            if matches!(
                capability,
                GrantCapability::Destructive
                    | GrantCapability::Critical
                    | GrantCapability::PolicyWrite
                    | GrantCapability::PluginAdmin
            ) && query.subject != GrantSubject::SystemAdmin
            {
                return Ok(ApproveVerdict::Deny {
                    reason: format!("高危能力 {} 只能由 system_admin 批准", capability.as_str()),
                });
            }
            if !run(*capability).allow {
                return Ok(ApproveVerdict::Deny {
                    reason: format!(
                        "审批人缺少 {} 权限，或范围不覆盖目标 {}",
                        capability.as_str(),
                        ticket.target.display()
                    ),
                });
            }
        }
        Ok(ApproveVerdict::Allow)
    }
}

/// core 侧的统一入口：跑实现、**任何异常都变成拒绝**。
///
/// fail-closed 放在这里而不是每个实现里：实现者忘了一件小事就会变成「出错即放行」。
pub fn approve_decision(policy: &dyn TicketPolicy, query: &ApproveQuery<'_>) -> ApproveVerdict {
    match policy.can_approve(query) {
        Ok(verdict) => verdict,
        Err(error) => ApproveVerdict::Deny {
            reason: format!("审批判定不可用（{error:?}），按拒绝处理"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysDeny;

    impl TicketPolicy for AlwaysDeny {
        fn can_approve(&self, _: &ApproveQuery<'_>) -> Result<ApproveVerdict, TicketPolicyError> {
            Err(TicketPolicyError::Unavailable)
        }
    }

    /// 实现出错必须变成拒绝，不能放行。
    ///
    /// 这条是 fail-closed 的落点：把 `Err` 当 `Allow` 就等于「插件一崩，谁都能批」。
    #[test]
    fn implementation_error_becomes_deny() {
        let ticket = AccessTicket {
            id: "tk-test".to_owned(),
            ..test_ticket()
        };
        let room = RoomId::new("qq", "private/admin").expect("合法");
        let grants = crate::grants::GrantsConfig::default();
        let query = ApproveQuery {
            approver: "admin",
            room: &room,
            subject: GrantSubject::User,
            place: Place::None,
            ticket: &ticket,
            grants: &grants,
            policy_version: 1,
        };
        match approve_decision(&AlwaysDeny, &query) {
            ApproveVerdict::Deny { reason } => {
                assert!(reason.contains("按拒绝处理"), "实际：{reason}")
            }
            ApproveVerdict::Allow => panic!("实现出错时不能放行"),
        }
    }

    fn test_ticket() -> AccessTicket {
        AccessTicket {
            id: String::new(),
            nonce: String::new(),
            principal: GrantSubject::User,
            place: Place::None,
            session: "s".to_owned(),
            request_room: RoomId::new("qq", "private/u").expect("合法"),
            target: crate::grants::AccessTarget::room(&RoomId::new("qq", "group/1").expect("合法")),
            ops: Vec::new(),
            params: None,
            reason: None,
            origin_sender: None,
            origin_sender_name: None,
            capabilities: std::collections::BTreeSet::new(),
            unverified: Vec::new(),
            seq: 1,
            created_at: 1,
            expires_at: 2,
            state: crate::tickets::TicketState::PendingApproval,
            request_turn: "t".to_owned(),
            batch_id: None,
            approval_route: Vec::new(),
            approval_room: None,
            approval_place: None,
            approval_ref: None,
            approver: None,
            trusted_group: false,
            used_at: None,
            announced: false,
            route_unavailable: None,
            route_unavailable_at: None,
            execution_status: None,
            executed_at: None,
            result: None,
            failure_reason: None,
            audit_ref: None,
            expire_reason: None,
        }
    }
}
