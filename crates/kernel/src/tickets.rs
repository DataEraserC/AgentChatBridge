//! 一次性访问票据：群内向管理员申请，私信批准后单次使用。
//!
//! 票据只放宽「能不能碰这个房间」的目标判定；它不替代
//! `[connections.policy.risk]`，也不自动授予 `plugin` 全权。
//!
//! 状态只活在本进程：重启后 pending / active 票据全部失效，不发通知、不恢复。
//! 这是 fail closed 的取舍，见 `docs/plan/access-grants.md`。

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;

use acb_proto::RoomId;
use serde::Deserialize;
use serde_json::Value;

use crate::grants::{AccessTarget, GrantCapability, GrantSubject, Place, truncate_target};

/// `[tickets]`：审批冷却与聚合的节奏。0 = 关闭对应冷却。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TicketsPolicy {
    /// 同一会话两次「已发出审批消息」之间的最小间隔。
    pub request_cooldown_secs: u64,
    /// 同一个 system admin 收到两条审批消息之间的最小间隔。
    pub admin_cooldown_secs: u64,
}

impl Default for TicketsPolicy {
    fn default() -> Self {
        Self {
            request_cooldown_secs: 60,
            admin_cooldown_secs: 30,
        }
    }
}

/// 申请后等管理员批准的默认上限：10 分钟。
pub const PENDING_TTL_MS: i64 = 10 * 60 * 1000;
/// 批准后可用的默认上限：2 分钟。
pub const ACTIVE_TTL_MS: i64 = 2 * 60 * 1000;
/// 一个轮次最多申请几张票据。
pub const MAX_REQUESTS_PER_TURN: usize = 3;
/// 一个会话同时最多持有几张（pending + active）。
pub const MAX_LIVE_PER_SESSION: usize = 8;
/// 单次批量申请最多几张票据。
pub const MAX_ITEMS_PER_BATCH: usize = 8;
/// 单个参数快照的最大字节数。
pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024;
/// 单张票据最多列几个 op。
pub const MAX_OPS: usize = 8;
/// 单个 op 的最大长度。
pub const MAX_OP_CHARS: usize = 64;
/// 内存里最多保留多少张票据（含终态）。
pub const MAX_TICKETS: usize = 200;
/// 同一审批人在窗口内允许的 nonce 失败次数；达到后临时拒绝。
pub const NONCE_FAILURE_LIMIT: usize = 5;
/// nonce 失败计数的滑动窗口，与 `PENDING_TTL_MS` 对齐。
pub const NONCE_FAILURE_WINDOW_MS: i64 = PENDING_TTL_MS;

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

/// 票据里被标 unverified 的 op 与原因摘要；审批展示与审计用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnverifiedOp {
    pub op: String,
    pub reason: String,
}

/// 一张票据。`nonce` 只在 pending 阶段有效；任何迁移后清空，防重放。
#[derive(Debug, Clone)]
pub struct AccessTicket {
    pub id: String,
    pub nonce: String,
    pub principal: GrantSubject,
    pub place: Place,
    pub session: String,
    /// 申请时所在的房间（原会话流水写结果指针的位置）。
    pub request_room: RoomId,
    pub target: AccessTarget,
    pub ops: Vec<String>,
    /// 挂起动作的参数快照；只存内存，随票据终态释放。
    pub params: Option<Value>,
    /// 申请轮次的触发者；唤醒执行结果时沿用，不继承管理员身份。
    pub origin_sender: Option<String>,
    /// 触发者的显示名，只用于审批私信与审计；不参与身份判定。
    pub origin_sender_name: Option<String>,
    pub capabilities: BTreeSet<GrantCapability>,
    /// 申请里命中的未验证 op；空 = 全部 Verified。
    pub unverified: Vec<UnverifiedOp>,
    /// 进程内的创建序号；批量执行按它还原申请顺序。
    pub seq: u64,
    pub created_at: i64,
    pub expires_at: i64,
    pub state: TicketState,
    pub request_turn: String,
    pub batch_id: Option<String>,
    /// 申请时解析出的审批目标（房间 id 列表）。空 = 路由不可达。
    pub approval_route: Vec<String>,
    /// 实际批准/拒绝所在的房间。
    pub approval_room: Option<String>,
    /// 实际批准/拒绝的场所（private / group / console）。
    pub approval_place: Option<String>,
    pub approval_ref: Option<String>,
    pub approver: Option<String>,
    /// 是否在可信群内批准（审计用）。
    pub trusted_group: bool,
    pub used_at: Option<i64>,
    /// 审批私信是否已经把这张票据列给管理员。同轮多张票据在收工时聚合发送。
    pub announced: bool,
    /// 最近一次路由不可达的原因；发送成功后清空。
    pub route_unavailable: Option<String>,
    pub route_unavailable_at: Option<i64>,
    /// core 执行状态：succeeded / failed / skipped。
    pub execution_status: Option<String>,
    pub executed_at: Option<i64>,
    pub result: Option<Value>,
    pub failure_reason: Option<String>,
    pub audit_ref: Option<String>,
}

