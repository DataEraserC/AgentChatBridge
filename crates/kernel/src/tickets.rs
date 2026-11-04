//! 一次性访问票据：群内向管理员申请，私信批准后单次使用。
//!
//! 票据只放宽「能不能碰这个房间」的目标判定；它不替代
//! `allow_destructive`，也不自动授予 `plugin` 全权。
//!
//! 状态只活在本进程：重启后 pending / active 票据全部失效，不发通知、不恢复。
//! 这是 fail closed 的取舍，见 `docs/plan/access-grants.md`。

use std::collections::hash_map::RandomState;
use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_proto::RoomId;

use crate::grants::{GrantCapability, GrantPlace, GrantSubject, truncate_target};

/// 申请后等管理员批准的默认上限：10 分钟。
pub const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
/// 批准后可用的默认上限：5 分钟。
pub const ACTIVE_TTL_MS: i64 = 5 * 60 * 1000;
/// 一个轮次最多申请几张票据。
pub const MAX_REQUESTS_PER_TURN: usize = 3;
/// 一个会话同时最多持有几张（pending + active）。
pub const MAX_LIVE_PER_SESSION: usize = 5;
/// 单张票据最多列几个 op。
pub const MAX_OPS: usize = 8;
/// 单个 op 的最大长度。
pub const MAX_OP_CHARS: usize = 64;
/// 内存里最多保留多少张票据（含终态）。
pub const MAX_TICKETS: usize = 200;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 票据状态。终态：`used` / `expired` / `revoked` / `denied`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketState {
    PendingApproval,
    Active,
    Used,
    Expired,
    Revoked,
    Denied,
}

impl TicketState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PendingApproval => "pending_approval",
            Self::Active => "active",
            Self::Used => "used",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::Denied => "denied",
        }
    }

    fn live(self) -> bool {
        matches!(self, Self::PendingApproval | Self::Active)
    }
}

/// 一张票据。`nonce` 只在 pending 阶段有效；任何迁移后清空，防重放。
#[derive(Debug, Clone)]
pub struct AccessTicket {
    pub id: String,
    pub nonce: String,
    pub principal: GrantSubject,
    pub place: GrantPlace,
    pub session: String,
    pub target: RoomId,
    pub ops: Vec<String>,
    pub capabilities: BTreeSet<GrantCapability>,
    pub created_at: i64,
    pub expires_at: i64,
    pub state: TicketState,
    pub request_turn: String,
    pub approval_ref: Option<String>,
    pub approver: Option<String>,
    pub used_at: Option<i64>,
}

/// 一次申请的全部输入。
#[derive(Debug, Clone)]
pub struct TicketRequest {
    pub principal: GrantSubject,
    pub place: GrantPlace,
    pub session: String,
    pub target: RoomId,
    pub ops: Vec<String>,
    pub capabilities: BTreeSet<GrantCapability>,
    pub request_turn: String,
    pub now: i64,
}

/// 申请结果。`duplicate=true` 表示命中了同一会话/目标/op 的现有票据，
/// 不再重复发审批消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketReply {
    pub id: String,
    pub nonce: String,
    pub state: TicketState,
    pub expires_at: i64,
    pub duplicate: bool,
}

/// 进程内的票据簿。所有方法都在 core 的全局锁内调用，不需要额外同步。
#[derive(Debug, Default)]
pub struct AccessTickets {
    tickets: HashMap<String, AccessTicket>,
    seq: u64,
}

impl AccessTickets {
    pub fn new() -> Self {
        Self::default()
    }

