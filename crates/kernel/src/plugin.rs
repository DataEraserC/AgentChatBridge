//! 扩展宿主骨架（ADR-0026）。
//!
//! Phase 1 只落地 wake 这一个扩展点：RootManager + NativeLoader + 内置 wake provider。
//! 目的是先把“provider id 解析、能力校验、native loader、manager 子树委托”跑通，
//! 行为与原来写死的 `WakePolicy::decide` 完全一致。
//!
//! 目前已落地的扩展点：wake、access、delivery composer、send hooks、turn-error。
//! 其余（receive / clock / 换投递引擎 / 动态 platform adapter）按 ADR-0026 的 phase
//! 逐个加，不在这里提前造空壳。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use acb_proto::{Element, Omitted, Record, RecordKind, RoomId, Sender, ToolError, TurnReason};
use serde_json::Value;
use thiserror::Error;

use crate::adapter::{
    AdapterError, AdapterFactory, AdapterInstance, AdapterManifest, create_instance,
    validate_manifest,
};
use crate::config::{
    AccessPolicy, ClockPolicy, Config, Connection, DeliveryPolicy, ReceivePolicy, RoutePolicy,
    TurnErrorAction, TurnErrorPolicy, WakeDecision, WakeInput, WakePolicy, WakeVerdict,
};
use crate::event_queue::EventQueue;
use crate::outbound::Sent;
use crate::plugin_meta::{PluginEffect, PluginInfo, PluginLifecycle, PluginMetadata, PluginSource};

/// 插件 API 版本。provider / manager / loader 的 manifest 都要对得上。
pub const PLUGIN_API_VERSION: u32 = 1;

/// 从 `catch_unwind` 的 payload 里取出可读文本，尽量不丢 panic 信息。
pub(crate) fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_owned();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    "non-string panic payload".to_owned()
}

/// 调 manager 的只读自述接口；panic 只当作这次调用失败，不穿过 core。
fn call_manager<T>(
    namespace: &str,
    call: &'static str,
    invoke: impl FnOnce() -> T,
) -> Result<T, PluginError> {
    catch_unwind(AssertUnwindSafe(invoke)).map_err(|payload| PluginError::Manager {
        namespace: namespace.to_owned(),
        detail: acb_i18n::t!(
            "kernel.plugin.manager_call_panicked",
            namespace = namespace,
            call = call,
            panic = panic_text(payload)
        ),
    })
}

/// 读 manager 健康状态；panic 视为 Unhealthy，不让控制台/健康检查线程崩掉。
fn manager_health(namespace: &str, manager: &Arc<dyn ManagerPlugin>) -> Health {
    match catch_unwind(AssertUnwindSafe(|| manager.health())) {
        Ok(health) => health,
        Err(payload) => Health::Unhealthy(acb_i18n::t!(
            "kernel.plugin.manager_health_panicked",
            namespace = namespace,
            panic = panic_text(payload)
        )),
    }
}

/// 观察者只读旁路：单个 panic 只跳过它自己，不能挡住记录入库后的唤醒调度。
fn observe_one(id: &ProviderId, observer: &Arc<dyn RecordObserver>, record: &Record) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| observer.observe(record))) {
        eprintln!(
            "{}",
            acb_i18n::t!(
                "kernel.plugin.observer_panicked",
                id = id,
                panic = panic_text(payload)
            )
        );
    }
}

/// 内置 wake provider 的 id。默认订阅没有显式写 provider 时用它。
pub const DEFAULT_WAKE_PROVIDER: &str = "acb.wake.default";

/// 内置 access provider 的 id。会话没有显式写 provider 时用它。
pub const DEFAULT_ACCESS_PROVIDER: &str = "acb.access.subscribed";

/// 内置投递 composer 的 id。会话没有显式写 delivery provider 时用它。
pub const DEFAULT_DELIVERY_PROVIDER: &str = "acb.delivery.default";

/// 内置失败通知 provider 的 id。会话没有显式写 turn_error provider 时用它。
pub const DEFAULT_TURN_ERROR_PROVIDER: &str = "acb.turn_error.default";

/// 内置「不启用时钟」provider 的 id。订阅没有显式写 clock provider 时用它。
pub const DEFAULT_CLOCK_PROVIDER: &str = "acb.clock.off";

/// 内置「固定间隔 / 房间空闲」时钟 provider 的 id。
pub const CLOCK_INTERVAL_IDLE_PROVIDER: &str = "acb.clock.interval_idle";

/// 内置入站策略：全部接收。
pub const DEFAULT_RECEIVE_PROVIDER: &str = "acb.receive.all";

/// 每轮从最旧端至少带几条（默认 composer 用）。
const DELIVERY_OLDEST: usize = 5;

/// 一轮投递的字节上限：整帧不能超过 1 MiB（协议上限），这里留一半余量给信封和记忆信息。
const MAX_DELIVERY_BYTES: usize = 512 * 1024;

/// 任何房间都允许访问（整个会话豁免订阅边界）。
pub const ALL_ROOMS_ACCESS_PROVIDER: &str = "acb.access.all_rooms";

/// 订阅的房间照常；另外列在 `access.rooms` 里的房间也算可访问。
///
/// 给「一个聊天完全豁免」用：不用为它加订阅（也就不会被它叫醒），
/// 但可以照常读、发、调平台方法。
pub const ELEVATED_ACCESS_PROVIDER: &str = "acb.access.elevated_in_rooms";

/// 按轮次来源提权：`access.origins` 里的房间触发的轮次可以访问任意房间；
/// 其它来源仍按订阅边界。用来把「私聊随便发、群聊只发订阅」这类规则写进配置。
pub const PRIVILEGED_ORIGINS_ACCESS_PROVIDER: &str = "acb.access.privileged_origins";

/// 内置 provider 的保留 namespace；树外 manager 不能占用。
pub const BUILTIN_NAMESPACE: &str = "acb";

/// 一个 provider id：`namespace.name`。第一个点前是 namespace，后面是 name。
///
/// name 自身可以再含点（`acb.wake.default`），但 namespace 不允许含点，否则
/// “按 namespace 找 manager”会变成猜前缀。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProviderId {
    namespace: String,
    name: String,
}

impl ProviderId {
    /// 解析并校验。空 namespace / 空 name、非法字符、没有点都拒绝。
    pub fn parse(text: &str) -> Result<Self, PluginError> {
        let text = text.trim();
        let Some((namespace, name)) = text.split_once('.') else {
            return Err(PluginError::InvalidProviderId(text.to_owned()));
        };
        if namespace.is_empty() || name.is_empty() {
            return Err(PluginError::InvalidProviderId(text.to_owned()));
        }
        let namespace_ok = namespace
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
        let name_ok = name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.');
        if !namespace_ok || !name_ok {
            return Err(PluginError::InvalidProviderId(text.to_owned()));
        }
        Ok(Self {
            namespace: namespace.to_owned(),
            name: name.to_owned(),
        })
    }

    /// 内置 wake provider。
    pub fn builtin_wake() -> Self {
        Self::parse(DEFAULT_WAKE_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置 access provider（默认按订阅）。
    pub fn builtin_access() -> Self {
        Self::parse(DEFAULT_ACCESS_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置 access provider：所有房间。
    pub fn builtin_all_rooms_access() -> Self {
        Self::parse(ALL_ROOMS_ACCESS_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置 access provider：指定房间提权。
    pub fn builtin_elevated_access() -> Self {
        Self::parse(ELEVATED_ACCESS_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置 access provider：指定来源房间提权。
    pub fn builtin_privileged_origins_access() -> Self {
        Self::parse(PRIVILEGED_ORIGINS_ACCESS_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置投递 composer。
    pub fn builtin_delivery() -> Self {
        Self::parse(DEFAULT_DELIVERY_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置失败通知 provider。
    pub fn builtin_turn_error() -> Self {
        Self::parse(DEFAULT_TURN_ERROR_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置「不启用时钟」provider。
    pub fn builtin_clock_off() -> Self {
        Self::parse(DEFAULT_CLOCK_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置「固定间隔 / 房间空闲」时钟 provider。
    pub fn builtin_clock_interval_idle() -> Self {
        Self::parse(CLOCK_INTERVAL_IDLE_PROVIDER).expect("built-in provider id must be valid")
    }

    /// 内置入站策略：全部接收。
    pub fn builtin_receive_all() -> Self {
        Self::parse(DEFAULT_RECEIVE_PROVIDER).expect("built-in provider id must be valid")
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.namespace, self.name)
    }
}

impl FromStr for ProviderId {
    type Err = PluginError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// 一个扩展点。Phase 1 只用到 `Wake`；其余是后续 phase 的合同名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExtensionPoint {
    Wake,
    Access,
    /// 工具调用的只读否决点；只能拒绝，不能放行 core 的判定。
    ToolAccess,
    Receive,
    /// 入库前的消息路由（命令、链接、任务）。
    Route,
    Send,
    Clock,
    Delivery,
    TurnError,
    /// 只读观察每条入库记录（统计、审计、索引）。
    Observer,
    /// 插件提供的工具。
    Tool,
    /// 插件提供的后台任务 handler。
    Task,
    /// 插件贡献的上下文片段（声明式、受预算约束）。
    Prompt,
    PlatformAdapter,
    Manager,
    Loader,
}

impl ExtensionPoint {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wake => "wake",
            Self::Access => "access",
            Self::ToolAccess => "tool_access",
            Self::Receive => "receive",
            Self::Route => "route",
            Self::Send => "send",
            Self::Clock => "clock",
            Self::Delivery => "delivery",
            Self::TurnError => "turn_error",
            Self::Observer => "observer",
            Self::Tool => "tool",
            Self::Task => "task",
            Self::Prompt => "prompt",
            Self::PlatformAdapter => "platform_adapter",
            Self::Manager => "manager",
            Self::Loader => "loader",
        }
    }
}

/// provider 被授予的能力。插件不能自己声明“我可以做更多”。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    WakeDecision,
    AccessDecision,
    Log,
    ReadTranscript,
    EmitSend,
    PlatformCatalog,
    DeliverPlan,
    SendHook,
    TurnErrorDecision,
    ClockDecision,
    ReceiveDecision,
    RouteDecision,
    /// 允许在路由动作里直接请求唤醒（绕过 wake 策略）。只给高信任载体。
    WakeRequest,
    /// 只读观察入库记录。
    ObservedRecord,
    /// 提供插件工具。
    ToolCall,
    /// 提供后台任务 handler。
    TaskHandle,
    /// 只读否决工具调用。不能覆盖 core 的既有判定。
    ToolAccessDecision,
    /// 贡献上下文片段（受 core 预算约束）。
    PromptContribute,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WakeDecision => "wake_decision",
            Self::AccessDecision => "access_decision",
            Self::Log => "log",
            Self::ReadTranscript => "read_transcript",
            Self::EmitSend => "emit_send",
            Self::PlatformCatalog => "platform_catalog",
            Self::DeliverPlan => "deliver_plan",
            Self::SendHook => "send_hook",
            Self::TurnErrorDecision => "turn_error_decision",
            Self::ClockDecision => "clock_decision",
            Self::ReceiveDecision => "receive_decision",
            Self::RouteDecision => "route_decision",
            Self::WakeRequest => "wake_request",
            Self::ObservedRecord => "observed_record",
            Self::ToolCall => "tool_call",
            Self::TaskHandle => "task_handle",
            Self::ToolAccessDecision => "tool_access_decision",
            Self::PromptContribute => "prompt_contribute",
        }
    }
}

/// provider 的描述。Root 用它做全局 id/版本/能力审计，也给 agent 看用途。
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub extension_point: ExtensionPoint,
    pub api_version: u32,
    pub capabilities: Vec<Capability>,
    /// 平台适配器的配置 `kind`（如 `onebot11` / `process`）；其他扩展点为 None。
    pub kind: Option<String>,
    /// 插件作者声明的说明。
    pub metadata: PluginMetadata,
    /// 插件来源，由 Root 标注。
    pub source: PluginSource,
}

impl ProviderDescriptor {
    pub fn new(
        id: ProviderId,
        extension_point: ExtensionPoint,
        api_version: u32,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            id,
            extension_point,
            api_version,
            capabilities,
            kind: None,
            metadata: PluginMetadata::EMPTY,
            source: PluginSource::Builtin,
        }
    }

    /// 平台适配器声明自己对应的连接 `kind`。
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = Some(kind.into());
        self
    }

    pub fn with_metadata(mut self, metadata: PluginMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_source(mut self, source: PluginSource) -> Self {
        self.source = source;
        self
    }
}

/// manager 子树最多几层（Root 算第 0 层）。超过就拒绝——防止一棵树无限往下长。
pub const MAX_MANAGER_DEPTH: usize = 4;

/// 一棵 manager 子树自己的 manifest。Root 用它做 namespace、版本和能力审计。
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerManifest {
    pub namespace: String,
    pub api_version: u32,
    /// 子树里所有 provider 需要的能力并集。Root 要求每个 descriptor 所需能力都在这里。
    pub capabilities: Vec<Capability>,
    /// manager 自己的说明；descriptor 缺字段时由 Root 用它补全。
    pub metadata: PluginMetadata,
}

impl ManagerManifest {
    pub fn new(
        namespace: impl Into<String>,
        api_version: u32,
        capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            namespace: namespace.into(),
            api_version,
            capabilities,
            metadata: PluginMetadata::EMPTY,
        }
    }

    pub fn with_metadata(mut self, metadata: PluginMetadata) -> Self {
        self.metadata = metadata;
        self
    }
}

/// 一种载体的 manifest。Root 用它校验版本和声明的扩展点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderManifest {
    pub kind: LoaderKind,
    pub api_version: u32,
    pub extensions: Vec<ExtensionPoint>,
}

impl LoaderManifest {
    pub fn new(kind: LoaderKind, api_version: u32, extensions: Vec<ExtensionPoint>) -> Self {
        Self {
            kind,
            api_version,
            extensions,
        }
    }
}

/// 热重载范围。只换指定 namespace 的子树，或者全部。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadScope {
    All,
    Namespace(String),
}

/// manager 自报的健康状态。Root 只汇总，不会因为 unhealthy 就停掉整个桥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    Healthy,
    Degraded(String),
    Unhealthy(String),
}

/// 一条 health 结果：哪个 namespace、什么状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerHealth {
    pub namespace: String,
    pub health: Health,
}

/// 插件系统错误。工具调用不在这里——那是 `acb_proto::ToolError`。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginError {
    #[error("{}", acb_i18n::t!("kernel.plugin.invalid_provider_id", id = .0))]
    InvalidProviderId(String),
    #[error("{}", acb_i18n::t!("kernel.plugin.unknown_provider", id = .0))]
    UnknownProvider(String),
    #[error("{}", acb_i18n::t!("kernel.plugin.duplicate_provider", id = .0))]
    DuplicateProvider(String),
    #[error("{}", acb_i18n::t!("kernel.plugin.unsupported_extension_point", id = id, point = format!("{:?}", point)))]
    UnsupportedExtensionPoint { id: String, point: ExtensionPoint },
    #[error("{}", acb_i18n::t!("kernel.plugin.wrong_provider_kind", id = id, found = format!("{:?}", found), expected = format!("{:?}", expected)))]
    WrongProviderKind {
        id: String,
        expected: ExtensionPoint,
        found: ExtensionPoint,
    },
    #[error("{}", acb_i18n::t!("kernel.plugin.api_version", id = id, found = found, required = required))]
    ApiVersion {
        id: String,
        found: u32,
        required: u32,
    },
    #[error("{}", acb_i18n::t!("kernel.plugin.missing_capability", id = id, capability = format!("{:?}", capability)))]
    MissingCapability { id: String, capability: Capability },
    #[error("{}", acb_i18n::t!("kernel.plugin.duplicate_namespace", namespace = .0))]
    DuplicateNamespace(String),
    #[error("{}", acb_i18n::t!("kernel.plugin.reserved_namespace", namespace = .0))]
    ReservedNamespace(String),
    #[error("{}", acb_i18n::t!("kernel.plugin.manager_error", namespace = namespace, detail = detail))]
    Manager { namespace: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.plugin.provider_error", id = id, detail = detail))]
    Provider { id: String, detail: String },
    #[error("{}", acb_i18n::t!("kernel.plugin.manager_depth", namespace = namespace, max = max))]
    ManagerDepth { namespace: String, max: usize },
    #[error("{}", acb_i18n::t!("kernel.plugin.manager_cycle", chain = .0))]
    ManagerCycle(String),
    #[error("{}", acb_i18n::t!("kernel.plugin.tool_name_taken", name = name, id = id))]
    ToolNameConflict { name: String, id: String },
    #[error("{}", acb_i18n::t!("kernel.plugin.load_failed", detail = .0))]
    Loader(String),
}

/// 传给 watcher / manager 的上下文。当前只带唤醒判定需要的东西。
///
/// `tags` 是适配器给的通用唤醒提示（未来由平台适配器填）；内置策略不读它，
/// 平台独有的策略插件可以读 `platform`、`event`、`tags`。
#[derive(Debug, Clone, Copy)]
pub struct WakeContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub connection: &'a str,
    pub platform: &'a str,
    pub sender_id: &'a str,
    pub text: &'a str,
    pub at_self: bool,
    pub reply_to_self: bool,
    /// 有没有正文。只有 @ / 引用 / 空白文本时为 false；事件恒为 false。
    pub has_body: bool,
    pub event: Option<&'a str>,
    pub tags: &'a [&'a str],
    pub unread: u32,
    /// 上一次唤醒（开轮）的 Unix 毫秒；没唤醒过就是本桥第一次见到该订阅的时间。
    pub last_wake_at: i64,
    pub now: i64,
}

/// 唤醒策略 provider。内置实现是 `DefaultWakeProvider`；它只做一层转发，
/// 把 `WakeContext` 收成原有的 `WakeInput`，保证行为不变。
pub trait WakeProvider: Send + Sync {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision;

