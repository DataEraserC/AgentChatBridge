//! 把进程通道包装成 Root 认识的 access provider。

use std::sync::{Arc, Mutex};

use acb_adapter_api::{
    WireAccessRequest, WireClockRequest, WireResult, WireRouteAction, WireRouteDecision,
    WireRouteRequest, WireTurnErrorAction, WireTurnErrorRequest, WireWakeRequest,
};
use acb_kernel::{
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Capability, ClockContext,
    ClockPolicy, ClockProvider, ExtensionPoint, Health, InterruptScope, ManagerManifest,
    ManagerPlugin, PLUGIN_API_VERSION, PluginError, PluginMetadata, PluginSource,
    ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, RoomId, RouteAction,
    RouteContext, RouteDecision, RoutePolicy, RouteProvider, TurnErrorAction, TurnErrorPolicy,
    TurnErrorProvider, TurnErrorRequest, WakeContext, WakeDecision, WakePolicy, WakeProvider,
    WakeVerdict,
};
use serde_json::json;

use crate::channel::ProcessChannel;

pub(crate) struct ProcessAccessManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessAccessProvider>>>,
}

impl ProcessAccessManager {
    pub(crate) fn new(
        provider: ProviderId,
        metadata: PluginMetadata,
        channel: Arc<ProcessChannel>,
    ) -> Self {
        Self {
            provider,
            metadata,
            channel,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for ProcessAccessManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::AccessDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if !matches!(options, ProviderOptions::Access(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process access provider requires Access options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessAccessProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::Access(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessAccessProvider {
    channel: Arc<ProcessChannel>,
}

impl AccessProvider for ProcessAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        let wire = WireAccessRequest {
            session: request.session.to_owned(),
            connection: request.connection.to_owned(),
            room: request.room.map(|room| room.to_string()),
            origin_room: request.origin_room.map(|room| room.to_string()),
            subject: request.subject.as_str().to_owned(),
            place: request.place.as_str().to_owned(),
            action: request.action.as_str().to_owned(),
            subscriptions: request
                .subscriptions
                .iter()
                .map(|room| room.to_string())
                .collect(),
        };
        let request = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self.channel.decide("access", options, request) {
            Ok(WireResult::Decision { decision, .. }) if decision == "allow" => {
                AccessDecision::Allow
            }
            Ok(WireResult::Decision { decision, .. }) if decision == "deny" => AccessDecision::Deny,
            Ok(WireResult::Decision { decision, .. }) => {
                self.channel
                    .note_invalid_reply(&format!("unknown access decision `{decision}`"));
                AccessDecision::Deny
            }
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected decision reply, got {other:?}"));
                AccessDecision::Deny
            }
            Err(_) => AccessDecision::Deny,
        }
    }
}

/// Wake 扩展点的进程实现（ADR-0033 第二层）。
///
/// 与 access 完全同构：同一个子进程、同一套 `decide` 请求，只是 `point`
/// 换成 `"wake"`。这也是「Process 载体补齐扩展点不需要新协议」的实证——
/// `CoreMessage::Decide` 的 `point` 本来就是字符串。
pub(crate) struct ProcessWakeManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessWakeProvider>>>,
}

impl ProcessWakeManager {
    pub(crate) fn new(
        provider: ProviderId,
        metadata: PluginMetadata,
        channel: Arc<ProcessChannel>,
    ) -> Self {
        Self {
            provider,
            metadata,
            channel,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for ProcessWakeManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if !matches!(options, ProviderOptions::Wake(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process wake provider requires Wake options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessWakeProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::Wake(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessWakeProvider {
    channel: Arc<ProcessChannel>,
}

impl WakeProvider for ProcessWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeVerdict {
        if !matches!(self.channel.health(), Health::Healthy) {
            // 子进程挂了：按「不唤醒」处理，让桥走内置默认，而不是把消息吞掉。
            return WakeVerdict {
                decision: WakeDecision::Stay,
                reason: Some("process provider is down".to_owned()),
                delay_ms: 0,
                batch: false,
            };
        }
        let wire = WireWakeRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            connection: context.connection.to_owned(),
            platform: context.platform.to_owned(),
            sender_id: context.sender_id.to_owned(),
            text: context.text.to_owned(),
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            has_body: context.has_body,
            event: context.event.map(str::to_owned),
            tags: context.tags.iter().map(|tag| (*tag).to_owned()).collect(),
            unread: context.unread,
            last_wake_at: context.last_wake_at,
            now: context.now,
        };
        let request = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options_json = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self.channel.decide("wake", options_json, request) {
            Ok(WireResult::Decision {
                decision,
                reason,
                delay_ms,
                batch,
            }) => {
                let parsed = match decision.as_str() {
                    "wake" => WakeDecision::Wake,
                    other => {
                        if other != "stay" {
                            self.channel
                                .note_invalid_reply(&format!("unknown wake decision `{other}`"));
                        }
                        WakeDecision::Stay
                    }
                };
                WakeVerdict {
                    decision: parsed,
                    reason,
                    delay_ms,
                    batch,
                }
            }
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected decision reply, got {other:?}"));
                WakeVerdict {
                    decision: WakeDecision::Stay,
                    reason: Some("process provider replied with an unexpected shape".to_owned()),
                    delay_ms: 0,
                    batch: false,
                }
            }
            Err(detail) => WakeVerdict {
                decision: WakeDecision::Stay,
                reason: Some(detail),
                delay_ms: 0,
                batch: false,
            },
        }
    }
}

/// Clock 扩展点的进程实现（ADR-0033 第二层）。
///
/// `ClockProvider::is_due` 只回一个 bool，wire 上仍复用 `WireResult::Decision`：
/// `due` / `wake` 为真，`skip` / `stay` 为假。与 Lua 载体接受的那两个词一致，
/// 免得同一个策略在两种载体里要说不同的词。
pub(crate) struct ProcessClockManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessClockProvider>>>,
}

impl ProcessClockManager {
    pub(crate) fn new(
        provider: ProviderId,
        metadata: PluginMetadata,
        channel: Arc<ProcessChannel>,
    ) -> Self {
        Self {
            provider,
            metadata,
            channel,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for ProcessClockManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::ClockDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if !matches!(options, ProviderOptions::Clock(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process clock provider requires Clock options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessClockProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::Clock(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessClockProvider {
    channel: Arc<ProcessChannel>,
}

impl ClockProvider for ProcessClockProvider {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool {
        // 子进程挂了就当「不该醒」：时钟轮是主动行为，载体不可用时静默跳过
        // 比凭空开一轮安全。
        if !matches!(self.channel.health(), Health::Healthy) {
            return false;
        }
        let wire = WireClockRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            now: context.now,
            last_clock_at: context.last_clock_at,
            last_wake_at: context.last_wake_at,
            last_record_at: context.last_record_at,
        };
        let request = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options_json = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self.channel.decide("clock", options_json, request) {
            Ok(WireResult::Decision { decision, .. }) => match decision.as_str() {
                "due" | "wake" => true,
                "skip" | "stay" => false,
                other => {
                    self.channel
                        .note_invalid_reply(&format!("unknown clock decision `{other}`"));
                    false
                }
            },
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected decision reply, got {other:?}"));
                false
            }
            Err(_) => false,
        }
    }
}

/// Route 扩展点的进程实现（ADR-0033 第二层）。
///
/// route 与其它三个不同：判定结果带**动作**（发消息 / 跑任务 / 打断 / 唤醒），
/// 所以回执不是 `Decision { decision: String }` 而是结构化的
/// `WireResult::Route { decision }`。这层不把它压成字符串——动作有参数，
/// 压成字符串就是把协议变成解析题。
pub(crate) struct ProcessRouteManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessRouteProvider>>>,
}

impl ProcessRouteManager {
    pub(crate) fn new(
        provider: ProviderId,
        metadata: PluginMetadata,
        channel: Arc<ProcessChannel>,
    ) -> Self {
        Self {
            provider,
            metadata,
            channel,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for ProcessRouteManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::RouteDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if !matches!(options, ProviderOptions::Route(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process route provider requires Route options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessRouteProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::Route(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessRouteProvider {
    channel: Arc<ProcessChannel>,
}

impl RouteProvider for ProcessRouteProvider {
    fn decide(&self, options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        // 载体不可用时 `Pass`：把消息交回正常流程（入库、按 wake 策略决定是否
        // 唤醒），比 `Drop` 安全——静默吃掉消息是更难查的故障。
        if !matches!(self.channel.health(), Health::Healthy) {
            return RouteDecision::Pass;
        }
        let wire = WireRouteRequest {
            connection: context.connection.to_owned(),
            platform: context.platform.to_owned(),
            room: context.room.to_string(),
            sender_id: context.sender.id.clone(),
            sender_role: sender_role_name(context.sender.role.as_ref()).to_owned(),
            is_admin: context.is_admin,
            kind: record_kind_name(context.kind).to_owned(),
            elements: context.elements.to_vec(),
            text: context.text.to_owned(),
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            has_body: context.has_body,
            event: context.event.map(str::to_owned),
            tags: context.tags.iter().map(|tag| (*tag).to_owned()).collect(),
            sessions: context.sessions.to_vec(),
        };
        let request = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options_json = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self.channel.decide("route", options_json, request) {
            Ok(WireResult::Route { decision }) => route_decision(decision),
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected route reply, got {other:?}"));
                RouteDecision::Pass
            }
            Err(_) => RouteDecision::Pass,
        }
    }
}

/// 身份 → wire 上的词。与 Lua 载体的 `role_name` 用同一套词，
/// 免得同一个角色在两种载体里要写不同的字符串。
fn sender_role_name(role: Option<&acb_kernel::SenderRole>) -> &'static str {
    match role {
        Some(acb_kernel::SenderRole::SystemAdmin) => "system_admin",
        Some(acb_kernel::SenderRole::Owner) => "owner",
        Some(acb_kernel::SenderRole::Admin) => "admin",
        None => "",
    }
}

/// 记录种类 → wire 上的词。只有两种：消息与事件。
fn record_kind_name(kind: &acb_kernel::RecordKind) -> &'static str {
    match kind {
        acb_kernel::RecordKind::Message => "message",
        acb_kernel::RecordKind::Event { .. } => "event",
    }
}

/// wire 的 route 判定 → core 的 `RouteDecision`。
///
/// 单拎出来是为了能直接测映射：尤其是 `Interrupt` 的三档 scope 与
/// 「房间省略 = 触发房间」这两条容易写错的语义。
fn route_decision(wire: WireRouteDecision) -> RouteDecision {
    match wire {
        WireRouteDecision::Pass => RouteDecision::Pass,
        WireRouteDecision::Drop { reason } => RouteDecision::Drop { reason },
        WireRouteDecision::Handle {
            store,
            wake,
            actions,
        } => RouteDecision::Handle {
            store,
            wake,
            actions: actions.into_iter().filter_map(route_action).collect(),
        },
    }
}

/// 一个 wire 动作 → core 动作。返回 `None` 表示这条动作用不了（房间 id 非法等），
/// 调用方跳过它而不是让整条判定失效——少一个动作好过整条消息不处理。
fn route_action(wire: WireRouteAction) -> Option<RouteAction> {
    match wire {
        WireRouteAction::Interrupt { scope, sessions } => {
            let scope = match scope.as_str() {
                "connection" => InterruptScope::Connection,
                "sessions" => InterruptScope::Sessions(sessions),
                // 未知值按默认（触发房间）：宁可打断范围小一点，
                // 也不要因为拼错一个词就把整条连接的所有会话都掐了。
                _ => InterruptScope::Room,
            };
            Some(RouteAction::Interrupt { scope })
        }
        WireRouteAction::Send { room, elements } => Some(RouteAction::Send {
            room: room.and_then(|text| RoomId::parse(&text).ok()),
            elements,
        }),
        WireRouteAction::Task { handler, params } => Some(RouteAction::Task { handler, params }),
        WireRouteAction::Wake {
            session,
            room,
            reason,
        } => Some(RouteAction::Wake {
            session,
            room: room.and_then(|text| RoomId::parse(&text).ok()),
            reason,
        }),
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use acb_adapter_api::WireRouteAction;

    /// `Pass` / `Drop` 一对一透传。
    #[test]
    fn pass_and_drop_map_directly() {
        assert_eq!(route_decision(WireRouteDecision::Pass), RouteDecision::Pass);
        assert_eq!(
            route_decision(WireRouteDecision::Drop {
                reason: Some("noise".to_owned()),
            }),
            RouteDecision::Drop {
                reason: Some("noise".to_owned()),
            }
        );
    }

    /// `Interrupt` 的三档 scope：拼错的值按 `Room` 兜底。
    ///
    /// 兜底方向要往「影响面小」走：把 `connection` 误判成一整条连接，会掐掉
    /// 别的群的会话；误判成当前房间最多少断一个。
    #[test]
    fn interrupt_scope_falls_back_to_room_on_unknown_value() {
        let scope_of = |scope: &str| match route_action(WireRouteAction::Interrupt {
            scope: scope.to_owned(),
            sessions: vec!["a".to_owned()],
        }) {
            Some(RouteAction::Interrupt { scope }) => scope,
            other => panic!("应当是 Interrupt，实际 {other:?}"),
        };

        assert_eq!(scope_of("room"), InterruptScope::Room);
        assert_eq!(scope_of("connection"), InterruptScope::Connection);
        assert_eq!(
            scope_of("sessions"),
            InterruptScope::Sessions(vec!["a".to_owned()])
        );
        assert_eq!(scope_of("nonsense"), InterruptScope::Room);
    }

    /// `Send` / `Wake` 的 room 省略 = 触发房间（`None`）；非法 id 只丢这一个动作。
    #[test]
    fn optional_room_is_none_or_dropped_action() {
        // 省略 room：保持 None，由 core 解释成触发房间。
        match route_action(WireRouteAction::Send {
            room: None,
            elements: Vec::new(),
        }) {
            Some(RouteAction::Send { room, .. }) => assert!(room.is_none()),
            other => panic!("应当是 Send，实际 {other:?}"),
        }
        match route_action(WireRouteAction::Wake {
            session: None,
            room: None,
            reason: Some("why".to_owned()),
        }) {
            Some(RouteAction::Wake { room, .. }) => assert!(room.is_none()),
            other => panic!("应当是 Wake，实际 {other:?}"),
        }
        // 非法 room id：动作仍然保留，只是 room 变成 None（不是整条丢弃）。
        match route_action(WireRouteAction::Send {
            room: Some("不是房间".to_owned()),
            elements: Vec::new(),
        }) {
            Some(RouteAction::Send { room, .. }) => assert!(room.is_none()),
            other => panic!("应当是 Send，实际 {other:?}"),
        }
    }

    /// `Handle` 的 store / wake 透传，动作按序映射。
    #[test]
    fn handle_carries_flags_and_actions_in_order() {
        let decision = route_decision(WireRouteDecision::Handle {
            store: true,
            wake: false,
            actions: vec![
                WireRouteAction::Task {
                    handler: "b23".to_owned(),
                    params: json!({"url": "x"}),
                },
                WireRouteAction::Interrupt {
                    scope: "room".to_owned(),
                    sessions: Vec::new(),
                },
            ],
        });
        match decision {
            RouteDecision::Handle {
                store,
                wake,
                actions,
            } => {
                assert!(store);
                assert!(!wake);
                assert_eq!(actions.len(), 2);
                assert!(matches!(actions[0], RouteAction::Task { .. }));
                assert!(matches!(actions[1], RouteAction::Interrupt { .. }));
            }
            other => panic!("应当是 Handle，实际 {other:?}"),
        }
    }
}

/// TurnError 扩展点的进程实现（ADR-0033 第二层最后一格）。
///
/// wire 形状早就有了（`WireTurnErrorRequest` / `WireTurnErrorAction` /
/// `WireResult::Actions`），缺的只是载体这一侧的 manager——这正是本次调查
/// 暴露的问题：协议先设计好了，但没有载体把它接上。
pub(crate) struct ProcessTurnErrorManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessTurnErrorProvider>>>,
}

impl ProcessTurnErrorManager {
    pub(crate) fn new(
        provider: ProviderId,
        metadata: PluginMetadata,
        channel: Arc<ProcessChannel>,
    ) -> Self {
        Self {
            provider,
            metadata,
            channel,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for ProcessTurnErrorManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::TurnErrorDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::TurnError,
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if !matches!(options, ProviderOptions::TurnError(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process turn_error provider requires TurnError options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessTurnErrorProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::TurnError(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessTurnErrorProvider {
    channel: Arc<ProcessChannel>,
}

impl TurnErrorProvider for ProcessTurnErrorProvider {
    fn decide(
        &self,
        options: &TurnErrorPolicy,
        request: &TurnErrorRequest<'_>,
    ) -> Vec<TurnErrorAction> {
        // 载体不可用时退回请求里带来的默认动作：失败通知是「出事了要有人知道」
        // 的通道，这时更不能把通知也弄丢。
        if !matches!(self.channel.health(), Health::Healthy) {
            return fallback_turn_error_actions(request);
        }
        let wire = WireTurnErrorRequest {
            session: request.session.to_owned(),
            room: request.room.to_string(),
            error: request.error.to_owned(),
            actions: request.actions.iter().map(turn_error_action_wire).collect(),
            default_actions: request
                .default_actions
                .iter()
                .map(turn_error_action_wire)
                .collect(),
        };
        let request_json = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options_json = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self
            .channel
            .decide("turn_error", options_json, request_json)
        {
            Ok(WireResult::Actions { actions }) => actions
                .into_iter()
                .filter_map(turn_error_action_core)
                .collect(),
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected actions reply, got {other:?}"));
                fallback_turn_error_actions(request)
            }
            Err(_) => fallback_turn_error_actions(request),
        }
    }
}

/// 内置默认选择：会话自己的动作非空就用它，否则用全局默认。
///
/// 载体不可用时沿用这条规则——它是「不出事时桥本来会怎么做」的定义，
/// 这里只是把它当作兜底，不发明新行为。
fn fallback_turn_error_actions(request: &TurnErrorRequest<'_>) -> Vec<TurnErrorAction> {
    if request.actions.is_empty() {
        request.default_actions.to_vec()
    } else {
        request.actions.to_vec()
    }
}

fn turn_error_action_wire(action: &TurnErrorAction) -> WireTurnErrorAction {
    match action {
        TurnErrorAction::Log => WireTurnErrorAction::Log,
        TurnErrorAction::Send { room } => WireTurnErrorAction::Send {
            room: room.to_string(),
        },
    }
}

/// wire 动作 → core 动作。房间 id 解析失败就跳过**这一条**动作，
/// 不让整份通知作废（少发一个房间好过一条都不发）。
fn turn_error_action_core(action: WireTurnErrorAction) -> Option<TurnErrorAction> {
    match action {
        WireTurnErrorAction::Log => Some(TurnErrorAction::Log),
        WireTurnErrorAction::Send { room } => RoomId::parse(&room)
            .ok()
            .map(|room| TurnErrorAction::Send { room }),
    }
}

#[cfg(test)]
mod turn_error_tests {
    use super::*;

    /// 动作双向映射：`Log` 与 `Send{room}` 都要能过去再回来。
    #[test]
    fn actions_round_trip() {
        let room = RoomId::new("qq", "group/1").expect("合法房间");
        let wire = turn_error_action_wire(&TurnErrorAction::Log);
        assert_eq!(wire, WireTurnErrorAction::Log);
        assert_eq!(turn_error_action_core(wire), Some(TurnErrorAction::Log));

        let wire = turn_error_action_wire(&TurnErrorAction::Send { room: room.clone() });
        assert_eq!(
            turn_error_action_core(wire),
            Some(TurnErrorAction::Send { room })
        );
    }

    /// 非法房间只用掉这一条动作，不牵连其它。
    #[test]
    fn invalid_room_drops_only_that_action() {
        assert_eq!(
            turn_error_action_core(WireTurnErrorAction::Send {
                room: "不是房间".to_owned(),
            }),
            None
        );
        // Log 不受影响。
        assert_eq!(
            turn_error_action_core(WireTurnErrorAction::Log),
            Some(TurnErrorAction::Log)
        );
    }

    /// 载体不可用时的兜底：沿用内置规则（会话动作优先，否则全局默认）。
    #[test]
    fn fallback_prefers_session_actions_then_defaults() {
        let room = RoomId::new("qq", "private/9").expect("合法房间");
        let session_actions = vec![TurnErrorAction::Send { room: room.clone() }];
        let defaults = vec![TurnErrorAction::Log];

        let with_session = TurnErrorRequest {
            session: "s",
            room: &room,
            error: "boom",
            actions: &session_actions,
            default_actions: &defaults,
        };
        assert_eq!(fallback_turn_error_actions(&with_session), session_actions);

        let without_session = TurnErrorRequest {
            session: "s",
            room: &room,
            error: "boom",
            actions: &[],
            default_actions: &defaults,
        };
        assert_eq!(fallback_turn_error_actions(&without_session), defaults);
    }
}