    /// 创建或复用一张 pending 票据。返回 `Err(reason)` 时不会创建任何东西。
    pub fn request(&mut self, request: TicketRequest) -> Result<TicketReply, String> {
        self.sweep(request.now);
        let ops = normalize_ops(&request.ops)?;
        if request.capabilities.is_empty() {
            return Err("capability set must not be empty".to_owned());
        }

        if let Some(existing) = self.tickets.values().find(|ticket| {
            ticket.state.live()
                && ticket.session == request.session
                && ticket.target == request.target
                && ticket.ops == ops
                && ticket.capabilities == request.capabilities
        }) {
            return Ok(TicketReply {
                id: existing.id.clone(),
                nonce: existing.nonce.clone(),
                state: existing.state,
                expires_at: existing.expires_at,
                duplicate: true,
            });
        }

        let turn_count = self
            .tickets
            .values()
            .filter(|ticket| ticket.state.live() && ticket.request_turn == request.request_turn)
            .count();
        if turn_count >= MAX_REQUESTS_PER_TURN {
            return Err(format!(
                "turn {} already has {} live tickets (max {MAX_REQUESTS_PER_TURN})",
                request.request_turn, turn_count
            ));
        }
        let session_count = self
            .tickets
            .values()
            .filter(|ticket| ticket.state.live() && ticket.session == request.session)
            .count();
        if session_count >= MAX_LIVE_PER_SESSION {
            return Err(format!(
                "session {} already has {} live tickets (max {MAX_LIVE_PER_SESSION})",
                request.session, session_count
            ));
        }

        self.seq = self.seq.saturating_add(1);
        let id = format!("tk-{}-{}", random_hex(), self.seq);
        let nonce = random_hex();
        let ticket = AccessTicket {
            id: id.clone(),
            nonce: nonce.clone(),
            principal: request.principal,
            place: request.place,
            session: request.session,
            target: request.target,
            ops,
            capabilities: request.capabilities,
            created_at: request.now,
            expires_at: request.now.saturating_add(PENDING_TTL_MS),
            state: TicketState::PendingApproval,
            request_turn: request.request_turn,
            approval_ref: None,
            approver: None,
            used_at: None,
        };
        self.audit("request", &ticket, "requested");
        let reply = TicketReply {
            id: ticket.id.clone(),
            nonce: ticket.nonce.clone(),
            state: ticket.state,
            expires_at: ticket.expires_at,
            duplicate: false,
        };
        self.tickets.insert(id, ticket);
        self.prune();
        Ok(reply)
    }

    /// 私信管理员批准。`nonce` 必须与 pending 票据完全一致。
    pub fn approve(
        &mut self,
        id: &str,
        nonce: &str,
        approval_ref: &str,
        approver: &str,
        now: i64,
    ) -> Result<TicketReply, String> {
        self.sweep(now);
        let snapshot = {
            let ticket = self
                .tickets
                .get_mut(id)
                .ok_or_else(|| format!("unknown ticket {id}"))?;
            if ticket.state != TicketState::PendingApproval {
                return Err(format!(
                    "ticket {id} is {}; only pending_approval can be approved",
                    ticket.state.as_str()
                ));
            }
            if ticket.nonce != nonce {
                return Err("nonce mismatch".to_owned());
            }
            ticket.state = TicketState::Active;
            ticket.expires_at = now.saturating_add(ACTIVE_TTL_MS);
            ticket.approval_ref = Some(approval_ref.to_owned());
            ticket.approver = Some(approver.to_owned());
            ticket.nonce.clear();
            ticket.snapshot()
        };
        self.audit("approve", &snapshot, "approved");
        Ok(reply(&snapshot, false))
    }

    /// 私信管理员拒绝。
    pub fn deny(
        &mut self,
        id: &str,
        nonce: &str,
        approval_ref: &str,
        approver: &str,
        now: i64,
    ) -> Result<TicketReply, String> {
        self.sweep(now);
        let snapshot = {
            let ticket = self
                .tickets
                .get_mut(id)
                .ok_or_else(|| format!("unknown ticket {id}"))?;
            if ticket.state != TicketState::PendingApproval {
                return Err(format!(
                    "ticket {id} is {}; only pending_approval can be denied",
                    ticket.state.as_str()
                ));
            }
            if ticket.nonce != nonce {
                return Err("nonce mismatch".to_owned());
            }
            ticket.state = TicketState::Denied;
            ticket.approval_ref = Some(approval_ref.to_owned());
            ticket.approver = Some(approver.to_owned());
            ticket.nonce.clear();
            ticket.snapshot()
        };
        self.audit("deny", &snapshot, "denied");
        Ok(reply(&snapshot, false))
    }

    /// 管理员撤销 pending 或 active 票据。撤销不需要 nonce，但调用方必须是
    /// 私信里的 system admin。
    pub fn revoke(&mut self, id: &str, approver: &str, now: i64) -> Result<TicketReply, String> {
        self.sweep(now);
        let snapshot = {
            let ticket = self
                .tickets
                .get_mut(id)
                .ok_or_else(|| format!("unknown ticket {id}"))?;
            if !ticket.state.live() {
                return Err(format!(
                    "ticket {id} is {}; cannot revoke",
                    ticket.state.as_str()
                ));
            }
            ticket.state = TicketState::Revoked;
            ticket.approver = Some(approver.to_owned());
            ticket.nonce.clear();
            ticket.snapshot()
        };
        self.audit("revoke", &snapshot, "revoked");
        Ok(reply(&snapshot, false))
    }