    /// 带原因的判定。默认调用 [`decide`](Self::decide)，reason 为空；
    /// 有能力的 provider 覆盖它，让 agent 知道「这次为什么醒」。
    fn decide_with_reason(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeVerdict {
        WakeVerdict {
            decision: self.decide(options, context),
            reason: None,
            delay_ms: 0,
        }
    }
}

/// 记录观察者。append 之后同步调用，只读；不能改记录、不能开轮、不能写水位。
pub trait RecordObserver: Send + Sync {
    fn observe(&self, record: &Record);
}

/// 插件提供的一个工具。schema 是 JSON Schema；发现时先给 name + summary，
/// 只有明确按 name 查询时才返回完整 schema。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    /// 工具名。不能和 core 内置工具重名。
    pub name: String,
    /// 一句话说明，给索引列表用。
    pub summary: String,
    /// JSON Schema（object），给 `plugin_tools({name})` 详情用。
    pub schema: Value,
}

/// 插件工具调用时的上下文。只给会话/房间/来源/是否在轮内，不交 Store、票和水位。
#[derive(Debug, Clone, Copy)]
pub struct ToolCallContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub origin_sender: Option<&'a str>,
    pub live: bool,
}

/// 工具提供者。一个 provider 可以带多个工具。
pub trait ToolProvider: Send + Sync {
    fn tools(&self) -> Vec<ToolSpec>;

    /// 调用其中一个工具。`tool` 必须是 `tools()` 里声明过的名字。
    fn call(
        &self,
        context: &ToolCallContext<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError>;
}

/// 工具调用否决点的上下文。只读；不含 Store、票和水位。
#[derive(Debug, Clone, Copy)]
pub struct ToolAccessContext<'a> {
    pub tool: &'a str,
    pub args: &'a Value,
    pub session: &'a str,
    pub room: &'a RoomId,
    pub platform: &'a str,
    pub origin_sender: Option<&'a str>,
    pub is_admin: bool,
    /// `true` = 轮次在跑；`false` = 轮次结束后的宽限窗口。
    pub live: bool,
}

/// 插件只能否决，不能放行。`Allow` 只是「我不拦」，core 的判定仍然优先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolAccessVerdict {
    Allow,
    Deny { reason: String },
}

/// 工具访问判定 provider。
///
/// 契约：同步、只读、快；不要做网络/磁盘 I/O，不要依赖调用次数（同一次调用可能
/// 被两层入口各问一次）。返回 `Err` 时 core 按失败关闭处理（拒绝本次调用）。
pub trait ToolAccessProvider: Send + Sync {
    fn check(&self, context: &ToolAccessContext<'_>) -> Result<ToolAccessVerdict, String>;
}

/// 上下文注入的输入。刻意只给会话/房间/平台/开轮原因；拿不到记录、Store、票或水位。
#[derive(Debug, Clone)]
pub struct PromptContext {
    pub session: String,
    pub room: RoomId,
    pub platform: String,
    pub reason: TurnReason,
    pub wake_reason: Option<String>,
    /// 本轮开始时间（毫秒）。
    pub now: i64,
}

/// 插件要贡献的一段上下文。来源由 core 填，插件不能伪造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptDraft {
    pub text: String,
    pub priority: i32,
}

/// 上下文注入 provider。
///
/// 契约：同步、只读、快；不要做网络/磁盘 I/O。core 在独立线程里调用并设超时，
/// 超时/报错只跳过该 provider 的片段，不影响本轮投递。
pub trait PromptProvider: Send + Sync {
    fn prompts(&self, context: &PromptContext) -> Result<Vec<PromptDraft>, String>;
}

/// 内置唤醒策略：等价于原来的 `WakePolicy::decide`。
#[derive(Debug, Default)]
pub struct DefaultWakeProvider;

impl WakeProvider for DefaultWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeVerdict {
        let input = WakeInput {
            sender_id: context.sender_id,
            text: context.text,
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            has_body: context.has_body,
            event: context.event,
            platform: context.platform,
            tags: context.tags,
            unread: context.unread,
        };
        options.decide_with_reason(&input)
    }
}

/// 一次访问请求要做什么。
///
/// 粗粒度就够：策略只决定「能不能碰这个房间/这条连接」，具体动作（读、写、
/// 破坏性）仍由 core 和连接配置各自把关。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessAction {
    /// 读房间：历史、事件、笔记、只读平台方法。
    Read,
    /// 往房间写：发消息、写类平台方法。
    Send,
    /// 使用一条连接（账号级平台方法、平台目录）。
    Connection,
}

impl AccessAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Send => "send",
            Self::Connection => "connection",
        }
    }
}

/// 访问判定的输入。`room` 为 `None` 表示连接级请求。
#[derive(Debug, Clone, Copy)]
pub struct AccessRequest<'a> {
    pub session: &'a str,
    pub connection: &'a str,
    /// 这次要访问的目标房间；连接级请求为 `None`。
    pub room: Option<&'a RoomId>,
    /// 当前轮次是从哪个房间触发的。工具调用时一定有；连接级请求也保留。
    ///
    /// 和目标 `room` 分开：`send_message_to` 的目标可以是别的房间，来源仍是本轮房间。
    pub origin_room: Option<&'a RoomId>,
    pub action: AccessAction,
    /// 这个会话自己订阅的房间。provider 只读，不写。
    pub subscriptions: &'a [RoomId],
}

/// 访问判定的结果。provider 只回答这个；拒绝时的错误文案由 core 组织。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessDecision {
    Allow,
    Deny,
}

/// 访问策略 provider。
pub trait AccessProvider: Send + Sync {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision;
}

/// 内置 access provider：只看会话订阅（原来写死的行为）。
#[derive(Debug, Default)]
pub struct SubscribedAccessProvider;

impl AccessProvider for SubscribedAccessProvider {
    fn check(&self, _options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        match request.action {
            AccessAction::Connection => {
                if request
                    .subscriptions
                    .iter()
                    .any(|room| room.connection == request.connection)
                {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
            AccessAction::Read | AccessAction::Send => {
                if request
                    .room
                    .is_some_and(|room| request.subscriptions.iter().any(|sub| sub == room))
                {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
        }
    }
}

/// 整个会话豁免：任何房间、任何连接都允许。
#[derive(Debug, Default)]
pub struct AllRoomsAccessProvider;

impl AccessProvider for AllRoomsAccessProvider {
    fn check(&self, _options: &AccessPolicy, _request: &AccessRequest<'_>) -> AccessDecision {
        AccessDecision::Allow
    }
}

/// 订阅的房间照常；`access.rooms` 里列出的房间同样允许。
///
/// 用「房间列表」而不是「全部」，是因为豁免要精确到那一个聊天：
/// 不用为它加订阅（也就不会被它叫醒），但仍然能读、能发、能调平台方法。
#[derive(Debug, Default)]
pub struct ElevatedInRoomsAccessProvider;

impl AccessProvider for ElevatedInRoomsAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        match request.action {
            AccessAction::Connection => {
                let subscribed = request
                    .subscriptions
                    .iter()
                    .any(|room| room.connection == request.connection);
                let elevated = options
                    .rooms
                    .iter()
                    .any(|room| room.connection == request.connection);
                if subscribed || elevated {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
            AccessAction::Read | AccessAction::Send => {
                let by_room = request
                    .room
                    .is_some_and(|room| request.subscriptions.iter().any(|sub| sub == room));
                let elevated = request
                    .room
                    .is_some_and(|room| options.rooms.iter().any(|listed| listed == room));
                if by_room || elevated {
                    AccessDecision::Allow
                } else {
                    AccessDecision::Deny
                }
            }
        }
    }
}

/// 按轮次来源提权：`access.origins` 里的房间触发的轮次可以访问任意房间；
/// 其它来源回到内置「只认订阅」。
#[derive(Debug, Default)]
pub struct PrivilegedOriginsAccessProvider;

impl AccessProvider for PrivilegedOriginsAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        let privileged = request
            .origin_room
            .is_some_and(|origin| options.origins.iter().any(|listed| listed == origin));
        if privileged {
            return AccessDecision::Allow;
        }
        SubscribedAccessProvider.check(options, request)
    }
}

/// 一次开轮的候选记录。`records` 按编号从旧到新。
#[derive(Debug, Clone, Copy)]
pub struct DeliveryRequest<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub records: &'a [Record],
    /// 条数上限；字节预算由 composer 自己管（默认实现有 512 KiB 上限）。
    pub limit: usize,
}

/// composer 决定本轮投哪些编号，以及中间跳过哪一段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryPlan {
    /// 要投的编号，顺序就是投递顺序。未知编号会被 core 忽略。
    pub keep: Vec<u64>,
    /// 被跳过的中段。`None` = 没跳。
    pub omitted: Option<Omitted>,
}

/// 投递批次选择 provider。它只决定「投什么」，不碰水位、票和 ack——
/// 那些仍是 Root 的职责（ADR-0026 硬约束）。
pub trait DeliveryComposer: Send + Sync {
    fn compose(&self, options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan;
}

/// 内置 composer：最旧一小段 + 最新一批，中间跳过。
#[derive(Debug, Default)]
pub struct DefaultDeliveryComposer;

impl DeliveryComposer for DefaultDeliveryComposer {
    fn compose(&self, _options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan {
        default_delivery_plan(request)
    }
}

/// 默认选批算法。composer 出错时 core 也用它兜底，保证一轮不会空手开。
pub fn default_delivery_plan(request: &DeliveryRequest<'_>) -> DeliveryPlan {
    let records = request.records;
    let len = records.len();
    if len == 0 {
        return DeliveryPlan {
            keep: Vec::new(),
            omitted: None,
        };
    }
    let limit = request.limit.max(1);

    // 条数切法：至少给最新留 1 条；`limit` 大时最旧端最多 5 条。
    let head_target = DELIVERY_OLDEST.min(limit.saturating_sub(1)).min(len);
    let tail_target = (limit - head_target).min(len - head_target);

    let sizes: Vec<usize> = records
        .iter()
        .map(|record| {
            serde_json::to_vec(record)
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        })
        .collect();

    let mut budget = MAX_DELIVERY_BYTES;
    // 最旧端：从旧往新装；第一条就算超预算也要留着（等同于「至少保一条」）。
    let mut head_keep = 0usize;
    while head_keep < head_target {
        let size = sizes[head_keep];
        if head_keep > 0 && size > budget {
            break;
        }
        budget = budget.saturating_sub(size);
        head_keep += 1;
    }

    // 最新端：从新往旧装，花剩下的预算。
    let mut tail_keep = 0usize;
    while tail_keep < tail_target {
        let index = len - 1 - tail_keep;
        if sizes[index] > budget {
            break;
        }
        budget -= sizes[index];
        tail_keep += 1;
    }
    // 一条都没留下：至少给最新 1 条，不能空手开轮。
    if head_keep + tail_keep == 0 {
        tail_keep = 1;
    }

    let mut keep = Vec::with_capacity(head_keep + tail_keep);
    keep.extend(records[..head_keep].iter().map(|record| record.id));
    keep.extend(records[len - tail_keep..].iter().map(|record| record.id));
    let middle = &records[head_keep..len - tail_keep];
    let omitted = middle
        .first()
        .zip(middle.last())
        .map(|(first, last)| Omitted {
            from: first.id,
            to: last.id,
            count: middle.len(),
        });
    DeliveryPlan { keep, omitted }
}

/// 这次发送从哪儿来。hook 可以按来源区分行为。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOrigin {
    /// `send_message` / `send_message_to` 的正文。
    Message,
    /// 一轮失败后的通知。
    TurnFailure,
    /// 路由插件（命令/任务）替桥发的消息。
    Route,
}

impl SendOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::TurnFailure => "turn_failure",
            Self::Route => "route",
        }
    }
}

/// 发送钩子的上下文。hook 只能看到这些，拿不到 Store / ticket / 水位。
#[derive(Debug, Clone, Copy)]
pub struct SendContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub origin: SendOrigin,
}

/// 发送前/后钩子。`before_send` 可以改写正文；`after_send` 只观察回执。
///
/// `before_send` 出错时整次发送失败（不静默跳过 hook）；`after_send` 不影响已经发出去的消息。
pub trait SendHook: Send + Sync {
    fn before_send(
        &self,
        _context: &SendContext<'_>,
        _elements: &mut Vec<Element>,
    ) -> Result<(), PluginError> {
        Ok(())
    }

    fn after_send(&self, _context: &SendContext<'_>, _sent: &Sent) {}
}

/// 一次失败通知的输入。动作列表原样给 provider，它只负责选出要执行哪些。
#[derive(Debug, Clone, Copy)]
pub struct TurnErrorRequest<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    pub error: &'a str,
    /// 会话自己配的动作（可能空）。
    pub actions: &'a [TurnErrorAction],
    /// 全局默认动作。
    pub default_actions: &'a [TurnErrorAction],
}

/// 失败通知 provider。默认实现保持原来的选择：会话动作非空就用它，否则用全局默认。
pub trait TurnErrorProvider: Send + Sync {
    fn decide(
        &self,
        options: &TurnErrorPolicy,
        request: &TurnErrorRequest<'_>,
    ) -> Vec<TurnErrorAction>;
}

#[derive(Debug, Default)]
pub struct DefaultTurnErrorProvider;

impl TurnErrorProvider for DefaultTurnErrorProvider {
    fn decide(
        &self,
        _options: &TurnErrorPolicy,
        request: &TurnErrorRequest<'_>,
    ) -> Vec<TurnErrorAction> {
        if request.actions.is_empty() {
            request.default_actions.to_vec()
        } else {
            request.actions.to_vec()
        }
    }
}