/// 一次申请的全部输入。
#[derive(Debug, Clone)]
pub struct TicketRequest {
    pub principal: GrantSubject,
    pub place: Place,
    pub session: String,
    pub request_room: RoomId,
    pub target: AccessTarget,
    pub ops: Vec<String>,
    pub params: Option<Value>,
    pub origin_sender: Option<String>,
    pub origin_sender_name: Option<String>,
    pub capabilities: BTreeSet<GrantCapability>,
    pub unverified: Vec<UnverifiedOp>,
    pub request_turn: String,
    pub batch_id: Option<String>,
    /// 申请时解析出的审批目标；空 = 路由不可达，不允许退回申请房间。
    pub approval_route: Vec<String>,
    pub now: i64,
}

/// 一次批准/拒绝/撤销的场所与身份，用于审计与路由去重。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalMeta {
    pub approval_room: String,
    pub approval_place: String,
    pub approver: String,
    pub approval_ref: String,
    pub trusted_group: bool,
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

/// 批量申请的结果：一个 batch id + N 张独立票据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchReply {
    pub batch_id: String,
    pub tickets: Vec<TicketReply>,
}

/// 进程内的票据簿。所有方法都在 core 的全局锁内调用，不需要额外同步。
#[derive(Debug, Default)]
pub struct AccessTickets {
    tickets: HashMap<String, AccessTicket>,
    seq: u64,
    /// 会话 → 最近一条审批私信时间。冷却按「发过消息」算，而不是按原始申请算：
    /// 同一轮里连续申请的多张票据仍然可以聚合成一条消息。
    session_notified_at: HashMap<String, i64>,
    /// 最近一条发给 system admin 的审批私信时间。
    admin_notified_at: Option<i64>,
    /// 审批人 → 窗口内 nonce 失败时间戳；用于错 nonce 计数与临时拒绝。
    nonce_failures: HashMap<String, Vec<i64>>,
    #[cfg(test)]
    audit_log: Vec<String>,
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
                && ticket.params == request.params
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