    /// 找到并消费一张匹配的 active 票据。返回票据 id；没有就不返回。
    /// 命中即置 `used`，即使后续的 destructive 闸门拒绝也不会退回。
    pub fn authorize(
        &mut self,
        session: &str,
        target: &RoomId,
        capability: GrantCapability,
        op: &str,
        now: i64,
    ) -> Option<String> {
        self.sweep(now);
        let id = self
            .tickets
            .values()
            .filter(|ticket| {
                ticket.state == TicketState::Active
                    && ticket.session == session
                    && &ticket.target == target
                    && ticket.capabilities.contains(&capability)
                    && ticket.ops.iter().any(|listed| listed == op)
                    && ticket.expires_at > now
            })
            .min_by_key(|ticket| (ticket.created_at, ticket.id.as_str()))
            .map(|ticket| ticket.id.clone())?;
        let snapshot = {
            let ticket = self.tickets.get_mut(&id)?;
            ticket.state = TicketState::Used;
            ticket.used_at = Some(now);
            ticket.nonce.clear();
            ticket.snapshot()
        };
        self.audit("use", &snapshot, "used");
        Some(id)
    }

    pub fn get(&self, id: &str) -> Option<&AccessTicket> {
        self.tickets.get(id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.tickets.len()
    }

    /// 过期扫描：pending/active 到点即失效。每次请求/批准/使用前都会跑一次。
    fn sweep(&mut self, now: i64) {
        let mut expired = Vec::new();
        for ticket in self.tickets.values_mut() {
            if ticket.state.live() && ticket.expires_at <= now {
                ticket.state = TicketState::Expired;
                ticket.nonce.clear();
                expired.push(ticket.snapshot());
            }
        }
        for ticket in &expired {
            self.audit("expire", ticket, "expired");
        }
        self.prune();
    }

    /// 只保留最近的票据，避免内存无限增长；live 票据永远保留。
    fn prune(&mut self) {
        if self.tickets.len() <= MAX_TICKETS {
            return;
        }
        let mut terminal: Vec<(i64, String)> = self
            .tickets
            .values()
            .filter(|ticket| !ticket.state.live())
            .map(|ticket| (ticket.created_at, ticket.id.clone()))
            .collect();
        terminal.sort_unstable();
        let excess = self.tickets.len() - MAX_TICKETS;
        for (_, id) in terminal.into_iter().take(excess) {
            self.tickets.remove(&id);
        }
    }

    fn audit(&self, event: &str, ticket: &AccessTicket, reason: &str) {
        let capabilities = ticket
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join("|");
        let ops = ticket.ops.join("|");
        eprintln!(
            "[access-ticket] event={} ticket={} state={} principal={} place={} capability={} target={} ops={} session={} turn={} approval_ref={} reason=\"{}\"",
            event,
            ticket.id,
            ticket.state.as_str(),
            ticket.principal.as_str(),
            ticket.place.as_str(),
            capabilities,
            truncate_target(&ticket.target.to_string()),
            ops,
            ticket.session,
            ticket.request_turn,
            ticket.approval_ref.as_deref().unwrap_or("-"),
            reason.replace('"', "'"),
        );
    }
}

impl AccessTicket {
    fn snapshot(&self) -> Self {
        self.clone()
    }
}

fn reply(ticket: &AccessTicket, duplicate: bool) -> TicketReply {
    TicketReply {
        id: ticket.id.clone(),
        nonce: ticket.nonce.clone(),
        state: ticket.state,
        expires_at: ticket.expires_at,
        duplicate,
    }
}

pub fn normalize_ops(ops: &[String]) -> Result<Vec<String>, String> {
    if ops.is_empty() {
        return Err("ops must not be empty".to_owned());
    }
    if ops.len() > MAX_OPS {
        return Err(format!("ops must not exceed {MAX_OPS} entries"));
    }
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::with_capacity(ops.len());
    for op in ops {
        let op = op.trim();
        if op.is_empty() || op.chars().count() > MAX_OP_CHARS {
            return Err(format!("invalid op {op:?}"));
        }
        if !op
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':'))
        {
            return Err(format!("invalid op {op:?}"));
        }
        if !seen.insert(op.to_owned()) {
            continue;
        }
        normalized.push(op.to_owned());
    }
    normalized.sort();
    Ok(normalized)
}