/// 时钟判定的输入。`last_clock_at` 由 core 维护：这个（会话, 房间）上一次开时钟轮的时间。
#[derive(Debug, Clone, Copy)]
pub struct ClockContext<'a> {
    pub session: &'a str,
    pub room: &'a RoomId,
    /// 当前 Unix 毫秒。
    pub now: i64,
    /// 上一次开时钟轮的时间；第一次 tick 时初始化成当前时间。
    pub last_clock_at: i64,
    /// 上一次唤醒（任意原因开轮）的 Unix 毫秒；没唤醒过时同第一次 tick 的时间。
    pub last_wake_at: i64,
    /// 房间里最后一条记录的时间；房间没记录就是 `None`。
    pub last_record_at: Option<i64>,
}

/// 时钟 provider：只回答「现在要不要开例行检查」，不碰消息和票。
pub trait ClockProvider: Send + Sync {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool;
}

/// 默认时钟：永不触发。
#[derive(Debug, Default)]
pub struct ClockOffProvider;

impl ClockProvider for ClockOffProvider {
    fn is_due(&self, _options: &ClockPolicy, _context: &ClockContext<'_>) -> bool {
        false
    }
}

/// 固定间隔或房间空闲超时。两个阈值都可选，任一命中就开轮。
#[derive(Debug, Default)]
pub struct IntervalIdleClockProvider;

impl ClockProvider for IntervalIdleClockProvider {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool {
        if let Some(interval_secs) = options.interval_secs {
            if interval_secs > 0 {
                let interval = (interval_secs as i64).saturating_mul(1000);
                if context.now.saturating_sub(context.last_clock_at) >= interval {
                    return true;
                }
            }
        }
        if let Some(idle_secs) = options.idle_secs {
            if idle_secs > 0 {
                if let Some(last_record_at) = context.last_record_at {
                    let idle = (idle_secs as i64).saturating_mul(1000);
                    if context.now.saturating_sub(last_record_at) >= idle {
                        return true;
                    }
                }
            }
        }
        false
    }
}

/// 一条入站记录的判定输入。provider 只能决定收不收，不能改内容或水位。
#[derive(Debug, Clone, Copy)]
pub struct ReceiveRequest<'a> {
    pub connection: &'a str,
    pub platform: &'a str,
    pub room: &'a RoomId,
    pub sender: &'a Sender,
    pub kind: &'a RecordKind,
    pub elements: &'a [Element],
    pub text: &'a str,
    pub at_self: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveDecision {
    Record,
    Drop,
}

/// 入站策略 provider。默认全部接收；自定义策略可以丢机器人、丢噪声等。
pub trait ReceiveProvider: Send + Sync {
    fn decide(&self, options: &ReceivePolicy, request: &ReceiveRequest<'_>) -> ReceiveDecision;
}

#[derive(Debug, Default)]
pub struct AcceptAllReceiveProvider;

impl ReceiveProvider for AcceptAllReceiveProvider {
    fn decide(&self, _options: &ReceivePolicy, _request: &ReceiveRequest<'_>) -> ReceiveDecision {
        ReceiveDecision::Record
    }
}

/// 消息路由的判定输入。比 receive 多带身份和“是不是冲我来的”，
/// 因为路由要区分管理员命令、裸 @、普通闲聊和链接。
#[derive(Debug, Clone)]
pub struct RouteContext<'a> {
    pub connection: &'a str,
    pub platform: &'a str,
    pub room: &'a RoomId,
    pub sender: &'a Sender,
    /// 发送者是不是这条连接点名的系统管理员。
    pub is_admin: bool,
    pub kind: &'a RecordKind,
    pub elements: &'a [Element],
    /// 只拼 Text 段：命令匹配不该被 @ / 引用的占位符干扰。
    pub text: &'a str,
    pub at_self: bool,
    pub reply_to_self: bool,
    pub has_body: bool,
    pub event: Option<&'a str>,
    pub tags: &'a [&'a str],
    /// 订阅这个房间的会话（按配置顺序）。路由动作默认用第一个会话的身份发消息。
    pub sessions: &'a [String],
}

/// 路由决定。
#[derive(Debug, Clone, PartialEq)]
pub enum RouteDecision {
    /// 继续走正常流程：入库、唤醒。
    Pass,
    /// 吃掉，不写流水、不叫醒。
    Drop { reason: Option<String> },
    /// 交给代码处理。
    Handle {
        /// 这条命令本身要不要写流水（默认不写）。
        store: bool,
        /// 处理完要不要让 AI 也看到这条（默认不看）。
        wake: bool,
        actions: Vec<RouteAction>,
    },
}

impl RouteDecision {
    /// 只做一件事的简写：`Handle { store:false, wake:false, actions:[action] }`。
    pub fn handle(action: RouteAction) -> Self {
        Self::Handle {
            store: false,
            wake: false,
            actions: vec![action],
        }
    }
}

/// 路由处理动作。core 负责在锁外执行真正有 I/O 的动作。
#[derive(Debug, Clone, PartialEq)]
pub enum RouteAction {
    /// 提前结束轮次。由 core 清 pending、标 active、发 interrupt。
    Interrupt { scope: InterruptScope },
    /// 以桥的身份发一条消息。发到 `room`，不写就发到触发消息所在房间。
    Send {
        room: Option<RoomId>,
        elements: Vec<Element>,
    },
    /// 跑一个后台任务（下载、转码等慢活），完成后按任务结果发送。
    Task { handler: String, params: Value },
    /// 高信任模块直接请求唤醒，绕过现有 wake 策略。
    ///
    /// core 会检查来源 provider 的信任层级（builtin / trusted / dynamic）与
    /// `WakeRequest` 能力、会话订阅关系和频率上限，每次请求都写审计。
    Wake {
        /// 目标会话；省略时唤醒目标房间里所有订阅者。
        session: Option<String>,
        /// 目标房间；省略时用触发消息所在房间。
        room: Option<RoomId>,
        /// 唤醒原因，带给 agent 并写进审计。
        reason: Option<String>,
    },
}

/// 打断范围。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterruptScope {
    /// 触发消息所在房间（默认）。
    Room,
    /// 指定会话（必须是订阅了触发房间的会话）。
    Sessions(Vec<String>),
    /// 触发消息所属连接上的所有会话。
    Connection,
}