        Ok(self.insert(request))
    }

    /// 批量申请：N 张独立票据共享一个 batch id。任何一条校验失败都拒绝整批。
    pub fn request_batch(
        &mut self,
        requests: Vec<TicketRequest>,
        now: i64,
    ) -> Result<BatchReply, String> {
        if requests.is_empty() {
            return Err("batch must contain at least one item".to_owned());
        }
        if requests.len() > MAX_ITEMS_PER_BATCH {
            return Err(format!(
                "batch has {} items (max {MAX_ITEMS_PER_BATCH}); whole batch rejected",
                requests.len()
            ));
        }
        self.sweep(now);

        let session = requests[0].session.clone();
        let turn = requests[0].request_turn.clone();
        let mut seen = BTreeSet::new();
        for request in &requests {
            let ops = normalize_ops(&request.ops)?;
            if request.capabilities.is_empty() {
                return Err("capability set must not be empty".to_owned());
            }
            if request.session != session || request.request_turn != turn {
                return Err("batch items must share session and turn".to_owned());
            }
            let key = (
                request.target.to_string(),
                ops.join("|"),
                request
                    .params
                    .as_ref()
                    .map(Value::to_string)
                    .unwrap_or_default(),
            );
            if !seen.insert(key) {
                return Err(
                    "batch contains duplicate target + ops; whole batch rejected".to_owned(),
                );
            }
            if self.tickets.values().any(|ticket| {
                ticket.state.live()
                    && ticket.session == request.session
                    && ticket.target == request.target
                    && ticket.ops == ops
                    && ticket.params == request.params
                    && ticket.capabilities == request.capabilities
            }) {
                return Err("batch item already has a live ticket; whole batch rejected".to_owned());
            }
        }

        let groups = self
            .tickets
            .values()
            .filter(|ticket| ticket.state.live() && ticket.request_turn == turn)
            .map(|ticket| ticket.batch_id.clone().unwrap_or_else(|| ticket.id.clone()))
            .collect::<BTreeSet<_>>();
        if groups.len() >= MAX_REQUESTS_PER_TURN {
            return Err(format!(
                "turn {turn} already has {} live request groups (max {MAX_REQUESTS_PER_TURN})",
                groups.len()
            ));
        }
        let session_count = self
            .tickets
            .values()
            .filter(|ticket| ticket.state.live() && ticket.session == session)
            .count();
        if session_count + requests.len() > MAX_LIVE_PER_SESSION {
            return Err(format!(
                "session {session} would have {} live tickets (max {MAX_LIVE_PER_SESSION}); whole batch rejected",
                session_count + requests.len()
            ));
        }

        let batch_id = format!("bt-{}", random_hex());
        let mut tickets = Vec::with_capacity(requests.len());
        for mut request in requests {
            request.batch_id = Some(batch_id.clone());
            tickets.push(self.insert(request));
        }
        Ok(BatchReply { batch_id, tickets })
    }

    fn insert(&mut self, request: TicketRequest) -> TicketReply {
        self.seq = self.seq.saturating_add(1);
        let id = format!("tk-{}-{}", random_hex(), self.seq);
        let nonce = random_hex();
        let ops = normalize_ops(&request.ops).unwrap_or_else(|_| request.ops.clone());
        let ticket = AccessTicket {
            id: id.clone(),
            nonce: nonce.clone(),
            principal: request.principal,
            place: request.place,
            session: request.session,
            request_room: request.request_room,
            target: request.target,
            ops,
            params: request.params,
            origin_sender: request.origin_sender,
            origin_sender_name: request.origin_sender_name,
            capabilities: request.capabilities,
            unverified: request.unverified,
            seq: self.seq,
            created_at: request.now,
            expires_at: request.now.saturating_add(PENDING_TTL_MS),
            state: TicketState::PendingApproval,
            request_turn: request.request_turn,
            batch_id: request.batch_id,
            approval_route: request.approval_route,
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
        reply
    }

    /// 同一会话距离下次可申请还剩多少毫秒。冷却从「最近一条审批私信」算起；
    /// 同轮里还没有发出消息的 pending 不会互相阻塞，方便聚合。
    pub fn session_cooldown_remaining(
        &self,
        session: &str,
        now: i64,
        cooldown_ms: i64,
    ) -> Option<i64> {
        if cooldown_ms <= 0 {
            return None;
        }
        let last = self.session_notified_at.get(session)?;
        let remaining = cooldown_ms.saturating_sub(now.saturating_sub(*last));
        (remaining > 0).then_some(remaining)
    }

    /// 同一个管理员距离可以再收一条审批私信还剩多少毫秒。
    pub fn admin_cooldown_remaining(&self, now: i64, cooldown_ms: i64) -> Option<i64> {
        if cooldown_ms <= 0 {
            return None;
        }
        let last = self.admin_notified_at?;
        let remaining = cooldown_ms.saturating_sub(now.saturating_sub(last));
        (remaining > 0).then_some(remaining)
    }

    /// 收集所有还没发给管理员的 pending 票据。调用方发出私信后再 `mark_announced`；
    /// 发送失败就保持未标记，下一次收工再试。
    pub fn pending_unannounced(&mut self, now: i64) -> Vec<AccessTicket> {
        self.sweep(now);
        let mut pending: Vec<AccessTicket> = self
            .tickets
            .values()
            .filter(|ticket| ticket.state == TicketState::PendingApproval && !ticket.announced)
            .cloned()
            .collect();
        pending.sort_by_key(|ticket| (ticket.created_at, ticket.seq));
        pending
    }

    /// 审批私信已发出：标记这些 pending 已通知，并推进会话/管理员冷却。
    pub fn mark_announced(&mut self, ids: &[String], now: i64) {
        let mut sessions = BTreeSet::new();
        for id in ids {
            if let Some(ticket) = self.tickets.get_mut(id) {
                if ticket.state == TicketState::PendingApproval {
                    ticket.announced = true;
                    ticket.route_unavailable = None;
                    ticket.route_unavailable_at = None;
                    sessions.insert(ticket.session.clone());
                }
            }
        }
        for session in sessions {
            self.session_notified_at.insert(session, now);
        }
        if !ids.is_empty() {
            self.admin_notified_at = Some(now);
        }
    }

    /// 记录一次审批路由不可达；不改变票据状态，下一次收工继续重试。
    pub fn mark_route_unavailable(&mut self, ids: &[String], reason: &str, now: i64) {
        for id in ids {
            if let Some(ticket) = self.tickets.get_mut(id) {
                if ticket.state == TicketState::PendingApproval {
                    ticket.route_unavailable = Some(reason.to_owned());
                    ticket.route_unavailable_at = Some(now);
                }
            }
        }
    }

    /// 记录 core 执行结果。active 票据执行后置终态 `used`（即使失败也不再复用）；
    /// 已经过期/撤销/用过的票据保持原终态，只补执行字段，不把终态改写成 used。
    pub fn complete_execution(
        &mut self,
        id: &str,
        status: &str,
        result: Option<Value>,
        failure: Option<String>,
        audit_ref: &str,
        now: i64,
    ) -> Result<(), String> {
        let snapshot = {
            let ticket = self
                .tickets
                .get_mut(id)
                .ok_or_else(|| format!("unknown ticket {id}"))?;
            if ticket.state == TicketState::Active {
                ticket.state = TicketState::Used;
                ticket.used_at = Some(now);
            }
            ticket.execution_status = Some(status.to_owned());
            ticket.executed_at = Some(now);
            ticket.result = result;
            ticket.failure_reason = failure;
            ticket.audit_ref = Some(audit_ref.to_owned());
            ticket.nonce.clear();
            ticket.params = None;
            ticket.snapshot()
        };
        self.audit("execute", &snapshot, status);
        Ok(())
    }

    /// 按 batch id 取票据（只读，按创建时间排序）。
    pub fn for_batch(&self, batch_id: &str) -> Vec<&AccessTicket> {
        let mut tickets: Vec<&AccessTicket> = self
            .tickets
            .values()
            .filter(|ticket| ticket.batch_id.as_deref() == Some(batch_id))
            .collect();
        tickets.sort_by_key(|ticket| (ticket.created_at, ticket.seq));
        tickets
    }

    /// 整批批准（带审批场所元数据）。
    pub fn approve_batch_with_meta(
        &mut self,
        batch_id: &str,
        meta: &ApprovalMeta,
        now: i64,
    ) -> Result<Vec<TicketReply>, String> {
        self.sweep(now);
        let ids: Vec<String> = self
            .for_batch(batch_id)
            .into_iter()
            .map(|ticket| ticket.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(format!("unknown batch {batch_id}"));
        }
        if ids.iter().any(|id| {
            self.tickets
                .get(id)
                .is_none_or(|ticket| ticket.state != TicketState::PendingApproval)
        }) {
            return Err(format!(
                "batch {batch_id} contains a ticket that is not pending_approval; whole batch rejected"
            ));
        }
        let mut replies = Vec::with_capacity(ids.len());
        for id in &ids {
            let ticket = self.tickets.get_mut(id).expect("ids came from the map");
            ticket.state = TicketState::Active;
            ticket.expires_at = now.saturating_add(ACTIVE_TTL_MS);
            apply_approval(ticket, meta);
            ticket.nonce.clear();
            let snapshot = ticket.snapshot();
            self.audit("approve", &snapshot, "approved");
            replies.push(reply(&snapshot, false));
        }
        self.audit_batch(
            "approve",
            batch_id,
            "active",
            ids.len(),
            &meta.approval_ref,
            "batch approved",
        );
        Ok(replies)
    }

    /// 整批批准：逐张 pending → active；任何一张不是 pending 都拒绝整批。
    pub fn approve_batch(
        &mut self,
        batch_id: &str,
        approval_ref: &str,
        approver: &str,
        now: i64,
    ) -> Result<Vec<TicketReply>, String> {
        self.sweep(now);
        let ids: Vec<String> = self
            .for_batch(batch_id)
            .into_iter()
            .map(|ticket| ticket.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(format!("unknown batch {batch_id}"));
        }
        if ids.iter().any(|id| {
            self.tickets
                .get(id)
                .is_none_or(|ticket| ticket.state != TicketState::PendingApproval)
        }) {
            return Err(format!(
                "batch {batch_id} contains a ticket that is not pending_approval; whole batch rejected"
            ));
        }
        let mut replies = Vec::with_capacity(ids.len());
        for id in &ids {
            let ticket = self.tickets.get_mut(id).expect("ids came from the map");
            ticket.state = TicketState::Active;
            ticket.expires_at = now.saturating_add(ACTIVE_TTL_MS);
            ticket.approval_ref = Some(approval_ref.to_owned());
            ticket.approver = Some(approver.to_owned());
            ticket.nonce.clear();
            let snapshot = ticket.snapshot();
            self.audit("approve", &snapshot, "approved");
            replies.push(reply(&snapshot, false));
        }
        self.audit_batch(
            "approve",
            batch_id,
            "active",
            ids.len(),
            approval_ref,
            "batch approved",
        );
        Ok(replies)
    }

    /// 整批拒绝（带审批场所元数据）。
    pub fn deny_batch_with_meta(
        &mut self,
        batch_id: &str,
        meta: &ApprovalMeta,
        now: i64,
    ) -> Result<Vec<TicketReply>, String> {
        self.sweep(now);
        let ids: Vec<String> = self
            .for_batch(batch_id)
            .into_iter()
            .map(|ticket| ticket.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(format!("unknown batch {batch_id}"));
        }
        if ids.iter().any(|id| {
            self.tickets
                .get(id)
                .is_none_or(|ticket| ticket.state != TicketState::PendingApproval)
        }) {
            return Err(format!(
                "batch {batch_id} contains a ticket that is not pending_approval; whole batch rejected"
            ));
        }
        let mut replies = Vec::with_capacity(ids.len());
        for id in &ids {
            let ticket = self.tickets.get_mut(id).expect("ids came from the map");
            ticket.state = TicketState::Denied;
            apply_approval(ticket, meta);
            ticket.nonce.clear();
            ticket.params = None;
            let snapshot = ticket.snapshot();
            self.audit("deny", &snapshot, "denied");
            replies.push(reply(&snapshot, false));
        }
        self.audit_batch(
            "deny",
            batch_id,
            "denied",
            ids.len(),
            &meta.approval_ref,
            "batch denied",
        );
        Ok(replies)
    }

    /// 整批拒绝：逐张 pending → denied；任何一张不是 pending 都拒绝整批。
    pub fn deny_batch(
        &mut self,
        batch_id: &str,
        approval_ref: &str,
        approver: &str,
        now: i64,
    ) -> Result<Vec<TicketReply>, String> {
        self.sweep(now);
        let ids: Vec<String> = self
            .for_batch(batch_id)
            .into_iter()
            .map(|ticket| ticket.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(format!("unknown batch {batch_id}"));
        }
        if ids.iter().any(|id| {
            self.tickets
                .get(id)
                .is_none_or(|ticket| ticket.state != TicketState::PendingApproval)
        }) {
            return Err(format!(
                "batch {batch_id} contains a ticket that is not pending_approval; whole batch rejected"
            ));
        }
        let mut replies = Vec::with_capacity(ids.len());
        for id in &ids {
            let ticket = self.tickets.get_mut(id).expect("ids came from the map");
            ticket.state = TicketState::Denied;
            ticket.approval_ref = Some(approval_ref.to_owned());
            ticket.approver = Some(approver.to_owned());
            ticket.nonce.clear();
            ticket.params = None;
            let snapshot = ticket.snapshot();
            self.audit("deny", &snapshot, "denied");
            replies.push(reply(&snapshot, false));
        }
        self.audit_batch(
            "deny",
            batch_id,
            "denied",
            ids.len(),
            approval_ref,
            "batch denied",
        );
        Ok(replies)
    }

    /// 单张批准（带审批场所元数据）。`nonce` 必须与 pending 票据完全一致。
    pub fn approve_with_meta(
        &mut self,
        id: &str,
        nonce: &str,
        meta: &ApprovalMeta,
        now: i64,
    ) -> Result<TicketReply, String> {
        self.sweep(now);
        self.check_nonce_attempts(&meta.approver, now)?;
        let (snapshot, nonce_matched) = {
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
                (ticket.snapshot(), false)
            } else {
                ticket.state = TicketState::Active;
                ticket.expires_at = now.saturating_add(ACTIVE_TTL_MS);
                apply_approval(ticket, meta);
                ticket.nonce.clear();
                (ticket.snapshot(), true)
            }
        };
        if !nonce_matched {
            let failures = self.record_nonce_failure(&meta.approver, now);
            let reason = format!(
                "nonce mismatch ({failures}/{NONCE_FAILURE_LIMIT} attempts in {} min)",
                NONCE_FAILURE_WINDOW_MS / 60_000
            );
            self.audit("deny", &snapshot, &reason);
            return Err("nonce mismatch".to_owned());
        }
        self.nonce_failures.remove(&meta.approver);
        self.audit("approve", &snapshot, "approved");
        Ok(reply(&snapshot, false))
    }

    /// 错 nonce 超过阈值后临时拒绝；窗口随时间滑动。
    fn check_nonce_attempts(&mut self, approver: &str, now: i64) -> Result<(), String> {
        let window_start = now.saturating_sub(NONCE_FAILURE_WINDOW_MS);
        let failures = match self.nonce_failures.get_mut(approver) {
            Some(attempts) => {
                attempts.retain(|at| *at > window_start);
                attempts.len()
            }
            None => 0,
        };
        if failures >= NONCE_FAILURE_LIMIT {
            return Err(format!(
                "nonce failure limit reached ({failures}/{NONCE_FAILURE_LIMIT} in {} min); approval temporarily rejected",
                NONCE_FAILURE_WINDOW_MS / 60_000
            ));
        }
        Ok(())
    }

    /// 记一次错 nonce，返回窗口内累计次数。
    fn record_nonce_failure(&mut self, approver: &str, now: i64) -> usize {
        let window_start = now.saturating_sub(NONCE_FAILURE_WINDOW_MS);
        let attempts = self.nonce_failures.entry(approver.to_owned()).or_default();
        attempts.push(now);
        attempts.retain(|at| *at > window_start);
        attempts.len()
    }

    /// 单张拒绝（带审批场所元数据）。
    pub fn deny_with_meta(
        &mut self,
        id: &str,
        nonce: &str,
        meta: &ApprovalMeta,
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
            apply_approval(ticket, meta);
            ticket.nonce.clear();
            ticket.params = None;
            ticket.snapshot()
        };
        self.audit("deny", &snapshot, "denied");
        Ok(reply(&snapshot, false))
    }

    /// 撤销（带审批场所元数据）。
    pub fn revoke_with_meta(
        &mut self,
        id: &str,
        meta: &ApprovalMeta,
        now: i64,
    ) -> Result<TicketReply, String> {
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
            apply_approval(ticket, meta);
            ticket.nonce.clear();
            ticket.params = None;
            ticket.snapshot()
        };
        self.audit("revoke", &snapshot, "revoked");
        Ok(reply(&snapshot, false))
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
            ticket.params = None;
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
            ticket.params = None;
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
        target: &AccessTarget,
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
            .min_by_key(|ticket| (ticket.created_at, ticket.seq))
            .map(|ticket| ticket.id.clone())?;
        let snapshot = {
            let ticket = self.tickets.get_mut(&id)?;
            ticket.state = TicketState::Used;
            ticket.used_at = Some(now);
            ticket.nonce.clear();
            ticket.params = None;
            ticket.snapshot()
        };
        self.audit("use", &snapshot, "used");
        Some(id)
    }

    pub fn get(&self, id: &str) -> Option<&AccessTicket> {
        self.tickets.get(id)
    }

    /// 本会话 live（pending + active）票据数，只读、不触发过期扫描。
    pub fn live_counts(&self, session: &str) -> (usize, usize) {
        let mut pending = 0;
        let mut active = 0;
        for ticket in self.tickets.values() {
            if ticket.session != session {
                continue;
            }
            match ticket.state {
                TicketState::PendingApproval => pending += 1,
                TicketState::Active => active += 1,
                _ => {}
            }
        }
        (pending, active)
    }

    /// 本会话在本轮已经占用的申请组数（单张票据算一组，批量按 batch_id 算一组）。
    pub fn live_groups_for_turn(&self, turn: &str) -> usize {
        self.tickets
            .values()
            .filter(|ticket| ticket.state.live() && ticket.request_turn == turn)
            .map(|ticket| ticket.batch_id.as_deref().unwrap_or(ticket.id.as_str()))
            .collect::<BTreeSet<_>>()
            .len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.tickets.len()
    }

    /// 过期扫描：pending/active 到点即失效。每次请求/批准/使用前都会跑一次。
    pub(crate) fn sweep(&mut self, now: i64) {
        let mut expired = Vec::new();
        for ticket in self.tickets.values_mut() {
            if ticket.state.live() && ticket.expires_at <= now {
                ticket.state = TicketState::Expired;
                ticket.nonce.clear();
                ticket.params = None;
                expired.push(ticket.snapshot());
            }
        }
        for ticket in &expired {
            self.audit("expire", ticket, "expired");
        }
        let failure_window_start = now.saturating_sub(NONCE_FAILURE_WINDOW_MS);
        self.nonce_failures.retain(|_, attempts| {
            attempts.retain(|at| *at > failure_window_start);
            !attempts.is_empty()
        });
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

    fn audit(&mut self, event: &str, ticket: &AccessTicket, reason: &str) {
        let capabilities = ticket
            .capabilities
            .iter()
            .map(|capability| capability.as_str())
            .collect::<Vec<_>>()
            .join("|");
        let ops = ticket.ops.join("|");
        let line = format!(
            "[access-ticket] event={} ticket={} batch={} state={} principal={} place={} capability={} target={} ops={} session={} turn={} sender={} sender_name={} approval_ref={} approval_route={} approval_room={} approval_place={} trusted_group={} unverified={} reason=\"{}\"",
            event,
            ticket.id,
            ticket.batch_id.as_deref().unwrap_or("-"),
            ticket.state.as_str(),
            ticket.principal.as_str(),
            ticket.place.as_str(),
            capabilities,
            truncate_target(&ticket.target.display()),
            ops,
            ticket.session,
            ticket.request_turn,
            ticket.origin_sender.as_deref().unwrap_or("-"),
            ticket.origin_sender_name.as_deref().unwrap_or("-"),
            ticket.approval_ref.as_deref().unwrap_or("-"),
            ticket.approval_route.join("|"),
            ticket.approval_room.as_deref().unwrap_or("-"),
            ticket.approval_place.as_deref().unwrap_or("-"),
            ticket.trusted_group,
            !ticket.unverified.is_empty(),
            reason.replace('"', "'"),
        );
        eprintln!("{line}");
        #[cfg(test)]
        self.audit_log.push(line);
    }

    fn audit_batch(
        &mut self,
        event: &str,
        batch_id: &str,
        state: &str,
        count: usize,
        approval_ref: &str,
        reason: &str,
    ) {
        let line = format!(
            "[access-ticket] event={} batch={} state={} count={} approval_ref={} reason=\"{}\"",
            event,
            batch_id,
            state,
            count,
            approval_ref,
            reason.replace('"', "'"),
        );
        eprintln!("{line}");
        #[cfg(test)]
        self.audit_log.push(line);
    }
}