fn random_hex() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(now);
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> RoomId {
        RoomId::parse("qq:group/999").expect("房间")
    }

    fn request(now: i64, ops: &[&str]) -> TicketRequest {
        TicketRequest {
            principal: GrantSubject::User,
            place: GrantPlace::Group,
            session: "main".to_owned(),
            target: target(),
            ops: ops.iter().map(|op| (*op).to_owned()).collect(),
            capabilities: BTreeSet::from([GrantCapability::RoomWrite]),
            request_turn: "turn-1".to_owned(),
            now,
        }
    }

    #[test]
    fn approve_then_use_once() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        assert_eq!(reply.state, TicketState::PendingApproval);
        assert!(!reply.duplicate);

        let approved = tickets
            .approve(
                &reply.id,
                &reply.nonce,
                "qq:private/1#9",
                "system_admin",
                2000,
            )
            .expect("批准");
        assert_eq!(approved.state, TicketState::Active);

        let id = tickets
            .authorize(
                "main",
                &target(),
                GrantCapability::RoomWrite,
                "send_message_to",
                2500,
            )
            .expect("票据可用");
        assert_eq!(id, reply.id);
        assert_eq!(tickets.get(&id).expect("还在簿上").state, TicketState::Used);

        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomWrite,
                    "send_message_to",
                    2501
                )
                .is_none(),
            "用后即废"
        );
    }

    #[test]
    fn wrong_nonce_and_replay_are_rejected() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        assert!(
            tickets
                .approve(&reply.id, "not-the-nonce", "ref", "system_admin", 2000)
                .is_err()
        );
        tickets
            .approve(&reply.id, &reply.nonce, "ref", "system_admin", 2000)
            .expect("批准");
        assert!(
            tickets
                .approve(&reply.id, &reply.nonce, "ref", "system_admin", 2001)
                .is_err(),
            "批准消息不能重放"
        );
    }

    #[test]
    fn pending_expires_and_dedup_returns_existing() {
        let mut tickets = AccessTickets::new();
        let first = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        let again = tickets
            .request(TicketRequest {
                request_turn: "turn-2".to_owned(),
                now: 2000,
                ..request(2000, &["send_message_to"])
            })
            .expect("复用");
        assert!(again.duplicate);
        assert_eq!(again.id, first.id);

        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomWrite,
                    "send_message_to",
                    1000 + PENDING_TTL_MS + 1
                )
                .is_none()
        );
        assert_eq!(
            tickets.get(&first.id).expect("还在簿上").state,
            TicketState::Expired
        );
    }

    #[test]
    fn op_and_target_must_match() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        tickets
            .approve(&reply.id, &reply.nonce, "ref", "system_admin", 2000)
            .expect("批准");
        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomWrite,
                    "send_message",
                    2500
                )
                .is_none(),
            "op 不在列表里"
        );
        assert!(
            tickets
                .authorize(
                    "main",
                    &RoomId::parse("qq:group/998").expect("房间"),
                    GrantCapability::RoomWrite,
                    "send_message_to",
                    2500
                )
                .is_none(),
            "目标房间不匹配"
        );
        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomRead,
                    "send_message_to",
                    2500
                )
                .is_none(),
            "能力不匹配"
        );
    }

    #[test]
    fn limits_and_revocation() {
        let mut tickets = AccessTickets::new();
        let first = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        tickets
            .revoke(&first.id, "system_admin", 2000)
            .expect("撤销");
        assert_eq!(
            tickets.get(&first.id).expect("还在簿上").state,
            TicketState::Revoked
        );
        assert!(tickets.revoke(&first.id, "system_admin", 2001).is_err());

        for index in 0..MAX_REQUESTS_PER_TURN {
            tickets
                .request(TicketRequest {
                    request_turn: "turn-2".to_owned(),
                    target: RoomId::parse(&format!("qq:group/{index}")).expect("房间"),
                    ..request(3000, &["send_message_to"])
                })
                .expect("申请");
        }
        assert!(
            tickets
                .request(TicketRequest {
                    request_turn: "turn-2".to_owned(),
                    target: RoomId::parse("qq:group/3").expect("房间"),
                    ..request(3000, &["send_message_to"])
                })
                .is_err(),
            "单轮上限"
        );
        assert_eq!(tickets.len(), 1 + MAX_REQUESTS_PER_TURN);
    }
}