/// 消息路由 provider。按连接配置、按顺序调用；返回 `Pass` 才继续下一个。
pub trait RouteProvider: Send + Sync {
    fn decide(&self, options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision;
}

/// 没有配置路由时的兜底：全部放行。
#[derive(Debug, Default)]
pub struct PassThroughRouteProvider;

impl RouteProvider for PassThroughRouteProvider {
    fn decide(&self, _options: &RoutePolicy, _context: &RouteContext<'_>) -> RouteDecision {
        RouteDecision::Pass
    }
}

/// manager 实例化 provider 时能拿到的 options。
#[derive(Debug, Clone, Copy)]
pub enum ProviderOptions<'a> {
    Wake(&'a WakePolicy),
    Access(&'a AccessPolicy),
    Delivery(&'a DeliveryPolicy),
    TurnError(&'a TurnErrorPolicy),
    Clock(&'a ClockPolicy),
    Receive(&'a ReceivePolicy),
    Route(&'a RoutePolicy),
    /// 观察者不需要策略配置。
    Observer,
    /// 工具提供者不需要策略配置。
    Tool,
    /// 任务 handler 不需要策略配置。
    Task,
    /// 工具访问判定不需要策略配置。
    ToolAccess,
    /// 上下文注入 provider 不需要策略配置。
    Prompt,
    /// 平台适配器：按这条连接的配置实例化。
    PlatformAdapter(&'a Connection),
    /// 不需要 options 的扩展点（发送钩子）。
    None,
}

/// manager 返回的 provider 实例。
pub enum ProviderInstance {
    Wake(Arc<dyn WakeProvider>),
    Access(Arc<dyn AccessProvider>),
    Delivery(Arc<dyn DeliveryComposer>),
    Send(Arc<dyn SendHook>),
    TurnError(Arc<dyn TurnErrorProvider>),
    Clock(Arc<dyn ClockProvider>),
    Receive(Arc<dyn ReceiveProvider>),
    Route(Arc<dyn RouteProvider>),
    Observer(Arc<dyn RecordObserver>),
    Tool(Arc<dyn ToolProvider>),
    Task(Arc<dyn crate::task::TaskHandler>),
    ToolAccess(Arc<dyn ToolAccessProvider>),
    Prompt(Arc<dyn PromptProvider>),
    PlatformAdapter(Arc<dyn AdapterFactory>),
}

impl ProviderInstance {
    /// 这个实例对应哪个扩展点。Root 用它拒绝“manager 返回错类型”。
    pub fn extension_point(&self) -> ExtensionPoint {
        match self {
            Self::Wake(_) => ExtensionPoint::Wake,
            Self::Access(_) => ExtensionPoint::Access,
            Self::Delivery(_) => ExtensionPoint::Delivery,
            Self::Send(_) => ExtensionPoint::Send,
            Self::TurnError(_) => ExtensionPoint::TurnError,
            Self::Clock(_) => ExtensionPoint::Clock,
            Self::Receive(_) => ExtensionPoint::Receive,
            Self::Route(_) => ExtensionPoint::Route,
            Self::Observer(_) => ExtensionPoint::Observer,
            Self::Tool(_) => ExtensionPoint::Tool,
            Self::Task(_) => ExtensionPoint::Task,
            Self::ToolAccess(_) => ExtensionPoint::ToolAccess,
            Self::Prompt(_) => ExtensionPoint::Prompt,
            Self::PlatformAdapter(_) => ExtensionPoint::PlatformAdapter,
        }
    }
}

/// 一棵子树的管理者。Root 只和这个接口说话；子树内部怎么注册由它自己决定。
///
/// 除了解析 provider，manager 还要能报 manifest、被子 manager 嵌套、在轮次边界
/// 重载、退出时收尾、报告健康状态（ADR-0026 Phase 3）。
pub trait ManagerPlugin: Send + Sync {
    /// 自己的 namespace、API 版本、能力和 options schema。
    fn manifest(&self) -> ManagerManifest;

    /// 子树里的 provider 清单。Root 用它做全局 id 唯一性和能力审计。
    fn descriptors(&self) -> Vec<ProviderDescriptor>;

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError>;

    /// 子树里的子 manager。Root 递归安装，并做深度上限与环路检测。
    fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
        Vec::new()
    }

    /// 轮次边界热重载。默认什么都不做。
    fn reload(&self, _scope: &ReloadScope) -> Result<(), PluginError> {
        Ok(())
    }

    /// 进程退出前收尾。默认什么都不做。
    fn shutdown(&self) {}

    /// 健康检查。默认健康。
    fn health(&self) -> Health {
        Health::Healthy
    }

    /// 子树自己的模块级信息（动态扩展目录用）。默认没有。
    fn details(&self) -> Value {
        Value::Null
    }

    /// 子树的可写根目录（authoring 工具做路径 jail 用）。默认没有。
    fn root(&self) -> Option<std::path::PathBuf> {
        None
    }
}

/// 一种载体的加载器。`NativeLoader` 是内置实现；动态/进程/Lua 后续加。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    Native,
    Dynamic,
    Process,
    Script,
}

impl LoaderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Dynamic => "dynamic",
            Self::Process => "process",
            Self::Script => "script",
        }
    }
}

/// 可回滚插件的产物引用。LKG 用它记录“这个版本的文件在哪”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginArtifact {
    pub path: String,
    pub version: Option<String>,
}

pub trait PluginLoader: Send + Sync {
    /// 载体类型、API 版本、它能带来的扩展点。
    fn manifest(&self) -> LoaderManifest;

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError>;

    /// 卸载这个 loader 安装的 provider。默认不支持——动态 loader 再覆盖。
    fn unload(&self, _manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        Err(PluginError::Loader(acb_i18n::t!(
            "kernel.plugin.loader_no_unload",
            id = id
        )))
    }

    /// 这个 provider 是否能由本 loader 安全热卸载。默认不能。
    fn can_unload(&self, _id: &ProviderId) -> bool {
        false
    }

    /// 已卸载但仍留在目录里的 provider 快照。默认没有。
    fn inactive(&self) -> Vec<PluginInfo> {
        Vec::new()
    }

    /// 重新加载一个已卸载的 provider。默认不支持——动态 loader 再覆盖。
    fn reload(&self, _manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        Err(PluginError::Loader(acb_i18n::t!(
            "kernel.plugin.loader_no_reload",
            id = id
        )))
    }

    /// 这个 provider 的产物引用（路径 + 版本）。默认没有；只有可回滚载体覆盖。
    fn artifact(&self, _id: &ProviderId) -> Option<PluginArtifact> {
        None
    }

    /// 改这个 provider 下次重新加载时使用的产物路径。默认不支持。
    fn set_artifact_path(&self, id: &ProviderId, _path: &str) -> Result<(), PluginError> {
        Err(PluginError::Loader(acb_i18n::t!(
            "kernel.plugin.loader_no_artifact",
            id = id
        )))
    }
}

fn native_meta(
    title: &'static str,
    summary: &'static str,
    effects: &[PluginEffect],
) -> PluginMetadata {
    PluginMetadata {
        title: Some(title.into()),
        summary: Some(summary.into()),
        description: None,
        tags: Vec::new().into(),
        effects: effects.to_vec().into(),
        docs_url: None,
        options_schema: None,
    }
}

/// 进程内 ToolProvider 的 loader。
///
/// 工具代码仍要跟宿主一起编译，但注册/卸载走现有 loader 与轮次边界状态机：
/// `queue_unload` / `queue_reload` 生效后，`plugin_tools` 索引随之出现/消失。
pub struct ToolLoader {
    entries: Vec<(ProviderDescriptor, Arc<dyn ToolProvider>)>,
    /// 已卸载但保留的目录快照，与 DynamicLoader 的 `unloaded` 语义一致。
    unloaded: Mutex<HashMap<ProviderId, PluginInfo>>,
}

impl ToolLoader {
    pub fn new(entries: Vec<(ProviderDescriptor, Arc<dyn ToolProvider>)>) -> Self {
        Self {
            entries,
            unloaded: Mutex::new(HashMap::new()),
        }
    }

    fn entry(&self, id: &ProviderId) -> Option<(&ProviderDescriptor, &Arc<dyn ToolProvider>)> {
        self.entries
            .iter()
            .find(|(descriptor, _)| &descriptor.id == id)
            .map(|(descriptor, provider)| (descriptor, provider))
    }
}

impl PluginLoader for ToolLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Native,
            PLUGIN_API_VERSION,
            vec![ExtensionPoint::Tool],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        let mut installed: Vec<ProviderId> = Vec::new();
        for (descriptor, provider) in &self.entries {
            match manager.register_tools(descriptor.clone(), provider.clone()) {
                Ok(()) => installed.push(descriptor.id.clone()),
                Err(error) => {
                    // 安装是批量的：一个重名不能让前面几个留下半套注册。
                    for id in installed {
                        let _ = manager.remove_tool(&id);
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn unload(&self, manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        let Some(mut info) = manager.info(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        manager.remove_tool(id)?;
        info.lifecycle = PluginLifecycle::Unloaded;
        self.unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), info);
        Ok(())
    }

    fn can_unload(&self, id: &ProviderId) -> bool {
        self.entry(id).is_some()
    }

    fn inactive(&self) -> Vec<PluginInfo> {
        self.unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn reload(&self, manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        if !self
            .unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(id)
        {
            return Err(PluginError::Loader(acb_i18n::t!(
                "kernel.plugin.tool_loader_not_unloaded",
                id = id
            )));
        }
        let Some((descriptor, provider)) = self.entry(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        manager.register_tools(descriptor.clone(), provider.clone())?;
        self.unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        Ok(())
    }
}

/// 内置 loader。它不读文件，只把编译进来的 provider 注册到 Root。
#[derive(Debug, Default)]
pub struct NativeLoader;

impl PluginLoader for NativeLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Native,
            PLUGIN_API_VERSION,
            vec![
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::TurnError,
                ExtensionPoint::Clock,
                ExtensionPoint::Receive,
                ExtensionPoint::Route,
            ],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        manager.register_wake(
            ProviderDescriptor::new(
                ProviderId::builtin_wake(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
            .with_metadata(native_meta(
                "默认唤醒",
                "被 @、引用、点名或关键词命中时叫醒会话",
                &[PluginEffect::MayWake, PluginEffect::MayStaySilent],
            )),
            Arc::new(DefaultWakeProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "只访问订阅房间",
                "允许读写当前会话已订阅的房间；拒绝其它房间",
                &[],
            )),
            Arc::new(SubscribedAccessProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_all_rooms_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "全房间访问",
                "允许访问任意房间，不受订阅列表限制（高风险）",
                &[],
            )),
            Arc::new(AllRoomsAccessProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_elevated_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "订阅房间内提升权限",
                "在已订阅房间内允许破坏性操作（踢人、禁言、改群名等）",
                &[],
            )),
            Arc::new(ElevatedInRoomsAccessProvider),
        )?;
        manager.register_access(
            ProviderDescriptor::new(
                ProviderId::builtin_privileged_origins_access(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(native_meta(
                "按来源房间提权",
                "从 origins 列表里的房间触发的轮次可访问任意房间；其它来源仍按订阅",
                &[],
            )),
            Arc::new(PrivilegedOriginsAccessProvider),
        )?;
        manager.register_delivery(
            ProviderDescriptor::new(
                ProviderId::builtin_delivery(),
                ExtensionPoint::Delivery,
                PLUGIN_API_VERSION,
                vec![Capability::DeliverPlan],
            )
            .with_metadata(native_meta(
                "默认投递",
                "按水位顺序投递未读；不改写正文，不额外延迟",
                &[],
            )),
            Arc::new(DefaultDeliveryComposer),
        )?;
        manager.register_turn_error(
            ProviderDescriptor::new(
                ProviderId::builtin_turn_error(),
                ExtensionPoint::TurnError,
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )
            .with_metadata(native_meta(
                "默认轮次错误策略",
                "按配置动作处理轮次失败：记录、反馈或中止",
                &[],
            )),
            Arc::new(DefaultTurnErrorProvider),
        )?;
        manager.register_clock(
            ProviderDescriptor::new(
                ProviderId::builtin_clock_off(),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
            .with_metadata(native_meta(
                "关闭定时唤醒",
                "不主动开新轮次，只由入站消息触发",
                &[PluginEffect::MayStaySilent],
            )),
            Arc::new(ClockOffProvider),
        )?;
        manager.register_clock(
            ProviderDescriptor::new(
                ProviderId::builtin_clock_interval_idle(),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
            .with_metadata(native_meta(
                "空闲定时唤醒",
                "会话空闲一段时间后主动开新轮次",
                &[PluginEffect::MayWake],
            )),
            Arc::new(IntervalIdleClockProvider),
        )?;
        manager.register_receive(
            ProviderDescriptor::new(
                ProviderId::builtin_receive_all(),
                ExtensionPoint::Receive,
                PLUGIN_API_VERSION,
                vec![Capability::ReceiveDecision],
            )
            .with_metadata(native_meta("接收全部入站", "所有入站记录都入库", &[])),
            Arc::new(AcceptAllReceiveProvider),
        )
    }
}

/// RootManager：固定门面。负责所有扩展点的注册/解析/校验。
#[derive(Default)]
pub struct PluginManager {
    wake: HashMap<ProviderId, Arc<dyn WakeProvider>>,
    access: HashMap<ProviderId, Arc<dyn AccessProvider>>,
    delivery: HashMap<ProviderId, Arc<dyn DeliveryComposer>>,
    send_hooks: HashMap<ProviderId, Arc<dyn SendHook>>,
    turn_errors: HashMap<ProviderId, Arc<dyn TurnErrorProvider>>,
    clocks: HashMap<ProviderId, Arc<dyn ClockProvider>>,
    receives: HashMap<ProviderId, Arc<dyn ReceiveProvider>>,
    routes: HashMap<ProviderId, Arc<dyn RouteProvider>>,
    observers: HashMap<ProviderId, Arc<dyn RecordObserver>>,
    tool_providers: HashMap<ProviderId, Arc<dyn ToolProvider>>,
    tool_access: HashMap<ProviderId, Arc<dyn ToolAccessProvider>>,
    prompt_providers: HashMap<ProviderId, Arc<dyn PromptProvider>>,
    /// handler 名 → (注册者 provider id, 执行器)。名字可比 provider id 短，所以
    /// 卸载时按注册者归属清，而不是猜名字前缀。
    task_handlers: HashMap<String, (ProviderId, Arc<dyn crate::task::TaskHandler>)>,
    /// 直接注册的平台适配器工厂，键是连接配置里的 `kind`。
    adapters: HashMap<String, Arc<dyn AdapterFactory>>,
    /// manager 子树声明的适配器：`kind` → provider id。
    adapter_providers: HashMap<String, ProviderId>,
    managers: HashMap<String, Arc<dyn ManagerPlugin>>,
    /// 安装顺序（含嵌套子树）。reload/health 正序，shutdown 逆序。
    manager_order: Vec<String>,
    descriptors: HashMap<ProviderId, ProviderDescriptor>,
    /// 实例化失败（含 panic）的 provider → 最近一次错误。控制台 `lastError` 用。
    instantiate_errors: Mutex<HashMap<ProviderId, String>>,
    /// 安装过的 loader，用于按 provider 找到能卸载它的载体。
    loaders: Vec<Arc<dyn PluginLoader>>,
}

/// 安装前的预检结果：先攒齐整棵子树，再落表，避免装到一半失败留下半棵树。
struct CollectedManager {
    namespace: String,
    manager: Arc<dyn ManagerPlugin>,
    descriptors: Vec<ProviderDescriptor>,
}

/// 收集一棵 manager 子树的所有 namespace（父在前、子在后）。
///
/// `visited` 防止运行期 `children()` 返回环路；安装阶段虽然已经拒绝静态环路，
/// 但 manager 可以在运行中改变自己的 child 集合，卸载路径仍要防御。
fn collect_manager_namespaces(
    manager: &Arc<dyn ManagerPlugin>,
    namespaces: &mut Vec<String>,
    visited: &mut HashSet<String>,
) {
    let namespace = manager.manifest().namespace.trim().to_owned();
    if namespace.is_empty() || !visited.insert(namespace.clone()) {
        return;
    }
    namespaces.push(namespace);
    for child in manager.children() {
        collect_manager_namespaces(&child, namespaces, visited);
    }
}

impl PluginManager {
    /// 带内置 provider 的 Root。`Bridge::new` 用这个，保证默认行为不变。
    pub fn with_native_builtins() -> Self {
        let mut manager = Self::default();
        manager
            .install_loader(&NativeLoader)
            .expect("built-in providers must register successfully");
        manager
    }

    /// 实例化一个 manager provider；插件 panic 只影响这一个 provider。
    ///
    /// 失败写进 `instantiate_errors`（控制台 `lastError` 可见），并返回
    /// `PluginError::Provider`。成功会清掉旧错误，允许插件恢复。
    fn instantiate_safe(
        &self,
        manager: &Arc<dyn ManagerPlugin>,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        match catch_unwind(AssertUnwindSafe(|| manager.instantiate(id, options))) {
            Ok(Ok(instance)) => {
                self.instantiate_errors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(id);
                Ok(instance)
            }
            Ok(Err(error)) => {
                self.instantiate_errors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.clone(), error.to_string());
                Err(error)
            }
            Err(payload) => {
                let error = PluginError::Provider {
                    id: id.to_string(),
                    detail: acb_i18n::t!(
                        "kernel.plugin.instantiate_panicked",
                        id = id,
                        panic = panic_text(payload)
                    ),
                };
                eprintln!("{error}");
                self.instantiate_errors
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.clone(), error.to_string());
                Err(error)
            }
        }
    }

    /// 最近一次实例化错误（控制台 `lastError` 用）。
    pub fn provider_error(&self, id: &ProviderId) -> Option<String> {
        self.instantiate_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }

    /// 安装一个 loader。动态/进程/Lua loader 后续通过这个入口注册。
    ///
    /// 先校验 loader 的 manifest：API 版本要对上，声明的扩展点必须是 Root 认识的。
    pub fn install_loader(&mut self, loader: &dyn PluginLoader) -> Result<(), PluginError> {
        let manifest = loader.manifest();
        if manifest.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersion {
                id: format!("loader:{}", manifest.kind.as_str()),
                found: manifest.api_version,
                required: PLUGIN_API_VERSION,
            });
        }
        if manifest.extensions.is_empty() {
            return Err(PluginError::Loader(acb_i18n::t!(
                "kernel.plugin.loader_no_extensions",
                loader = manifest.kind.as_str()
            )));
        }
        for point in &manifest.extensions {
            if !matches!(
                point,
                ExtensionPoint::Wake
                    | ExtensionPoint::Access
                    | ExtensionPoint::Delivery
                    | ExtensionPoint::Send
                    | ExtensionPoint::TurnError
                    | ExtensionPoint::Clock
                    | ExtensionPoint::Receive
                    | ExtensionPoint::Route
                    | ExtensionPoint::Observer
                    | ExtensionPoint::Tool
                    | ExtensionPoint::Task
                    | ExtensionPoint::ToolAccess
                    | ExtensionPoint::Prompt
                    | ExtensionPoint::PlatformAdapter
            ) {
                return Err(PluginError::UnsupportedExtensionPoint {
                    id: format!("loader:{}", manifest.kind.as_str()),
                    point: *point,
                });
            }
        }
        loader.install(self)
    }

    /// 安装一个需要保留引用、后续可以按 provider 卸载的 loader（动态库 v2 用）。
    pub fn install_loader_owned(
        &mut self,
        loader: Arc<dyn PluginLoader>,
    ) -> Result<(), PluginError> {
        self.install_loader(loader.as_ref())?;
        self.loaders.push(loader);
        Ok(())
    }

    /// 按 provider id 找到能卸载它的 loader 并卸载。只能在安全点调用（无在途轮次）。
    pub fn unload_provider(&mut self, id: &ProviderId) -> Result<(), PluginError> {
        let loaders = self.loaders.clone();
        let mut last = None;
        for loader in loaders {
            match loader.unload(self, id) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!("kernel.plugin.no_loader_unload", id = id),
        }))
    }

    /// 重新加载一个已卸载的 provider。只能在安全点调用（无在途轮次）。
    pub fn reload_provider(&mut self, id: &ProviderId) -> Result<(), PluginError> {
        let loaders = self.loaders.clone();
        let mut last = None;
        for loader in loaders {
            match loader.reload(self, id) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!("kernel.plugin.no_loader_reload", id = id),
        }))
    }

    /// 取一个 provider 的产物引用（LKG 登记/回滚用）。
    pub fn artifact_of(&self, id: &ProviderId) -> Option<PluginArtifact> {
        self.loaders.iter().find_map(|loader| loader.artifact(id))
    }

    /// 改一个 provider 下次重载使用的产物路径。只能在安全点调用。
    pub fn set_artifact_path(&self, id: &ProviderId, path: &str) -> Result<(), PluginError> {
        let mut last = None;
        for loader in &self.loaders {
            match loader.set_artifact_path(id, path) {
                Ok(()) => return Ok(()),
                Err(error) => last = Some(error),
            }
        }
        Err(last.unwrap_or_else(|| PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!("kernel.plugin.no_loader_artifact", id = id),
        }))
    }

    /// 移除一棵 manager 子树及其 descriptor、路由和适配器登记。
    ///
    /// 递归收集 `children()` 的整棵子树，按“子先父后”的逆序 shutdown 并清理注册表。
    /// 调用前必须保证没有在途轮次或已解析的 provider 实例；这是安全点约束，不是本方法
    /// 能自行验证的状态。
    pub fn uninstall_manager(&mut self, namespace: &str) -> Result<(), PluginError> {
        let Some(manager) = self.managers.get(namespace).cloned() else {
            return Err(PluginError::Manager {
                namespace: namespace.to_owned(),
                detail: acb_i18n::t!("kernel.plugin.manager_not_installed", namespace = namespace),
            });
        };

        // 先完整收集子树再动手：如果收集到一半失败，不能留下半棵卸载的树。
        // visited 同时挡住动态 `children()` 返回环路的情况；安装阶段的环路校验
        // 只管静态树，运行期仍要防御。
        let mut namespaces = Vec::new();
        let mut visited = HashSet::new();
        collect_manager_namespaces(&manager, &mut namespaces, &mut visited);

        // 逆序：最深/最后的 child 先 shutdown，父 manager 最后。
        for namespace in namespaces.iter().rev() {
            if let Some(manager) = self.managers.remove(namespace) {
                manager.shutdown();
            }
            self.manager_order.retain(|name| name != namespace);
            self.descriptors.retain(|id, _| id.namespace() != namespace);
            self.wake.retain(|id, _| id.namespace() != namespace);
            self.access.retain(|id, _| id.namespace() != namespace);
            self.delivery.retain(|id, _| id.namespace() != namespace);
            self.send_hooks.retain(|id, _| id.namespace() != namespace);
            self.turn_errors.retain(|id, _| id.namespace() != namespace);
            self.clocks.retain(|id, _| id.namespace() != namespace);
            self.receives.retain(|id, _| id.namespace() != namespace);
            self.routes.retain(|id, _| id.namespace() != namespace);
            self.observers.retain(|id, _| id.namespace() != namespace);
            self.tool_providers
                .retain(|id, _| id.namespace() != namespace);
            self.tool_access.retain(|id, _| id.namespace() != namespace);
            self.prompt_providers
                .retain(|id, _| id.namespace() != namespace);
            self.task_handlers
                .retain(|_, (owner, _)| owner.namespace() != namespace);
            self.adapter_providers
                .retain(|_, id| id.namespace() != namespace);
        }
        Ok(())
    }

    /// 直接注册一个平台适配器工厂（内置适配器、native loader 用它）。
    ///
    /// 适配器也走 Root 的 manifest / 版本 / 能力审计，和 provider 一样。
    pub fn register_adapter(
        &mut self,
        factory: Arc<dyn AdapterFactory>,
    ) -> Result<(), AdapterError> {
        self.register_adapter_with_source(factory, PluginSource::Builtin)
    }

    /// 带来源注册适配器。动态库 loader 用它标注 `Dynamic`。
    pub fn register_adapter_with_source(
        &mut self,
        factory: Arc<dyn AdapterFactory>,
        source: PluginSource,
    ) -> Result<(), AdapterError> {
        let manifest = factory.manifest();
        validate_manifest(&manifest)?;
        let kind = manifest.kind.as_ref().to_owned();
        if source != PluginSource::Builtin
            && manifest
                .metadata
                .summary
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(AdapterError::MissingDocs(kind));
        }
        if self.adapters.contains_key(&kind) || self.adapter_providers.contains_key(&kind) {
            return Err(AdapterError::DuplicateKind(kind));
        }

        let id = ProviderId::parse(manifest.id.as_ref())
            .map_err(|_| AdapterError::BadId(manifest.id.as_ref().to_owned()))?;
        if self.descriptors.contains_key(&id) {
            return Err(AdapterError::DuplicateId(id.to_string()));
        }
        self.descriptors.insert(
            id.clone(),
            ProviderDescriptor::new(
                id,
                ExtensionPoint::PlatformAdapter,
                PLUGIN_API_VERSION,
                vec![Capability::PlatformCatalog],
            )
            .with_kind(kind.clone())
            .with_metadata(manifest.metadata.clone())
            .with_source(source),
        );
        self.adapters.insert(kind, factory);
        Ok(())
    }

    /// Root 目录：所有 provider / 适配器的只读说明，按扩展点和 id 排序。
    ///
    /// 描述由插件作者声明，core 不靠它做行为判断；health 来自 manager 子树。
    pub fn catalog(&self) -> Vec<PluginInfo> {
        let instantiate_errors = self
            .instantiate_errors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let mut infos: Vec<PluginInfo> = self
            .descriptors
            .values()
            .map(|descriptor| {
                let health = match instantiate_errors.get(&descriptor.id) {
                    Some(error) => Health::Unhealthy(error.clone()),
                    // 平台适配器没有 manager；健康状态来自工厂（动态适配器会报句柄失效）。
                    None if descriptor.extension_point == ExtensionPoint::PlatformAdapter => {
                        descriptor
                            .kind
                            .as_ref()
                            .and_then(|kind| self.adapters.get(kind))
                            .map(|factory| factory.health())
                            .unwrap_or(Health::Healthy)
                    }
                    None => self
                        .managers
                        .get(descriptor.id.namespace())
                        .map(|manager| manager_health(descriptor.id.namespace(), manager))
                        .unwrap_or(Health::Healthy),
                };
                let unloadable = self
                    .loaders
                    .iter()
                    .any(|loader| loader.can_unload(&descriptor.id));
                PluginInfo {
                    id: descriptor.id.clone(),
                    extension: descriptor.extension_point,
                    metadata: descriptor.metadata.clone(),
                    capabilities: descriptor.capabilities.clone(),
                    kind: descriptor.kind.clone(),
                    source: descriptor.source,
                    health,
                    lifecycle: if unloadable {
                        PluginLifecycle::Unloadable
                    } else {
                        PluginLifecycle::Resident
                    },
                }
            })
            .collect();
        // 卸载后的插件不在 descriptors 里，但仍在 loader 的 inactive 目录里；
        // 合并进来，控制台才能显示「已卸载」并触发重新加载。
        for loader in &self.loaders {
            for info in loader.inactive() {
                if !infos.iter().any(|existing| existing.id == info.id) {
                    infos.push(info);
                }
            }
        }
        infos.sort_by(|left, right| {
            (left.extension.as_str(), left.id.to_string())
                .cmp(&(right.extension.as_str(), right.id.to_string()))
        });
        infos
    }

    /// 目录的 JSON 视图（控制台 / 测试用）。
    pub fn catalog_json(&self) -> Vec<Value> {
        self.catalog().iter().map(PluginInfo::to_json).collect()
    }

    /// 按 provider id 查一条目录。找不到返回 None（配置引用错误由启动校验先报）。
    pub fn info(&self, id: &ProviderId) -> Option<PluginInfo> {
        self.catalog().into_iter().find(|info| &info.id == id)
    }

    /// 按连接配置实例化适配器。先查直接注册的工厂，再按 `kind` 委托 manager 子树。
    pub fn create_adapter(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        if let Some(factory) = self.adapters.get(&connection.kind) {
            return create_instance(factory.as_ref(), connection, events);
        }

        let Some(id) = self.adapter_providers.get(&connection.kind).cloned() else {
            return Err(AdapterError::UnknownKind(connection.kind.clone()));
        };
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!(
                    "kernel.plugin.manager_not_installed",
                    namespace = id.namespace()
                ),
            ));
        };

        match self.instantiate_safe(manager, &id, ProviderOptions::PlatformAdapter(connection)) {
            Ok(ProviderInstance::PlatformAdapter(factory)) => {
                create_instance(factory.as_ref(), connection, events)
            }
            Ok(other) => Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!(
                    "kernel.plugin.not_a_platform_adapter",
                    id = id,
                    found = format!("{:?}", other.extension_point())
                ),
            )),
            Err(error) => Err(AdapterError::Create(
                connection.id.clone(),
                error.to_string(),
            )),
        }
    }

    /// 按连接 `kind` 取已注册适配器的 manifest。动态库/外置进程适配器也由 loader
    /// 直接注册进 Root，因此和内置适配器一样能查。
    pub fn adapter_manifest(&self, kind: &str) -> Option<AdapterManifest> {
        self.adapters.get(kind).map(|factory| factory.manifest())
    }

    /// 已直接注册的适配器 manifest（给控制台/诊断用）。
    pub fn adapter_manifests(&self) -> Vec<AdapterManifest> {
        let mut manifests: Vec<AdapterManifest> = self
            .adapters
            .values()
            .map(|factory| factory.manifest())
            .collect();
        manifests.sort_by(|left, right| left.kind.cmp(&right.kind));
        manifests
    }

    /// 注册 wake provider。Root 在这里校验扩展点、API 版本、能力和重复 id。
    pub fn register_wake(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn WakeProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(&descriptor, ExtensionPoint::Wake, Capability::WakeDecision)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.wake.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册 access provider。校验规则同 wake。
    pub fn register_access(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn AccessProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Access,
            Capability::AccessDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.access.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册投递 composer。校验规则同 wake。
    pub fn register_delivery(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn DeliveryComposer>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Delivery,
            Capability::DeliverPlan,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.delivery.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册发送钩子。校验规则同 wake。
    pub fn register_send_hook(
        &mut self,
        descriptor: ProviderDescriptor,
        hook: Arc<dyn SendHook>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(&descriptor, ExtensionPoint::Send, Capability::SendHook)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.send_hooks.insert(descriptor.id, hook);
        Ok(())
    }

    /// 注册失败通知 provider。校验规则同 wake。
    pub fn register_turn_error(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn TurnErrorProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::TurnError,
            Capability::TurnErrorDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.turn_errors.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册时钟 provider。校验规则同 wake。
    pub fn register_clock(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn ClockProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Clock,
            Capability::ClockDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.clocks.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册入站策略 provider。校验规则同 wake。
    pub fn register_receive(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn ReceiveProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Receive,
            Capability::ReceiveDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.receives.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册消息路由 provider。校验规则同 wake。
    pub fn register_route(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn RouteProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Route,
            Capability::RouteDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.routes.insert(descriptor.id, provider);
        Ok(())
    }

    /// 注册插件工具提供者。工具名不能和内置工具或已注册插件工具重名。
    pub fn register_tools(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn ToolProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(&descriptor, ExtensionPoint::Tool, Capability::ToolCall)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        let mut names = HashSet::new();
        for spec in provider.tools() {
            if crate::tools::TOOL_NAMES.contains(&spec.name.as_str())
                || self
                    .declared_tool_names()
                    .iter()
                    .any(|name| name == &spec.name)
            {
                return Err(PluginError::ToolNameConflict {
                    name: spec.name,
                    id: descriptor.id.to_string(),
                });
            }
            if !names.insert(spec.name.clone()) {
                return Err(PluginError::ToolNameConflict {
                    name: spec.name,
                    id: descriptor.id.to_string(),
                });
            }
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.tool_providers.insert(descriptor.id, provider);
        Ok(())
    }

    /// 移除一个直接注册的 ToolProvider（loader 卸载时用）。manager 提供的工具
    /// 不能单个摘掉，必须走 manager 卸载，避免留下半套子树。
    pub fn remove_tool(&mut self, id: &ProviderId) -> Result<(), PluginError> {
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Tool {
            return Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Tool,
                found: descriptor.extension_point,
            });
        }
        if self.tool_providers.remove(id).is_none() {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: acb_i18n::t!("kernel.plugin.tool_manager_owned", id = id),
            });
        }
        self.descriptors.remove(id);
        Ok(())
    }

    /// 所有插件工具的目录（name + summary + provider）。目录不全量给 schema。
    pub fn plugin_tool_index(&self) -> Vec<(ProviderId, ToolSpec)> {
        let mut out = Vec::new();
        for descriptor in self
            .descriptors
            .values()
            .filter(|descriptor| descriptor.extension_point == ExtensionPoint::Tool)
        {
            let Some(provider) = self.tool_provider(&descriptor.id) else {
                continue;
            };
            for spec in provider.tools() {
                if crate::tools::TOOL_NAMES.contains(&spec.name.as_str()) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "kernel.plugin.tool_name_conflict",
                            name = spec.name,
                            id = descriptor.id
                        )
                    );
                    continue;
                }
                out.push((descriptor.id.clone(), spec));
            }
        }
        out.sort_by(|left, right| left.1.name.cmp(&right.1.name));
        out
    }

    /// 按工具名找到提供者；重名时返回错误（注册时已经拒绝，这里是防御）。
    pub fn tool_provider_for(&self, name: &str) -> Option<(ProviderId, Arc<dyn ToolProvider>)> {
        self.plugin_tool_index()
            .into_iter()
            .find(|(_, spec)| spec.name == name)
            .and_then(|(id, _)| self.tool_provider(&id).map(|provider| (id, provider)))
    }

    fn tool_provider(&self, id: &ProviderId) -> Option<Arc<dyn ToolProvider>> {
        if let Some(provider) = self.tool_providers.get(id) {
            return Some(provider.clone());
        }
        let manager = self.managers.get(id.namespace())?;
        match self.instantiate_safe(manager, id, ProviderOptions::Tool) {
            Ok(ProviderInstance::Tool(provider)) => Some(provider),
            Ok(other) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.plugin.tool_wrong_kind",
                        id = id,
                        found = format!("{:?}", other.extension_point())
                    )
                );
                None
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!("kernel.plugin.tool_failed", id = id, error = error)
                );
                None
            }
        }
    }

    fn declared_tool_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for provider in self.tool_providers.values() {
            for spec in provider.tools() {
                names.push(spec.name);
            }
        }
        for descriptor in self
            .descriptors
            .values()
            .filter(|descriptor| descriptor.extension_point == ExtensionPoint::Tool)
        {
            if self.tool_providers.contains_key(&descriptor.id) {
                continue;
            }
            if let Some(provider) = self.tool_provider(&descriptor.id) {
                for spec in provider.tools() {
                    names.push(spec.name);
                }
            }
        }
        names
    }

    /// 注册后台任务 handler。handler 名不能是内置 `exec`，也不能重复。
    pub fn register_task_handler(
        &mut self,
        descriptor: ProviderDescriptor,
        name: &str,
        handler: Arc<dyn crate::task::TaskHandler>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(&descriptor, ExtensionPoint::Task, Capability::TaskHandle)?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        if name == "exec" || self.task_handlers.contains_key(name) {
            return Err(PluginError::DuplicateProvider(name.to_owned()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.task_handlers
            .insert(name.to_owned(), (descriptor.id, handler));
        Ok(())
    }

    /// 按 handler 名取任务执行器。直接注册优先；否则把名字当 provider id 委托 manager。
    pub fn task_handler(&self, name: &str) -> Option<Arc<dyn crate::task::TaskHandler>> {
        if let Some((_, handler)) = self.task_handlers.get(name) {
            return Some(handler.clone());
        }
        let id = ProviderId::parse(name).ok()?;
        let descriptor = self.descriptors.get(&id)?;
        if descriptor.extension_point != ExtensionPoint::Task {
            return None;
        }
        let manager = self.managers.get(id.namespace())?;
        match self.instantiate_safe(manager, &id, ProviderOptions::Task) {
            Ok(ProviderInstance::Task(handler)) => Some(handler),
            Ok(other) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.plugin.task_wrong_kind",
                        id = id,
                        found = format!("{:?}", other.extension_point())
                    )
                );
                None
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!("kernel.plugin.task_failed", id = id, error = error)
                );
                None
            }
        }
    }

    /// 注册记录观察者。只读，不参与调度。
    pub fn register_observer(
        &mut self,
        descriptor: ProviderDescriptor,
        observer: Arc<dyn RecordObserver>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Observer,
            Capability::ObservedRecord,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.observers.insert(descriptor.id, observer);
        Ok(())
    }

    /// 把一条刚入库的记录交给所有观察者。顺序按 provider id 排序，保证可复现。
    ///
    /// 观察者异常不影响主流程；这里没有返回值，错误由 manager 自己记录健康状态。
    pub fn observe(&self, record: &Record) {
        let mut ids: Vec<&ProviderId> = self
            .descriptors
            .values()
            .filter(|descriptor| descriptor.extension_point == ExtensionPoint::Observer)
            .map(|descriptor| &descriptor.id)
            .collect();
        ids.sort();
        for id in ids {
            if let Some(observer) = self.observers.get(id) {
                observe_one(id, observer, record);
                continue;
            }
            let Some(manager) = self.managers.get(id.namespace()) else {
                continue;
            };
            match self.instantiate_safe(manager, id, ProviderOptions::Observer) {
                Ok(ProviderInstance::Observer(observer)) => observe_one(id, &observer, record),
                Ok(other) => eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.plugin.observer_wrong_kind",
                        id = id,
                        found = format!("{:?}", other.extension_point())
                    )
                ),
                Err(error) => eprintln!(
                    "{}",
                    acb_i18n::t!("kernel.plugin.observer_failed", id = id, error = error)
                ),
            }
        }
    }

    /// 注册工具访问否决点。和观察者一样，安装即生效；不额外占会话配置。
    pub fn register_tool_access(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn ToolAccessProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::ToolAccess,
            Capability::ToolAccessDecision,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.tool_access.insert(descriptor.id, provider);
        Ok(())
    }

    /// 依次询问所有工具访问 provider（按 provider id 排序，保证可复现）。
    ///
    /// 返回 `None` = 没人拦；`Some((provider, reason))` = 拒绝本次调用。
    /// provider 返回错误或 manager 实例化失败时按**失败关闭**处理：宁可拒绝，
    /// 也不让一个坏掉的策略插件变成隐式放行。
    pub fn check_tool_access(
        &self,
        context: &ToolAccessContext<'_>,
    ) -> Option<(ProviderId, String)> {
        let mut ids: Vec<&ProviderId> = self
            .descriptors
            .values()
            .filter(|descriptor| descriptor.extension_point == ExtensionPoint::ToolAccess)
            .map(|descriptor| &descriptor.id)
            .collect();
        ids.sort();
        for id in ids {
            let provider = if let Some(provider) = self.tool_access.get(id) {
                provider.clone()
            } else {
                let Some(manager) = self.managers.get(id.namespace()) else {
                    // descriptor 在、实现不在：加载器自己的健康问题，不影响 core 判定。
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.plugin.tool_access_missing", id = id)
                    );
                    continue;
                };
                match self.instantiate_safe(manager, id, ProviderOptions::ToolAccess) {
                    Ok(ProviderInstance::ToolAccess(provider)) => provider,
                    Ok(other) => {
                        let reason = acb_i18n::t!(
                            "kernel.plugin.tool_access_wrong_kind",
                            id = id,
                            found = format!("{:?}", other.extension_point())
                        );
                        eprintln!("{reason}");
                        return Some((id.clone(), reason));
                    }
                    Err(error) => {
                        let reason = acb_i18n::t!(
                            "kernel.plugin.tool_access_failed",
                            id = id,
                            error = error
                        );
                        eprintln!("{reason}");
                        return Some((id.clone(), reason));
                    }
                }
            };
            match provider.check(context) {
                Ok(ToolAccessVerdict::Allow) => {}
                Ok(ToolAccessVerdict::Deny { reason }) => return Some((id.clone(), reason)),
                Err(error) => {
                    let reason =
                        acb_i18n::t!("kernel.plugin.tool_access_error", id = id, error = error);
                    eprintln!("{reason}");
                    return Some((id.clone(), reason));
                }
            }
        }
        None
    }

    /// 注册上下文注入 provider。
    pub fn register_prompt(
        &mut self,
        descriptor: ProviderDescriptor,
        provider: Arc<dyn PromptProvider>,
    ) -> Result<(), PluginError> {
        self.check_descriptor(
            &descriptor,
            ExtensionPoint::Prompt,
            Capability::PromptContribute,
        )?;
        if self.descriptors.contains_key(&descriptor.id) {
            return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
        }
        self.descriptors
            .insert(descriptor.id.clone(), descriptor.clone());
        self.prompt_providers.insert(descriptor.id, provider);
        Ok(())
    }

    /// 按 provider id 取上下文注入 provider。只有会话配置显式引用的才会被调用。
    pub fn prompt_provider(&self, id: &ProviderId) -> Option<Arc<dyn PromptProvider>> {
        if let Some(provider) = self.prompt_providers.get(id) {
            return Some(provider.clone());
        }
        let descriptor = self.descriptors.get(id)?;
        if descriptor.extension_point != ExtensionPoint::Prompt {
            return None;
        }
        let manager = self.managers.get(id.namespace())?;
        match self.instantiate_safe(manager, id, ProviderOptions::Prompt) {
            Ok(ProviderInstance::Prompt(provider)) => Some(provider),
            Ok(other) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.plugin.prompt_wrong_kind",
                        id = id,
                        found = format!("{:?}", other.extension_point())
                    )
                );
                None
            }
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!("kernel.plugin.prompt_failed", id = id, error = error)
                );
                None
            }
        }
    }

    /// 安装一棵树外 manager 子树。它拥有自己的 namespace 和子树注册表；Root 只做
    /// namespace / 版本 / 能力 / id 审计，之后把它子树里的 id 委托给它。
    ///
    /// 递归安装子 manager，并检查层级上限与环路。整棵子树先预检、再落表：
    /// 任何一层不合格都不会留下半棵装好的子树。
    pub fn install_manager(&mut self, manager: Arc<dyn ManagerPlugin>) -> Result<(), PluginError> {
        let mut collected = Vec::new();
        let mut ancestry = Vec::new();
        self.collect_manager(manager, &mut ancestry, &mut collected)?;
        for entry in collected {
            for descriptor in entry.descriptors {
                if descriptor.extension_point == ExtensionPoint::PlatformAdapter {
                    if let Some(kind) = descriptor.kind.clone() {
                        self.adapter_providers.insert(kind, descriptor.id.clone());
                    }
                }
                self.descriptors.insert(descriptor.id.clone(), descriptor);
            }
            self.manager_order.push(entry.namespace.clone());
            self.managers.insert(entry.namespace, entry.manager);
        }
        Ok(())
    }

    /// 预检一棵子树：manifest、descriptor、深度、环路、重复 id/namespace。
    fn collect_manager(
        &self,
        manager: Arc<dyn ManagerPlugin>,
        ancestry: &mut Vec<String>,
        collected: &mut Vec<CollectedManager>,
    ) -> Result<(), PluginError> {
        let manifest = manager.manifest();
        let namespace = manifest.namespace.trim().to_owned();
        if namespace.is_empty() {
            return Err(PluginError::Manager {
                namespace: String::new(),
                detail: acb_i18n::t!("kernel.plugin.namespace_empty").to_owned(),
            });
        }
        if namespace == BUILTIN_NAMESPACE {
            return Err(PluginError::ReservedNamespace(namespace));
        }
        if ancestry.iter().any(|parent| parent == &namespace) {
            let mut path = ancestry.clone();
            path.push(namespace);
            return Err(PluginError::ManagerCycle(path.join(" -> ")));
        }
        if ancestry.len() >= MAX_MANAGER_DEPTH {
            return Err(PluginError::ManagerDepth {
                namespace,
                max: MAX_MANAGER_DEPTH,
            });
        }
        let namespace_ok = namespace
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
        if !namespace_ok {
            return Err(PluginError::Manager {
                namespace,
                detail: acb_i18n::t!("kernel.plugin.namespace_charset").to_owned(),
            });
        }
        if self.managers.contains_key(&namespace)
            || collected.iter().any(|entry| entry.namespace == namespace)
        {
            return Err(PluginError::DuplicateNamespace(namespace));
        }
        if manifest.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersion {
                id: format!("manager:{namespace}"),
                found: manifest.api_version,
                required: PLUGIN_API_VERSION,
            });
        }

        let mut descriptors = call_manager(&namespace, "descriptors", || manager.descriptors())?;
        for descriptor in &mut descriptors {
            descriptor.metadata.merge_missing(&manifest.metadata);
        }
        for descriptor in &descriptors {
            if descriptor.source != PluginSource::Builtin
                && descriptor
                    .metadata
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
            {
                return Err(PluginError::Manager {
                    namespace: namespace.clone(),
                    detail: acb_i18n::t!(
                        "kernel.plugin.provider_no_summary",
                        provider = descriptor.id
                    ),
                });
            }
            if descriptor.id.namespace() != namespace {
                return Err(PluginError::Manager {
                    namespace: namespace.clone(),
                    detail: acb_i18n::t!(
                        "kernel.plugin.descriptor_namespace_mismatch",
                        descriptor = descriptor.id,
                        namespace = namespace
                    ),
                });
            }
            let required = match descriptor.extension_point {
                ExtensionPoint::Wake => Capability::WakeDecision,
                ExtensionPoint::Access => Capability::AccessDecision,
                ExtensionPoint::Delivery => Capability::DeliverPlan,
                ExtensionPoint::Send => Capability::SendHook,
                ExtensionPoint::TurnError => Capability::TurnErrorDecision,
                ExtensionPoint::Clock => Capability::ClockDecision,
                ExtensionPoint::Receive => Capability::ReceiveDecision,
                ExtensionPoint::Route => Capability::RouteDecision,
                ExtensionPoint::Observer => Capability::ObservedRecord,
                ExtensionPoint::Tool => Capability::ToolCall,
                ExtensionPoint::Task => Capability::TaskHandle,
                ExtensionPoint::ToolAccess => Capability::ToolAccessDecision,
                ExtensionPoint::Prompt => Capability::PromptContribute,
                ExtensionPoint::PlatformAdapter => Capability::PlatformCatalog,
                point => {
                    return Err(PluginError::UnsupportedExtensionPoint {
                        id: descriptor.id.to_string(),
                        point,
                    });
                }
            };
            if descriptor.extension_point == ExtensionPoint::PlatformAdapter {
                let kind = descriptor.kind.as_deref().map(str::trim).unwrap_or("");
                if kind.is_empty() {
                    return Err(PluginError::Manager {
                        namespace: namespace.clone(),
                        detail: acb_i18n::t!(
                            "kernel.plugin.adapter_no_kind",
                            descriptor = descriptor.id
                        ),
                    });
                }
                let duplicate = self.adapters.contains_key(kind)
                    || self.adapter_providers.contains_key(kind)
                    || collected.iter().any(|entry| {
                        entry.descriptors.iter().any(|existing| {
                            existing.extension_point == ExtensionPoint::PlatformAdapter
                                && existing.kind.as_deref() == Some(kind)
                        })
                    });
                if duplicate {
                    return Err(PluginError::DuplicateProvider(acb_i18n::t!(
                        "kernel.plugin.duplicate_adapter_kind",
                        kind = kind
                    )));
                }
            } else if descriptor.kind.is_some() {
                return Err(PluginError::Manager {
                    namespace: namespace.clone(),
                    detail: acb_i18n::t!(
                        "kernel.plugin.kind_only_for_adapters",
                        descriptor = descriptor.id
                    ),
                });
            }
            if !manifest.capabilities.contains(&required) {
                return Err(PluginError::MissingCapability {
                    id: format!("manager:{namespace}"),
                    capability: required,
                });
            }
            self.check_descriptor(descriptor, descriptor.extension_point, required)?;
            if self.descriptors.contains_key(&descriptor.id)
                || collected.iter().any(|entry| {
                    entry
                        .descriptors
                        .iter()
                        .any(|existing| existing.id == descriptor.id)
                })
            {
                return Err(PluginError::DuplicateProvider(descriptor.id.to_string()));
            }
        }

        collected.push(CollectedManager {
            namespace: namespace.clone(),
            manager,
            descriptors,
        });
        ancestry.push(namespace);
        for child in collected.last().expect("just pushed").manager.children() {
            self.collect_manager(child, ancestry, collected)?;
        }
        ancestry.pop();
        Ok(())
    }

    /// 轮次边界热重载。返回失败的 namespace 和原因；一个子树失败不影响其他子树。
    pub fn reload_managers(&self, scope: &ReloadScope) -> Vec<(String, PluginError)> {
        let mut failures = Vec::new();
        for namespace in &self.manager_order {
            if !matches!(scope, ReloadScope::All)
                && !matches!(scope, ReloadScope::Namespace(wanted) if wanted == namespace)
            {
                continue;
            }
            let Some(manager) = self.managers.get(namespace) else {
                continue;
            };
            if let Err(error) = manager.reload(scope) {
                failures.push((namespace.clone(), error));
            }
        }
        failures
    }

    /// 指定 namespace 的 manager 是否已安装。
    pub fn has_manager(&self, namespace: &str) -> bool {
        self.managers.contains_key(namespace)
    }

    /// 指定 manager 的模块级信息（动态扩展目录用）。
    pub fn manager_details(&self, namespace: &str) -> Option<Value> {
        self.managers
            .get(namespace)
            .map(|manager| manager.details())
    }

    /// 指定 manager 的根目录（authoring 工具做路径 jail 用）。
    pub fn manager_root(&self, namespace: &str) -> Option<std::path::PathBuf> {
        self.managers
            .get(namespace)
            .and_then(|manager| manager.root())
    }

    /// 汇总所有 manager 的自报健康状态（按安装顺序）。
    pub fn manager_health(&self) -> Vec<ManagerHealth> {
        self.manager_order
            .iter()
            .filter_map(|namespace| {
                self.managers.get(namespace).map(|manager| ManagerHealth {
                    namespace: namespace.clone(),
                    health: manager_health(namespace, manager),
                })
            })
            .collect()
    }

    /// 进程退出前收尾。子 manager 先收，父 manager 后收。
    pub fn shutdown_managers(&self) {
        for namespace in self.manager_order.iter().rev() {
            if let Some(manager) = self.managers.get(namespace) {
                manager.shutdown();
            }
        }
    }

    /// 解析 wake provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    ///
    /// 要求 manager 的 provider 也必须先报 descriptor——这是 Root 做全局 id
    /// 唯一性和能力审计的前提，不允许“manager 藏着不报”。
    pub fn resolve_wake(
        &self,
        id: &ProviderId,
        options: &WakePolicy,
    ) -> Result<Arc<dyn WakeProvider>, PluginError> {
        if let Some(provider) = self.wake.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Wake {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::Wake(options))? {
            ProviderInstance::Wake(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Wake,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析 access provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_access(
        &self,
        id: &ProviderId,
        options: &AccessPolicy,
    ) -> Result<Arc<dyn AccessProvider>, PluginError> {
        if let Some(provider) = self.access.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Access {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::Access(options))? {
            ProviderInstance::Access(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Access,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析投递 composer。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_delivery(
        &self,
        id: &ProviderId,
        options: &DeliveryPolicy,
    ) -> Result<Arc<dyn DeliveryComposer>, PluginError> {
        if let Some(provider) = self.delivery.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Delivery {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::Delivery(options))? {
            ProviderInstance::Delivery(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Delivery,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析一个发送钩子。
    pub fn resolve_send_hook(&self, id: &ProviderId) -> Result<Arc<dyn SendHook>, PluginError> {
        if let Some(hook) = self.send_hooks.get(id) {
            return Ok(hook.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Send {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::None)? {
            ProviderInstance::Send(hook) => Ok(hook),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Send,
                found: other.extension_point(),
            }),
        }
    }

    /// 按配置顺序解析一组发送钩子。
    pub fn resolve_send_hooks(
        &self,
        ids: &[ProviderId],
    ) -> Result<Vec<Arc<dyn SendHook>>, PluginError> {
        ids.iter().map(|id| self.resolve_send_hook(id)).collect()
    }

    /// 解析失败通知 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_turn_error(
        &self,
        id: &ProviderId,
        options: &TurnErrorPolicy,
    ) -> Result<Arc<dyn TurnErrorProvider>, PluginError> {
        if let Some(provider) = self.turn_errors.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::TurnError {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::TurnError(options))? {
            ProviderInstance::TurnError(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::TurnError,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析时钟 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_clock(
        &self,
        id: &ProviderId,
        options: &ClockPolicy,
    ) -> Result<Arc<dyn ClockProvider>, PluginError> {
        if let Some(provider) = self.clocks.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Clock {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::Clock(options))? {
            ProviderInstance::Clock(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Clock,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析入站策略 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_receive(
        &self,
        id: &ProviderId,
        options: &ReceivePolicy,
    ) -> Result<Arc<dyn ReceiveProvider>, PluginError> {
        if let Some(provider) = self.receives.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Receive {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::Receive(options))? {
            ProviderInstance::Receive(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Receive,
                found: other.extension_point(),
            }),
        }
    }

    /// 解析消息路由 provider。先查 Root 直接注册的，再按 namespace 委托 manager。
    pub fn resolve_route(
        &self,
        id: &ProviderId,
        options: &RoutePolicy,
    ) -> Result<Arc<dyn RouteProvider>, PluginError> {
        if let Some(provider) = self.routes.get(id) {
            return Ok(provider.clone());
        }
        let Some(descriptor) = self.descriptors.get(id) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        if descriptor.extension_point != ExtensionPoint::Route {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: id.to_string(),
                point: descriptor.extension_point,
            });
        }
        let Some(manager) = self.managers.get(id.namespace()) else {
            return Err(PluginError::UnknownProvider(id.to_string()));
        };
        match self.instantiate_safe(manager, id, ProviderOptions::Route(options))? {
            ProviderInstance::Route(provider) => Ok(provider),
            other => Err(PluginError::WrongProviderKind {
                id: id.to_string(),
                expected: ExtensionPoint::Route,
                found: other.extension_point(),
            }),
        }
    }

    /// 启动时校验整份配置引用的 provider 都存在。`main` 在起线程前调用。
    pub fn validate_config(&self, config: &Config) -> Result<(), PluginError> {
        for connection in &config.connections {
            self.resolve_receive(&connection.receive.provider, &connection.receive)?;
            for route in &connection.routes {
                self.resolve_route(&route.provider, route)?;
            }
        }
        for session in &config.sessions {
            self.resolve_access(&session.access.provider, &session.access)?;
            self.resolve_delivery(&session.delivery.provider, &session.delivery)?;
            self.resolve_send_hooks(&session.outbound.before)?;
            self.resolve_send_hooks(&session.outbound.after)?;
            self.resolve_turn_error(&session.turn_error.provider, &session.turn_error)?;
            for subscription in &session.subscriptions {
                self.resolve_wake(&subscription.wake.provider, &subscription.wake)?;
                self.resolve_clock(&subscription.clock.provider, &subscription.clock)?;
            }
        }
        Ok(())
    }

    /// 当前 registry 里的 provider 描述（给控制台/测试审计用）。
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.descriptors.values().cloned().collect()
    }

    fn check_descriptor(
        &self,
        descriptor: &ProviderDescriptor,
        expected: ExtensionPoint,
        required: Capability,
    ) -> Result<(), PluginError> {
        if descriptor.extension_point != expected {
            return Err(PluginError::UnsupportedExtensionPoint {
                id: descriptor.id.to_string(),
                point: descriptor.extension_point,
            });
        }
        if descriptor.api_version != PLUGIN_API_VERSION {
            return Err(PluginError::ApiVersion {
                id: descriptor.id.to_string(),
                found: descriptor.api_version,
                required: PLUGIN_API_VERSION,
            });
        }
        if !descriptor.capabilities.contains(&required) {
            return Err(PluginError::MissingCapability {
                id: descriptor.id.to_string(),
                capability: required,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessPolicy, Config, WakePolicy};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_room() -> RoomId {
        RoomId::new("qq", "group/1").expect("合法")
    }

    fn context(room: &RoomId) -> WakeContext<'_> {
        WakeContext {
            session: "main",
            room,
            connection: "qq",
            platform: "qq",
            sender_id: "10001",
            text: "在吗",
            at_self: true,
            reply_to_self: false,
            has_body: true,
            event: None,
            tags: &[],
            unread: 1,
            last_wake_at: 0,
            now: 0,
        }
    }

    #[test]
    fn provider_id_parses_and_rejects_garbage() {
        let id = ProviderId::parse("acb.wake.default").expect("合法");
        assert_eq!(id.namespace(), "acb");
        assert_eq!(id.name(), "wake.default");
        assert_eq!(id.to_string(), "acb.wake.default");

        for bad in ["", "acb", ".x", "x.", "x y.z", "x/y.z"] {
            assert!(
                ProviderId::parse(bad).is_err(),
                "应当拒绝不合法的 provider id：{bad}"
            );
        }
    }

    #[test]
    fn native_loader_registers_the_default_wake_provider() {
        let manager = PluginManager::with_native_builtins();
        let provider = manager
            .resolve_wake(&ProviderId::builtin_wake(), &WakePolicy::default())
            .expect("内置 provider 应当可解析");
        let room = test_room();
        let ctx = context(&room);
        assert_eq!(
            provider.decide(&WakePolicy::default(), &ctx),
            WakeDecision::Wake
        );

        assert!(manager.descriptors().iter().any(|descriptor| {
            descriptor.id == ProviderId::builtin_wake()
                && descriptor.extension_point == ExtensionPoint::Wake
        }));
    }

    #[test]
    fn native_loader_registers_the_access_providers() {
        let manager = PluginManager::with_native_builtins();
        for id in [
            ProviderId::builtin_access(),
            ProviderId::builtin_all_rooms_access(),
            ProviderId::builtin_elevated_access(),
            ProviderId::builtin_privileged_origins_access(),
        ] {
            manager
                .resolve_access(&id, &AccessPolicy::default())
                .expect("内置 access provider 应当可解析");
        }
        assert_eq!(
            manager
                .descriptors()
                .iter()
                .filter(|descriptor| descriptor.extension_point == ExtensionPoint::Access)
                .count(),
            4
        );
    }

    #[test]
    fn access_providers_follow_the_configured_boundary() {
        let subscribed = test_room();
        let inside = RoomId::new("qq", "group/999").expect("合法");
        let other = RoomId::new("qq", "group/888").expect("合法");
        let subscriptions = vec![subscribed.clone()];
        fn request<'a>(
            room: &'a RoomId,
            action: AccessAction,
            subscriptions: &'a [RoomId],
        ) -> AccessRequest<'a> {
            AccessRequest {
                session: "main",
                connection: "qq",
                room: Some(room),
                origin_room: Some(room),
                action,
                subscriptions,
            }
        }
        let default = AccessPolicy::default();

        // 默认：只认订阅。
        assert_eq!(
            SubscribedAccessProvider.check(
                &default,
                &request(&subscribed, AccessAction::Send, &subscriptions)
            ),
            AccessDecision::Allow
        );
        assert_eq!(
            SubscribedAccessProvider.check(
                &default,
                &request(&inside, AccessAction::Read, &subscriptions)
            ),
            AccessDecision::Deny
        );

        // 全豁免：任何房间都行。
        assert_eq!(
            AllRoomsAccessProvider.check(
                &default,
                &request(&inside, AccessAction::Send, &subscriptions)
            ),
            AccessDecision::Allow
        );

        // 指定房间：列表里的行，列表外不行。
        let elevated_policy = AccessPolicy {
            provider: ProviderId::builtin_elevated_access(),
            rooms: vec![inside.clone()],
            ..Default::default()
        };
        assert_eq!(
            ElevatedInRoomsAccessProvider.check(
                &elevated_policy,
                &request(&inside, AccessAction::Read, &subscriptions)
            ),
            AccessDecision::Allow
        );
        assert_eq!(
            ElevatedInRoomsAccessProvider.check(
                &elevated_policy,
                &request(&other, AccessAction::Send, &subscriptions)
            ),
            AccessDecision::Deny
        );
    }

    #[test]
    fn privileged_origins_only_when_the_turn_started_there() {
        let origin = RoomId::new("qq", "private/7").expect("合法");
        let subscribed = test_room();
        let target = RoomId::new("qq", "group/999").expect("合法");
        let subscriptions = vec![subscribed.clone()];
        let policy = AccessPolicy {
            provider: ProviderId::builtin_privileged_origins_access(),
            origins: vec![origin.clone()],
            ..Default::default()
        };
        let request = AccessRequest {
            session: "main",
            connection: "qq",
            room: Some(&target),
            origin_room: Some(&origin),
            action: AccessAction::Send,
            subscriptions: &subscriptions,
        };
        assert_eq!(
            PrivilegedOriginsAccessProvider.check(&policy, &request),
            AccessDecision::Allow,
            "提权来源可以发到没订阅的房间"
        );

        // 同样的目标、不同的来源：回到订阅边界，拒绝。
        let denied = AccessRequest {
            origin_room: Some(&subscribed),
            ..request
        };
        assert_eq!(
            PrivilegedOriginsAccessProvider.check(&policy, &denied),
            AccessDecision::Deny,
            "非提权来源不能发到订阅外的房间"
        );

        // 非提权来源发自己订阅的房间照常允许。
        let own = AccessRequest {
            room: Some(&subscribed),
            origin_room: Some(&subscribed),
            ..request
        };
        assert_eq!(
            PrivilegedOriginsAccessProvider.check(&policy, &own),
            AccessDecision::Allow
        );
    }

    #[test]
    fn connection_access_follows_the_same_policy() {
        let subscribed = test_room();
        let subscriptions = vec![subscribed];
        let request = AccessRequest {
            session: "main",
            connection: "dc",
            room: None,
            origin_room: None,
            action: AccessAction::Connection,
            subscriptions: &subscriptions,
        };
        let default = AccessPolicy::default();
        assert_eq!(
            SubscribedAccessProvider.check(&default, &request),
            AccessDecision::Deny,
            "订阅里没有 dc 这条连接"
        );
        assert_eq!(
            AllRoomsAccessProvider.check(&default, &request),
            AccessDecision::Allow
        );

        let elevated_policy = AccessPolicy {
            provider: ProviderId::builtin_elevated_access(),
            rooms: vec![RoomId::new("dc", "channel/9").expect("合法")],
            ..Default::default()
        };
        assert_eq!(
            ElevatedInRoomsAccessProvider.check(&elevated_policy, &request),
            AccessDecision::Allow,
            "豁免房间在 dc 上，连接级也该放行"
        );
    }

    #[test]
    fn default_wake_provider_keeps_the_documented_behavior() {
        let room = test_room();
        let ctx = context(&room);
        let policy = WakePolicy::default();
        assert_eq!(
            DefaultWakeProvider.decide(&policy, &ctx),
            WakeDecision::Wake
        );

        // 没有 @、没有昵称、没有累积、不是引用：不醒。
        let quiet = WakeContext {
            at_self: false,
            text: "随便说说",
            unread: 1,
            ..ctx
        };
        assert_eq!(
            DefaultWakeProvider.decide(&policy, &quiet),
            WakeDecision::Stay
        );
    }

    struct AlwaysWake;

    impl WakeProvider for AlwaysWake {
        fn decide(&self, _options: &WakePolicy, _context: &WakeContext<'_>) -> WakeDecision {
            WakeDecision::Wake
        }
    }

    struct TestManager;

    impl ManagerPlugin for TestManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("test", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("test.always").expect("合法"),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "always" {
                Ok(ProviderInstance::Wake(Arc::new(AlwaysWake)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_namespace_is_delegated() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(TestManager))
            .expect("树外 manager 应当能安装");

        let id = ProviderId::parse("test.always").expect("合法");
        let provider = manager
            .resolve_wake(&id, &WakePolicy::default())
            .expect("manager 子树里的 provider 应当可解析");
        let room = test_room();
        let ctx = context(&room);
        assert_eq!(
            provider.decide(&WakePolicy::default(), &ctx),
            WakeDecision::Wake
        );

        let missing = ProviderId::parse("test.missing").expect("合法");
        assert!(matches!(
            manager.resolve_wake(&missing, &WakePolicy::default()),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    struct AccessManager;

    impl ManagerPlugin for AccessManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "vendor",
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("vendor.all_rooms").expect("合法"),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "all_rooms" {
                Ok(ProviderInstance::Access(Arc::new(AllRoomsAccessProvider)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_can_supply_access_providers() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(AccessManager))
            .expect("树外 manager 应当能装 access provider");

        let id = ProviderId::parse("vendor.all_rooms").expect("合法");
        manager
            .resolve_access(&id, &AccessPolicy::default())
            .expect("子树里的 access provider 应当可解析");
    }

    struct WrongKindManager;

    impl ManagerPlugin for WrongKindManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "wrong",
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("wrong.access").expect("合法"),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )]
        }

        fn instantiate(
            &self,
            _id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Ok(ProviderInstance::Wake(Arc::new(DefaultWakeProvider)))
        }
    }

    #[test]
    fn manager_returning_the_wrong_provider_kind_is_rejected() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(WrongKindManager))
            .expect("descriptor 本身合法");
        let id = ProviderId::parse("wrong.access").expect("合法");
        assert!(matches!(
            manager.resolve_access(&id, &AccessPolicy::default()),
            Err(PluginError::WrongProviderKind {
                expected: ExtensionPoint::Access,
                found: ExtensionPoint::Wake,
                ..
            })
        ));
    }

    #[test]
    fn tree_out_manager_cannot_take_the_builtin_namespace() {
        struct AcbManager;
        impl ManagerPlugin for AcbManager {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new("acb", PLUGIN_API_VERSION, Vec::new())
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                Vec::new()
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }

        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(AcbManager)),
            Err(PluginError::ReservedNamespace(_))
        ));
    }

    #[test]
    fn duplicate_provider_id_is_rejected() {
        let mut manager = PluginManager::with_native_builtins();
        let descriptor = ProviderDescriptor::new(
            ProviderId::builtin_wake(),
            ExtensionPoint::Wake,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        );
        assert!(matches!(
            manager.register_wake(descriptor, Arc::new(DefaultWakeProvider)),
            Err(PluginError::DuplicateProvider(_))
        ));
    }

    struct StubTask;

    impl crate::task::TaskHandler for StubTask {
        fn handle(
            &self,
            _task_id: &str,
            _payload: &Value,
        ) -> Result<crate::task::TaskOutcome, String> {
            Ok(crate::task::TaskOutcome::Nothing)
        }
    }

    struct DemoManager;

    impl ManagerPlugin for DemoManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("demo", PLUGIN_API_VERSION, Vec::new())
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            Vec::new()
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }

    #[test]
    fn task_handler_is_removed_with_its_registration_namespace() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(DemoManager))
            .expect("能装 demo manager");
        let id = ProviderId::parse("demo.echo").expect("合法");
        manager
            .register_task_handler(
                ProviderDescriptor::new(
                    id,
                    ExtensionPoint::Task,
                    PLUGIN_API_VERSION,
                    vec![Capability::TaskHandle],
                ),
                "echo",
                Arc::new(StubTask),
            )
            .expect("能注册");
        assert!(manager.task_handler("echo").is_some());

        manager.uninstall_manager("demo").expect("能卸载");
        assert!(
            manager.task_handler("echo").is_none(),
            "卸载 namespace 后 handler 不能残留"
        );
    }

    struct StubDeny;

    impl ToolAccessProvider for StubDeny {
        fn check(&self, _context: &ToolAccessContext<'_>) -> Result<ToolAccessVerdict, String> {
            Ok(ToolAccessVerdict::Deny {
                reason: "blocked".into(),
            })
        }
    }

    #[test]
    fn tool_access_provider_is_removed_with_its_namespace() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(DemoManager))
            .expect("能装 demo manager");
        let id = ProviderId::parse("demo.access").expect("合法");
        manager
            .register_tool_access(
                ProviderDescriptor::new(
                    id,
                    ExtensionPoint::ToolAccess,
                    PLUGIN_API_VERSION,
                    vec![Capability::ToolAccessDecision],
                ),
                Arc::new(StubDeny),
            )
            .expect("能注册");

        let room = test_room();
        let args = Value::Null;
        let context = ToolAccessContext {
            tool: "list_rooms",
            args: &args,
            session: "main",
            room: &room,
            platform: "qq",
            origin_sender: None,
            is_admin: false,
            live: true,
        };
        assert!(
            manager.check_tool_access(&context).is_some(),
            "注册后应当能拦"
        );

        manager.uninstall_manager("demo").expect("能卸载");
        assert!(
            manager.check_tool_access(&context).is_none(),
            "卸载 namespace 后否决点不能残留"
        );
    }

    struct StubPrompt;

    impl PromptProvider for StubPrompt {
        fn prompts(&self, _context: &PromptContext) -> Result<Vec<PromptDraft>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn prompt_provider_is_removed_with_its_namespace() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(DemoManager))
            .expect("能装 demo manager");
        let id = ProviderId::parse("demo.prompt").expect("合法");
        manager
            .register_prompt(
                ProviderDescriptor::new(
                    id.clone(),
                    ExtensionPoint::Prompt,
                    PLUGIN_API_VERSION,
                    vec![Capability::PromptContribute],
                ),
                Arc::new(StubPrompt),
            )
            .expect("能注册");
        assert!(manager.prompt_provider(&id).is_some());

        manager.uninstall_manager("demo").expect("能卸载");
        assert!(
            manager.prompt_provider(&id).is_none(),
            "卸载 namespace 后注入来源不能残留"
        );
    }

    /// 树外 manager 的嵌套子树：父 manager 自己不提供 provider，子 manager 提供。
    struct NestedManager {
        namespace: String,
        provide: bool,
        child: Option<Arc<dyn ManagerPlugin>>,
    }

    fn nested(
        namespace: &str,
        provide: bool,
        child: Option<Arc<dyn ManagerPlugin>>,
    ) -> Arc<dyn ManagerPlugin> {
        Arc::new(NestedManager {
            namespace: namespace.to_owned(),
            provide,
            child,
        })
    }

    impl ManagerPlugin for NestedManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                self.namespace.clone(),
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            if !self.provide {
                return Vec::new();
            }
            vec![ProviderDescriptor::new(
                ProviderId::parse(&format!("{}.always", self.namespace)).expect("合法"),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if self.provide && id.name() == "always" {
                Ok(ProviderInstance::Wake(Arc::new(AlwaysWake)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }

        fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
            self.child.clone().into_iter().collect()
        }
    }

    fn chain(levels: usize) -> Arc<dyn ManagerPlugin> {
        let mut current = None;
        for level in (1..=levels).rev() {
            current = Some(nested(&format!("level{level}"), true, current));
        }
        current.expect("至少一层")
    }

    #[test]
    fn nested_manager_subtree_is_installed_and_resolved() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(nested("parent", false, Some(nested("child", true, None))))
            .expect("嵌套子树应当能安装");

        let id = ProviderId::parse("child.always").expect("合法");
        manager
            .resolve_wake(&id, &WakePolicy::default())
            .expect("子 manager 的 provider 应当可解析");

        let health = manager.manager_health();
        assert_eq!(
            health
                .iter()
                .map(|entry| entry.namespace.as_str())
                .collect::<Vec<_>>(),
            vec!["parent", "child"],
            "安装顺序应该先父后子"
        );
    }

    struct PassRoute;

    impl RouteProvider for PassRoute {
        fn decide(&self, _options: &RoutePolicy, _context: &RouteContext<'_>) -> RouteDecision {
            RouteDecision::Pass
        }
    }

    struct RouteChild;

    impl ManagerPlugin for RouteChild {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "childroute",
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision, Capability::RouteDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![
                ProviderDescriptor::new(
                    ProviderId::parse("childroute.always").expect("合法"),
                    ExtensionPoint::Wake,
                    PLUGIN_API_VERSION,
                    vec![Capability::WakeDecision],
                ),
                ProviderDescriptor::new(
                    ProviderId::parse("childroute.route").expect("合法"),
                    ExtensionPoint::Route,
                    PLUGIN_API_VERSION,
                    vec![Capability::RouteDecision],
                ),
            ]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            match id.name() {
                "always" => Ok(ProviderInstance::Wake(Arc::new(AlwaysWake))),
                "route" => Ok(ProviderInstance::Route(Arc::new(PassRoute))),
                _ => Err(PluginError::UnknownProvider(id.to_string())),
            }
        }
    }

    #[test]
    fn uninstalling_parent_removes_child_providers_and_routes() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(nested("parent", false, Some(Arc::new(RouteChild))))
            .expect("嵌套子树应当能安装");

        let wake_id = ProviderId::parse("childroute.always").expect("合法");
        let route_id = ProviderId::parse("childroute.route").expect("合法");
        manager
            .resolve_wake(&wake_id, &WakePolicy::default())
            .expect("子 manager 的 wake provider 应当可解析");
        manager
            .resolve_route(
                &route_id,
                &RoutePolicy {
                    provider: route_id.clone(),
                    options: serde_json::json!({}),
                },
            )
            .expect("子 manager 的 route provider 应当可解析");

        manager
            .uninstall_manager("parent")
            .expect("递归卸载 parent 子树");

        assert!(
            manager
                .resolve_wake(&wake_id, &WakePolicy::default())
                .is_err(),
            "child wake provider 不能残留"
        );
        assert!(
            manager
                .resolve_route(
                    &route_id,
                    &RoutePolicy {
                        provider: route_id.clone(),
                        options: serde_json::json!({}),
                    },
                )
                .is_err(),
            "child route provider 不能残留"
        );
        assert!(
            manager.info(&wake_id).is_none(),
            "child descriptor 不能残留"
        );
        assert!(
            manager.info(&route_id).is_none(),
            "child route descriptor 不能残留"
        );
        assert!(manager.manager_health().is_empty(), "子树 manager 不能残留");
    }

    #[test]
    fn manager_depth_is_limited() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(chain(MAX_MANAGER_DEPTH))
            .expect("上限内应当能装");

        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(chain(MAX_MANAGER_DEPTH + 1)),
            Err(PluginError::ManagerDepth { max, .. }) if max == MAX_MANAGER_DEPTH
        ));
    }

    struct SelfCycle;

    impl ManagerPlugin for SelfCycle {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("loop", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            Vec::new()
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }

        fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
            vec![Arc::new(SelfCycle)]
        }
    }

    #[test]
    fn manager_cycle_is_rejected() {
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(SelfCycle)),
            Err(PluginError::ManagerCycle(path)) if path == "loop -> loop"
        ));
    }

    #[test]
    fn manager_manifest_is_checked() {
        struct BadNamespace;
        impl ManagerPlugin for BadNamespace {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new("bad", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                vec![ProviderDescriptor::new(
                    ProviderId::parse("other.always").expect("合法"),
                    ExtensionPoint::Wake,
                    PLUGIN_API_VERSION,
                    vec![Capability::WakeDecision],
                )]
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(BadNamespace)),
            Err(PluginError::Manager { .. })
        ));

        struct BadVersion;
        impl ManagerPlugin for BadVersion {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new(
                    "badver",
                    PLUGIN_API_VERSION + 1,
                    vec![Capability::WakeDecision],
                )
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                Vec::new()
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(BadVersion)),
            Err(PluginError::ApiVersion { .. })
        ));

        struct MissingCapability;
        impl ManagerPlugin for MissingCapability {
            fn manifest(&self) -> ManagerManifest {
                ManagerManifest::new("nocap", PLUGIN_API_VERSION, Vec::new())
            }
            fn descriptors(&self) -> Vec<ProviderDescriptor> {
                vec![ProviderDescriptor::new(
                    ProviderId::parse("nocap.always").expect("合法"),
                    ExtensionPoint::Wake,
                    PLUGIN_API_VERSION,
                    vec![Capability::WakeDecision],
                )]
            }
            fn instantiate(
                &self,
                id: &ProviderId,
                _options: ProviderOptions<'_>,
            ) -> Result<ProviderInstance, PluginError> {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_manager(Arc::new(MissingCapability)),
            Err(PluginError::MissingCapability {
                capability: Capability::WakeDecision,
                ..
            })
        ));
    }

    #[derive(Default)]
    struct LifecycleManager {
        reloads: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    impl ManagerPlugin for LifecycleManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("life", PLUGIN_API_VERSION, vec![Capability::WakeDecision])
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            Vec::new()
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }

        fn reload(&self, _scope: &ReloadScope) -> Result<(), PluginError> {
            self.reloads.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn shutdown(&self) {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
        }

        fn health(&self) -> Health {
            Health::Degraded("测试状态".to_owned())
        }
    }

    #[test]
    fn manager_lifecycle_hooks_are_driven_by_root() {
        let lifecycle = Arc::new(LifecycleManager::default());
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(lifecycle.clone())
            .expect("应当能安装");

        assert!(manager.reload_managers(&ReloadScope::All).is_empty());
        assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 1);

        // 范围不匹配的 namespace 不该被打扰。
        assert!(
            manager
                .reload_managers(&ReloadScope::Namespace("other".to_owned()))
                .is_empty()
        );
        assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 1);

        assert!(
            manager
                .reload_managers(&ReloadScope::Namespace("life".to_owned()))
                .is_empty()
        );
        assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 2);

        assert_eq!(
            manager.manager_health(),
            vec![ManagerHealth {
                namespace: "life".to_owned(),
                health: Health::Degraded("测试状态".to_owned()),
            }]
        );

        manager.shutdown_managers();
        assert_eq!(lifecycle.shutdowns.load(Ordering::SeqCst), 1);
    }

    struct PanicDescriptorsManager;

    impl ManagerPlugin for PanicDescriptorsManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("panicdesc", PLUGIN_API_VERSION, Vec::new())
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            panic!("descriptors exploded");
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }

    #[test]
    fn panicking_descriptors_becomes_install_error() {
        let mut manager = PluginManager::with_native_builtins();
        let error = manager
            .install_manager(Arc::new(PanicDescriptorsManager))
            .expect_err("descriptors panic 要变成安装错误");
        match error {
            PluginError::Manager { namespace, detail } => {
                assert_eq!(namespace, "panicdesc");
                assert!(detail.contains("descriptors"), "{detail}");
                assert!(detail.contains("descriptors exploded"), "{detail}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            manager.manager_health().is_empty(),
            "预检失败不能留下半棵 manager 子树"
        );
    }

    struct PanicHealthManager;

    impl ManagerPlugin for PanicHealthManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new("panichealth", PLUGIN_API_VERSION, Vec::new())
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            Vec::new()
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            Err(PluginError::UnknownProvider(id.to_string()))
        }

        fn health(&self) -> Health {
            panic!("health exploded");
        }
    }

    #[test]
    fn panicking_health_is_reported_not_propagated() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(PanicHealthManager))
            .expect("健康检查 panic 不影响安装");

        let health = manager.manager_health();
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].namespace, "panichealth");
        assert!(
            matches!(&health[0].health, Health::Unhealthy(detail) if detail.contains("health exploded")),
            "panic 要变成 Unhealthy：{:?}",
            health[0].health
        );

        // 第二次读取仍正常：panic 不能让结构或锁状态坏掉。
        assert_eq!(manager.manager_health().len(), 1);
    }

    struct BadVersionLoader;

    impl PluginLoader for BadVersionLoader {
        fn manifest(&self) -> LoaderManifest {
            LoaderManifest::new(
                LoaderKind::Native,
                PLUGIN_API_VERSION + 1,
                vec![ExtensionPoint::Wake],
            )
        }
        fn install(&self, _manager: &mut PluginManager) -> Result<(), PluginError> {
            Ok(())
        }
    }

    struct UnsupportedExtensionLoader;

    impl PluginLoader for UnsupportedExtensionLoader {
        fn manifest(&self) -> LoaderManifest {
            LoaderManifest::new(
                LoaderKind::Process,
                PLUGIN_API_VERSION,
                vec![ExtensionPoint::Manager],
            )
        }
        fn install(&self, _manager: &mut PluginManager) -> Result<(), PluginError> {
            Ok(())
        }
    }

    struct EmptyLoader;

    impl PluginLoader for EmptyLoader {
        fn manifest(&self) -> LoaderManifest {
            LoaderManifest::new(LoaderKind::Script, PLUGIN_API_VERSION, Vec::new())
        }
        fn install(&self, _manager: &mut PluginManager) -> Result<(), PluginError> {
            Ok(())
        }
    }

    #[test]
    fn loader_manifest_is_checked() {
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.install_loader(&BadVersionLoader),
            Err(PluginError::ApiVersion { .. })
        ));
        assert!(matches!(
            manager.install_loader(&UnsupportedExtensionLoader),
            Err(PluginError::UnsupportedExtensionPoint {
                point: ExtensionPoint::Manager,
                ..
            })
        ));
        assert!(matches!(
            manager.install_loader(&EmptyLoader),
            Err(PluginError::Loader(_))
        ));

        assert_eq!(
            NativeLoader.manifest().extensions,
            vec![
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::TurnError,
                ExtensionPoint::Clock,
                ExtensionPoint::Receive,
                ExtensionPoint::Route
            ]
        );
    }

    #[test]
    fn native_loader_refuses_to_unload_by_default() {
        let mut manager = PluginManager::with_native_builtins();
        assert!(matches!(
            NativeLoader.unload(&mut manager, &ProviderId::builtin_wake()),
            Err(PluginError::Loader(_))
        ));
    }

    #[test]
    fn validate_config_rejects_an_unknown_provider_before_start() {
        let text = r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"

            [sessions.subscriptions.wake]
            provider = "vendor.missing"
        "#;
        let config = Config::parse(text).expect("语法合法");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    #[test]
    fn validate_config_rejects_an_unknown_access_provider_before_start() {
        let text = r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            [sessions.access]
            provider = "vendor.missing"

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"
        "#;
        let config = Config::parse(text).expect("语法合法");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    fn config_with_extra(extra: &str) -> Config {
        let text = format!(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
            text = "你是桥。"

            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"

            {extra}

            [[sessions.subscriptions]]
            connection = "qq"
            room = "group/1"
            "#
        );
        Config::parse(&text).expect("语法合法")
    }

    #[test]
    fn validate_config_rejects_an_unknown_delivery_provider_before_start() {
        let config = config_with_extra("[sessions.delivery]\nprovider = \"vendor.missing\"");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    #[test]
    fn interval_idle_clock_respects_thresholds() {
        let room = test_room();
        let policy = ClockPolicy {
            provider: ProviderId::builtin_clock_interval_idle(),
            interval_secs: Some(60),
            idle_secs: Some(120),
            ..Default::default()
        };
        let context = ClockContext {
            session: "main",
            room: &room,
            now: 0,
            last_clock_at: 0,
            last_wake_at: 0,
            last_record_at: None,
        };
        assert!(!ClockOffProvider.is_due(&policy, &context));
        assert!(!IntervalIdleClockProvider.is_due(&policy, &context));

        let due_interval = ClockContext {
            now: 61_000,
            ..context
        };
        assert!(IntervalIdleClockProvider.is_due(&policy, &due_interval));

        let idle_only = ClockPolicy {
            interval_secs: None,
            idle_secs: Some(120),
            ..policy
        };
        let due_idle = ClockContext {
            now: 120_000,
            last_clock_at: 0,
            last_record_at: Some(0),
            ..context
        };
        assert!(IntervalIdleClockProvider.is_due(&idle_only, &due_idle));
        let not_idle = ClockContext {
            now: 100_000,
            ..due_idle
        };
        assert!(!IntervalIdleClockProvider.is_due(&idle_only, &not_idle));
    }

    struct NeverDueClock;

    impl ClockProvider for NeverDueClock {
        fn is_due(&self, _options: &ClockPolicy, _context: &ClockContext<'_>) -> bool {
            false
        }
    }

    struct ClockManager;

    impl ManagerPlugin for ClockManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "ticker",
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("ticker.never").expect("合法"),
                ExtensionPoint::Clock,
                PLUGIN_API_VERSION,
                vec![Capability::ClockDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "never" {
                Ok(ProviderInstance::Clock(Arc::new(NeverDueClock)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_can_supply_clock_providers() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(ClockManager))
            .expect("应当能安装");
        let id = ProviderId::parse("ticker.never").expect("合法");
        manager
            .resolve_clock(&id, &ClockPolicy::default())
            .expect("时钟 provider 应当可解析");
    }

    #[test]
    fn default_turn_error_provider_keeps_the_config_behavior() {
        let room = test_room();
        let session_actions = vec![TurnErrorAction::Send { room: room.clone() }];
        let default_actions = vec![TurnErrorAction::Log];
        let request = TurnErrorRequest {
            session: "main",
            room: &room,
            error: "boom",
            actions: &session_actions,
            default_actions: &default_actions,
        };
        assert_eq!(
            DefaultTurnErrorProvider.decide(&TurnErrorPolicy::default(), &request),
            session_actions
        );

        let empty: Vec<TurnErrorAction> = Vec::new();
        let request = TurnErrorRequest {
            actions: &empty,
            ..request
        };
        assert_eq!(
            DefaultTurnErrorProvider.decide(&TurnErrorPolicy::default(), &request),
            default_actions
        );
    }

    struct NoopTurnErrorProvider;

    impl TurnErrorProvider for NoopTurnErrorProvider {
        fn decide(
            &self,
            _options: &TurnErrorPolicy,
            _request: &TurnErrorRequest<'_>,
        ) -> Vec<TurnErrorAction> {
            Vec::new()
        }
    }

    struct TurnErrorManager;

    impl ManagerPlugin for TurnErrorManager {
        fn manifest(&self) -> ManagerManifest {
            ManagerManifest::new(
                "vendor",
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )
        }

        fn descriptors(&self) -> Vec<ProviderDescriptor> {
            vec![ProviderDescriptor::new(
                ProviderId::parse("vendor.silent").expect("合法"),
                ExtensionPoint::TurnError,
                PLUGIN_API_VERSION,
                vec![Capability::TurnErrorDecision],
            )]
        }

        fn instantiate(
            &self,
            id: &ProviderId,
            _options: ProviderOptions<'_>,
        ) -> Result<ProviderInstance, PluginError> {
            if id.name() == "silent" {
                Ok(ProviderInstance::TurnError(Arc::new(NoopTurnErrorProvider)))
            } else {
                Err(PluginError::UnknownProvider(id.to_string()))
            }
        }
    }

    #[test]
    fn manager_can_supply_turn_error_providers() {
        let mut manager = PluginManager::with_native_builtins();
        manager
            .install_manager(Arc::new(TurnErrorManager))
            .expect("应当能安装");
        let id = ProviderId::parse("vendor.silent").expect("合法");
        let provider = manager
            .resolve_turn_error(&id, &TurnErrorPolicy::default())
            .expect("失败通知 provider 应当可解析");
        let room = test_room();
        let request = TurnErrorRequest {
            session: "main",
            room: &room,
            error: "boom",
            actions: &[],
            default_actions: &[],
        };
        assert!(
            provider
                .decide(&TurnErrorPolicy::default(), &request)
                .is_empty()
        );
    }

    #[test]
    fn validate_config_rejects_an_unknown_turn_error_provider_before_start() {
        let config = config_with_extra("[sessions.turn_error]\nprovider = \"vendor.missing\"");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }

    #[test]
    fn validate_config_rejects_an_unknown_send_hook_before_start() {
        let config = config_with_extra("[sessions.outbound]\nbefore = [\"vendor.missing\"]");
        let manager = PluginManager::with_native_builtins();
        assert!(matches!(
            manager.validate_config(&config),
            Err(PluginError::UnknownProvider(_))
        ));
    }
}