impl AccessTicket {
    fn snapshot(&self) -> Self {
        self.clone()
    }
}

fn apply_approval(ticket: &mut AccessTicket, meta: &ApprovalMeta) {
    ticket.approval_room = Some(meta.approval_room.clone());
    ticket.approval_place = Some(meta.approval_place.clone());
    ticket.approval_ref = Some(meta.approval_ref.clone());
    ticket.approver = Some(meta.approver.clone());
    ticket.trusted_group = meta.trusted_group;
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
    // 固定长度读取 OS 熵源，绝不无界读；32 字节只取前 8 字节组成 16 hex。
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .expect("open and read 32 bytes from /dev/urandom");
    format!(
        "{:016x}",
        u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> AccessTarget {
        AccessTarget::room(&RoomId::parse("qq:group/999").expect("房间"))
    }

    fn meta(approver: &str) -> ApprovalMeta {
        ApprovalMeta {
            approval_room: "qq:private/1".to_owned(),
            approval_place: "private".to_owned(),
            approver: approver.to_owned(),
            approval_ref: "qq:private/1#9".to_owned(),
            trusted_group: false,
        }
    }

    fn request(now: i64, ops: &[&str]) -> TicketRequest {
        TicketRequest {
            principal: GrantSubject::User,
            place: Place::Room(crate::grants::RoomScope::MultiParty),
            session: "main".to_owned(),
            request_room: RoomId::parse("qq:group/123").expect("房间"),
            target: target(),
            ops: ops.iter().map(|op| (*op).to_owned()).collect(),
            params: Some(serde_json::json!({"elements": [{"type": "text", "text": "x"}]})),
            origin_sender: Some("user1".to_owned()),
            origin_sender_name: Some("群友一".to_owned()),
            capabilities: BTreeSet::from([GrantCapability::RoomWrite]),
            unverified: Vec::new(),
            request_turn: "turn-1".to_owned(),
            batch_id: None,
            approval_route: vec!["qq:private/10002".to_owned()],
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
    fn pending_ticket_expires_after_ttl_without_approval() {
        let mut tickets = AccessTickets::new();
        let pending = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        assert_eq!(
            tickets.get(&pending.id).expect("在簿上").state,
            TicketState::PendingApproval
        );
        tickets.sweep(1000 + PENDING_TTL_MS);
        assert_eq!(
            tickets.get(&pending.id).expect("还在簿上").state,
            TicketState::Expired,
            "PENDING_TTL 到点必须自然过期"
        );
        assert!(
            tickets
                .approve(
                    &pending.id,
                    &pending.nonce,
                    "ref",
                    "system_admin",
                    1000 + PENDING_TTL_MS
                )
                .is_err(),
            "过期票据不能再批准"
        );
    }

    #[test]
    fn used_and_expired_tickets_are_rejected() {
        let mut tickets = AccessTickets::new();
        let used = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        tickets
            .approve(&used.id, &used.nonce, "ref", "system_admin", 2000)
            .expect("批准");
        tickets
            .authorize(
                "main",
                &target(),
                GrantCapability::RoomWrite,
                "send_message_to",
                2100,
            )
            .expect("第一次使用");
        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomWrite,
                    "send_message_to",
                    2101
                )
                .is_none(),
            "used 后再次使用必须拒绝"
        );
        assert_eq!(
            tickets.get(&used.id).expect("还在簿上").state,
            TicketState::Used
        );

        let expired = tickets
            .request(request(3000, &["send_message_to"]))
            .expect("申请");
        tickets
            .approve(&expired.id, &expired.nonce, "ref", "system_admin", 4000)
            .expect("批准");
        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomWrite,
                    "send_message_to",
                    4000 + ACTIVE_TTL_MS + 1
                )
                .is_none(),
            "active 过期后必须拒绝"
        );
        assert_eq!(
            tickets.get(&expired.id).expect("还在簿上").state,
            TicketState::Expired
        );
    }

    #[test]
    fn expired_execution_is_skipped_and_state_is_kept() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        tickets
            .approve(&reply.id, &reply.nonce, "ref", "system_admin", 2000)
            .expect("批准");
        let now = 2000 + ACTIVE_TTL_MS + 1;
        tickets.sweep(now);
        tickets
            .complete_execution(
                &reply.id,
                "skipped",
                None,
                Some("ticket is expired".to_owned()),
                "ref",
                now,
            )
            .expect("记录跳过");
        let ticket = tickets.get(&reply.id).expect("还在簿上");
        assert_eq!(ticket.state, TicketState::Expired, "终态不能被改写成 used");
        assert_eq!(ticket.execution_status.as_deref(), Some("skipped"));
        assert_eq!(ticket.failure_reason.as_deref(), Some("ticket is expired"));
    }

    #[test]
    fn active_execution_consumes_the_ticket() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        tickets
            .approve(&reply.id, &reply.nonce, "ref", "system_admin", 2000)
            .expect("批准");
        tickets
            .complete_execution(&reply.id, "succeeded", None, None, "ref", 2100)
            .expect("记录执行");
        let ticket = tickets.get(&reply.id).expect("还在簿上");
        assert_eq!(ticket.state, TicketState::Used);
        assert_eq!(ticket.execution_status.as_deref(), Some("succeeded"));
        assert_eq!(ticket.executed_at, Some(2100));
        assert!(ticket.params.is_none(), "终态释放参数快照");
        assert!(
            tickets
                .authorize(
                    "main",
                    &target(),
                    GrantCapability::RoomWrite,
                    "send_message_to",
                    2101
                )
                .is_none(),
            "core 执行后票据已消费"
        );
    }

    #[test]
    fn cooldowns_block_until_the_window_passes() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        let pending = tickets.pending_unannounced(1000);
        let ids: Vec<String> = pending.iter().map(|ticket| ticket.id.clone()).collect();
        assert_eq!(ids.len(), 1);
        tickets.mark_announced(&ids, 1000);
        assert_eq!(
            tickets.session_cooldown_remaining("main", 2000, 60_000),
            Some(59_000)
        );
        assert_eq!(
            tickets.session_cooldown_remaining("main", 61_000, 60_000),
            None,
            "窗口过后恢复"
        );
        assert_eq!(tickets.admin_cooldown_remaining(2000, 30_000), Some(29_000));
        assert_eq!(tickets.admin_cooldown_remaining(31_000, 30_000), None);
        assert_eq!(reply.state, TicketState::PendingApproval);
    }

    #[test]
    fn batch_approve_sets_each_ticket_active_and_audits() {
        let mut tickets = AccessTickets::new();
        let requests = vec![
            request(1000, &["send_message_to"]),
            TicketRequest {
                target: AccessTarget::room(&RoomId::parse("qq:group/998").expect("房间")),
                ..request(1000, &["get_history"])
            },
        ];
        let batch = tickets.request_batch(requests, 1000).expect("批量申请");
        assert_eq!(batch.tickets.len(), 2);
        let ids: Vec<String> = batch
            .tickets
            .iter()
            .map(|ticket| ticket.id.clone())
            .collect();
        tickets.pending_unannounced(1000);
        tickets.mark_announced(&ids, 1000);
        let replies = tickets
            .approve_batch(&batch.batch_id, "ref", "system_admin", 2000)
            .expect("整批批准");
        assert_eq!(replies.len(), 2);
        for reply in &replies {
            assert_eq!(
                tickets.get(&reply.id).expect("还在簿上").state,
                TicketState::Active
            );
        }
        let batch_audit = format!("event=approve batch={}", batch.batch_id);
        assert!(
            tickets
                .audit_log
                .iter()
                .any(|line| line.contains(&batch_audit)),
            "整批批准必须留一条 batch 审计：{:?}",
            tickets.audit_log
        );
        assert!(
            tickets
                .audit_log
                .iter()
                .any(|line| line.contains("sender=user1 sender_name=群友一")),
            "票据审计要带触发者 id 与显示名：{:?}",
            tickets.audit_log
        );
    }

    #[test]
    fn batch_rejects_too_many_or_duplicate_items() {
        let mut tickets = AccessTickets::new();
        let too_many: Vec<TicketRequest> = (0..=MAX_ITEMS_PER_BATCH)
            .map(|index| TicketRequest {
                target: AccessTarget::room(
                    &RoomId::parse(&format!("qq:group/{index}")).expect("房间"),
                ),
                ..request(1000, &["get_history"])
            })
            .collect();
        assert!(
            tickets.request_batch(too_many, 1000).is_err(),
            "超过 8 条必须整批拒绝"
        );
        let duplicate = vec![
            request(1000, &["send_message_to"]),
            request(1000, &["send_message_to"]),
        ];
        assert!(
            tickets.request_batch(duplicate, 1000).is_err(),
            "批内重复目标 + op 必须整批拒绝"
        );
        assert_eq!(tickets.len(), 0, "失败不应留下任何票据");
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
    fn wrong_nonce_is_audited_as_deny_and_only_success_audits_approve() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        let approver = meta("qq:alice");
        let error = tickets
            .approve_with_meta(&reply.id, "not-the-nonce", &approver, 2000)
            .expect_err("错 nonce 必须拒绝");
        assert_eq!(error, "nonce mismatch");
        assert_eq!(
            tickets.get(&reply.id).expect("还在簿上").state,
            TicketState::PendingApproval
        );
        assert!(
            tickets
                .audit_log
                .iter()
                .any(|line| line.contains("event=deny") && line.contains(&reply.id)),
            "错 nonce 必须留 deny 审计：{:?}",
            tickets.audit_log
        );
        assert!(
            !tickets
                .audit_log
                .iter()
                .any(|line| line.contains("event=approve") && line.contains(&reply.id)),
            "校验失败不能记 approve：{:?}",
            tickets.audit_log
        );

        tickets
            .approve_with_meta(&reply.id, &reply.nonce, &approver, 2000)
            .expect("批准");
        assert!(
            tickets
                .audit_log
                .iter()
                .any(|line| line.contains("event=approve") && line.contains(&reply.id)),
            "成功批准才记 approve：{:?}",
            tickets.audit_log
        );
    }

    #[test]
    fn repeated_bad_nonces_are_throttled_per_approver() {
        let mut tickets = AccessTickets::new();
        let reply = tickets
            .request(request(1000, &["send_message_to"]))
            .expect("申请");
        let alice = meta("qq:alice");
        for attempt in 0..NONCE_FAILURE_LIMIT {
            let error = tickets
                .approve_with_meta(&reply.id, "not-the-nonce", &alice, 2000 + attempt as i64)
                .expect_err("错 nonce 必须拒绝");
            assert_eq!(error, "nonce mismatch");
        }
        let throttled = tickets
            .approve_with_meta(&reply.id, &reply.nonce, &alice, 2010)
            .expect_err("超过阈值后即使 nonce 正确也临时拒绝");
        assert!(
            throttled.contains("temporarily rejected"),
            "拒绝原因要说明临时拒绝: {throttled}"
        );

        let approved = tickets
            .approve_with_meta(&reply.id, &reply.nonce, &meta("qq:admin"), 2010)
            .expect("其他审批人不受影响");
        assert_eq!(approved.state, TicketState::Active);
    }

    #[test]
    fn random_hex_is_16_hex_and_unique() {
        let mut seen = BTreeSet::new();
        for _ in 0..1024 {
            let value = random_hex();
            assert_eq!(value.len(), 16, "nonce 仍是 16 hex: {value}");
            assert!(
                value.chars().all(|ch| ch.is_ascii_hexdigit()),
                "nonce 必须是 hex: {value}"
            );
            assert!(seen.insert(value.clone()), "nonce 不能重复: {value}");
        }
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
                    &AccessTarget::room(&RoomId::parse("qq:group/998").expect("房间")),
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
                    target: AccessTarget::room(
                        &RoomId::parse(&format!("qq:group/{index}")).expect("房间"),
                    ),
                    ..request(3000, &["send_message_to"])
                })
                .expect("申请");
        }
        assert!(
            tickets
                .request(TicketRequest {
                    request_turn: "turn-2".to_owned(),
                    target: AccessTarget::room(&RoomId::parse("qq:group/3").expect("房间")),
                    ..request(3000, &["send_message_to"])
                })
                .is_err(),
            "单轮上限"
        );
        assert_eq!(tickets.len(), 1 + MAX_REQUESTS_PER_TURN);
    }
}
