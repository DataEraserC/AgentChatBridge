//! 平台适配器 wire 协议（ADR-0027）。
//!
//! 同一份 JSON 消息定义被两种载体共用：
//! - **外置进程**：stdin/stdout 上一行一条 JSON；
//! - **动态库**：C ABI 的 `invoke` 函数收到/返回同样形状的 JSON。
//!
//! core 侧只认识这个形状，不认识平台；适配器实现可以写在任何语言里。
//!
//! 消息方向：
//!
//! ```text
//! core ──▶ adapter   Send / Call / Shutdown
//! adapter ──▶ core   Hello（第一行）/ Event / Reply / Log
//! ```

use acb_proto::{Element, Omitted, Record, RecordKind, RoomId, Sender};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize as DeriveSerialize};
use serde_json::Value;

/// wire 协议版本。core 与适配器必须对得上。
pub const WIRE_VERSION: u32 = 1;

/// 动态库适配器的 C ABI。
///
/// 只有 `acb_adapter_entry_v1` 一个导出符号；其余函数通过 [`AdapterV1`] 表拿。
/// 所有跨 ABI 的文本都是 JSON，扩展点靠 `op` / `type` 字段演进，不改 ABI 形状。
pub mod abi {
    use std::ffi::{c_char, c_void};

    /// 动态库 ABI 版本。
    pub const ABI_VERSION: u32 = 1;
    /// 唯一的导出符号名。
    pub const ENTRY_SYMBOL: &[u8] = b"acb_adapter_entry_v1\0";

    /// unload-safe native plugin ABI v2（ADR-0030）。
    pub const ABI_VERSION_V2: u32 = 2;
    pub const ENTRY_SYMBOL_V2: &[u8] = b"acb_plugin_entry_v2\0";
    /// 插件声明：满足卸载契约，允许 loader 在安全点 dlclose。
    pub const PLUGIN_FLAG_UNLOAD_SAFE: u32 = 1;

    pub type EntryFnV2 = unsafe extern "C" fn() -> *const AcbPluginV2;

    pub type SubmitEventFn = unsafe extern "C" fn(ctx: *mut c_void, message_json: *const c_char);
    pub type ShouldStopFn = unsafe extern "C" fn(ctx: *mut c_void) -> i32;
    pub type LogFn =
        unsafe extern "C" fn(ctx: *mut c_void, level: *const c_char, message: *const c_char);
    pub type ManifestFn = unsafe extern "C" fn() -> *mut c_char;
    pub type CreateFn =
        unsafe extern "C" fn(config_json: *const c_char, host: *const HostV1) -> *mut c_void;
    pub type InvokeFn =
        unsafe extern "C" fn(handle: *mut c_void, request_json: *const c_char) -> *mut c_char;
    pub type DestroyFn = unsafe extern "C" fn(handle: *mut c_void);
    pub type FreeStringFn = unsafe extern "C" fn(text: *mut c_char);
    pub type EntryFn = unsafe extern "C" fn() -> *const AdapterV1;

    /// core 传给插件的回调。插件必须在 `create` 里**复制**这份表，不要保存指针。
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct HostV1 {
        pub abi_version: u32,
        pub ctx: *mut c_void,
        pub submit_event: Option<SubmitEventFn>,
        pub should_stop: Option<ShouldStopFn>,
        pub log: Option<LogFn>,
    }

    /// 插件导出的函数表。函数指针为 None 表示不支持。
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct AdapterV1 {
        pub abi_version: u32,
        /// 返回 JSON `WireManifest`；调用方用 `free_string` 释放。
        pub manifest: Option<ManifestFn>,
        /// 按 `WireConnection` JSON 创建实例；返回不透明的 handle。
        pub create: Option<CreateFn>,
        /// 请求是 `CoreMessage` JSON，返回 `AdapterMessage` JSON（`Reply` 或 `Log`）。
        pub invoke: Option<InvokeFn>,
        pub destroy: Option<DestroyFn>,
        pub free_string: Option<FreeStringFn>,
    }

    /// ABI v2 函数表。字段前缀与 v1 兼容，额外带 struct_size 和 flags。
    #[derive(Clone, Copy)]
    #[repr(C)]
    pub struct AcbPluginV2 {
        pub abi_version: u32,
        pub struct_size: u32,
        pub flags: u32,
        pub manifest: Option<ManifestFn>,
        pub create: Option<CreateFn>,
        pub invoke: Option<InvokeFn>,
        pub destroy: Option<DestroyFn>,
        pub free_string: Option<FreeStringFn>,
    }
}

/// 插件 manifest 的 wire 形状。
///
/// `extension` 决定这是平台适配器还是别的扩展点（当前 P4 支持 `wake`）。
/// 平台适配器用 `kind` / `transports`；策略 provider 用 `id` 作为 provider id。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireManifest {
    pub id: String,
    pub platform: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub transports: Vec<String>,
    pub api_version: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// 会写进流水的平台事件名（不带平台前缀），如 `poke`、`reaction_add`。
    #[serde(default)]
    pub events: Vec<String>,
    /// 给事件打的跨平台语义标签，如 `poke`、`reaction`、`notice`。
    #[serde(default)]
    pub event_tags: Vec<String>,
    /// 入站记录里的 `at_self` 是否可信。
    #[serde(default)]
    pub supports_at: bool,
    /// 入站记录里的 `reply_to_self` 是否可信。
    #[serde(default)]
    pub supports_reply: bool,
    /// 单条文本消息的建议上限（字符）；None = 未声明，不等于无限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_limit: Option<usize>,
    /// `platform_adapter`（默认）或 `wake`。
    #[serde(default = "default_extension")]
    pub extension: String,
    /// 给 agent / 控制台看的说明。描述不是保证，效果由 core 审计。
    #[serde(default)]
    pub metadata: WirePluginMetadata,
    /// 这条连接的默认一对一房间（完整 room id，如 `qq:private/10001`）。
    /// 审批路由的全局默认只从这里取，core 不再拼 `private/{account}`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_room: Option<String>,
}

/// 插件作者声明的描述；core 原样搬进 Root 目录。
#[derive(Debug, Clone, Default, PartialEq, DeriveSerialize, Deserialize)]
pub struct WirePluginMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// `may_wake` / `may_stay_silent` / `may_delay` / `may_modify_text` /
    /// `may_drop` / `external_call` / `nondeterministic`。
    #[serde(default)]
    pub effects: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options_schema: Option<Value>,
}

/// 一条连接的 wire 形状（动态库接管适配器实例化时用；连接注册协议同形）。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireConnection {
    pub id: String,
    pub platform: String,
    pub kind: String,
    pub transport: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub account: String,
    #[serde(default)]
    pub admin_qqs: Vec<String>,
    #[serde(default)]
    pub options: Value,
    /// secrets 体系的引用名（连接注册用）。适配器库不存明文凭据时填这个，
    /// core 在应用注册时解析；`token` 与 `token_secret` 都缺 = 无凭据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<String>,
    /// 消息路由声明（JSON 数组，`RoutePolicy` 形）。缺省 = 全部交给 AI。
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub routes: Value,
}

/// 动态库 `create` 收到的实例配置。
///
/// 适配器和策略 provider 共用同一个入口，用 `kind` 区分：
/// - `adapter`：一条平台连接；
/// - `provider`：一个策略实例（如 wake），`options` 是对应策略的配置。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireInstanceConfig {
    Adapter {
        connection: WireConnection,
    },
    Provider {
        provider: String,
        extension: String,
        #[serde(default)]
        options: Value,
    },
}

/// wake 判定的 wire 形状；插件只读，不写。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireWakeRequest {
    pub session: String,
    pub room: String,
    pub connection: String,
    pub platform: String,
    pub sender_id: String,
    pub text: String,
    #[serde(default)]
    pub at_self: bool,
    #[serde(default)]
    pub reply_to_self: bool,
    /// 有没有正文。只有 @ / 引用 / 空白文本时为 false；平台事件恒为 false。
    #[serde(default)]
    pub has_body: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub last_wake_at: i64,
    #[serde(default)]
    pub now: i64,
}

/// access 策略判定的 wire 形状。`room = null` 表示连接级请求。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireAccessRequest {
    pub session: String,
    pub connection: String,
    /// 要访问的目标房间；连接级请求为 `null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// 当前轮次是从哪个房间触发的；旧插件不读这个字段时按 `null` 处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_room: Option<String>,
    /// 本轮主体与场所；旧插件不读时按空串处理。
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub place: String,
    pub action: String,
    #[serde(default)]
    pub subscriptions: Vec<String>,
}

/// receive 策略判定的 wire 形状。字段与 kernel 的 `ReceiveRequest` 对齐。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireReceiveRequest {
    pub connection: String,
    pub platform: String,
    pub room: String,
    pub sender: Sender,
    pub kind: RecordKind,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub at_self: bool,
}

/// clock 策略判定的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireClockRequest {
    pub session: String,
    pub room: String,
    pub now: i64,
    pub last_clock_at: i64,
    #[serde(default)]
    pub last_wake_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_record_at: Option<i64>,
}

/// 投递批次选择的 wire 形状。`records` 是这一轮的候选流水（含 id）。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireDeliveryRequest {
    pub session: String,
    pub room: String,
    pub limit: usize,
    #[serde(default)]
    pub records: Vec<Record>,
    /// 触发消息之前的上下文（可能已确认过）。
    #[serde(default)]
    pub context: Vec<Record>,
    /// 触发这一轮的消息编号；时钟/攒批唤醒为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<u64>,
    #[serde(default)]
    pub context_before: usize,
    #[serde(default)]
    pub context_after: usize,
    #[serde(default)]
    pub batch_head: usize,
}

/// composer 选出的编号与跳过的中段；字段语义同 kernel 的 `DeliveryPlan`。
#[derive(Debug, Clone, Default, PartialEq, Eq, DeriveSerialize, Deserialize)]
pub struct WireDeliveryPlan {
    #[serde(default)]
    pub keep: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<Omitted>,
    /// 完整未读区间；空 = core 自己算。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_ranges: Vec<[u64; 2]>,
}

/// 失败通知动作的 wire 形状。
#[derive(Debug, Clone, PartialEq, Eq, DeriveSerialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireTurnErrorAction {
    Log,
    Send { room: String },
}

/// 失败通知 provider 的输入；动作列表原样给出，插件只负责挑。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireTurnErrorRequest {
    pub session: String,
    pub room: String,
    pub error: String,
    #[serde(default)]
    pub actions: Vec<WireTurnErrorAction>,
    #[serde(default)]
    pub default_actions: Vec<WireTurnErrorAction>,
}

/// 发送钩子的 wire 形状。`phase = "before"` 带 `elements`，`"after"` 带 `sent`。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireSendRequest {
    pub session: String,
    pub room: String,
    pub phase: String,
    pub origin: String,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

/// 一条入站记录。字段与 kernel 的 `RecordDraft` 对齐，由 core 负责转换。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireRecord {
    pub room: RoomId,
    pub kind: RecordKind,
    pub sender: Sender,
    /// Unix 毫秒。
    pub at: i64,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default)]
    pub at_self: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(default)]
    pub reply_to_self: bool,
    /// 事件指向的平台对象编号（例如撤回事件指向被撤回的消息）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_platform_id: Option<String>,
    /// 给目标记录追加的标签（如 `recall`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target_labels: Vec<String>,
    /// 只进流水，不投递、不唤醒、不计未读。标记会落库，之后新加的订阅者也生效。
    #[serde(default)]
    pub suppressed: bool,
    /// 房间结构事实：`direct` / `multi_party`。缺省 = 未声明，core 按 `multi_party`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_scope: Option<String>,
    /// 可选房间标签（如 `console`）。core 只做展示/场所映射。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub room_labels: Vec<String>,
}

/// 一个方法参数的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireParamSpec {
    pub name: String,
    #[serde(default = "default_param_type")]
    pub ty: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub desc: String,
    #[serde(default)]
    pub values: Vec<String>,
    /// 平台给的示例值；core 不再按参数名猜平台语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub example: Option<serde_json::Value>,
}

/// 一个房间目标的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireTargetSpec {
    pub param: String,
    pub room_kind: String,
    #[serde(default = "default_target_role")]
    pub role: String,
}

/// 一个平台方法声明的可信度。
#[derive(Debug, Clone, PartialEq, Eq, DeriveSerialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WireVerification {
    Verified { source: String },
    Unverified { reason: String, experiment: String },
}

/// 一个平台方法的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WirePermissionSpec {
    pub grant: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_permission")]
    pub default: String,
}

fn default_permission() -> String {
    "ask".to_owned()
}

/// 一个平台方法的 wire 形状。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireMethodSpec {
    pub name: String,
    /// 别名 op。目录不展开别名；core 解析时归一化到主 op。
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_risk")]
    pub risk: String,
    #[serde(default = "default_scope")]
    pub scope: String,
    #[serde(default)]
    pub params: Vec<WireParamSpec>,
    #[serde(default)]
    pub targets: Vec<WireTargetSpec>,
    #[serde(default = "default_effect")]
    pub effect: String,
    #[serde(default = "default_record")]
    pub record: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<WirePermissionSpec>>,
    pub verification: WireVerification,
}

/// 一次发送/调用顺带产生的流水记录。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireOutgoingRecord {
    pub kind: WireOutgoingKind,
    #[serde(default)]
    pub elements: Vec<Element>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireOutgoingKind {
    Message,
    Event { event: String },
}

/// 适配器返回的错误。分类与 kernel 的 `OutboundError` / `CallError` 对齐。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireError {
    UnknownMethod {
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggestion: Option<String>,
    },
    UnsupportedPlatform {
        detail: String,
    },
    NotImplemented {
        detail: String,
    },
    Forbidden {
        detail: String,
    },
    BadParams {
        detail: String,
    },
    Unavailable {
        detail: String,
    },
    Rejected {
        detail: String,
    },
    Unsupported {
        detail: String,
    },
}

/// core → 适配器。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CoreMessage {
    /// 动态库版本用：返回 `WireResult::Catalog`。外置进程也可以用它，但启动时
    /// 通常已经在 hello 里带了目录。
    Describe {
        id: u64,
    },
    /// 问一个策略 provider（`point` = `wake` / `access` …）。
    Decide {
        id: u64,
        point: String,
        #[serde(default)]
        options: Value,
        #[serde(default)]
        request: Value,
    },
    Send {
        id: u64,
        room: RoomId,
        #[serde(default)]
        elements: Vec<Element>,
    },
    /// 让适配器枚举它当前提供的连接（连接注册协议的 list）。
    /// 适配器用 [`WireResult::Connections`] 回。
    ConnectionsList {
        id: u64,
    },
    Call {
        id: u64,
        room: RoomId,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Shutdown,
}

/// 适配器 → core 的一次动作回执。
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WireResult {
    Catalog {
        #[serde(default)]
        catalog: Vec<WireMethodSpec>,
    },
    /// `decision` 是策略自己的词：wake 用 `wake` / `stay`，access 用 `allow` / `deny`。
    Decision {
        decision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// wake 判定：命中后先等多久再开轮（毫秒）。0 / 不写 = 立即。
        #[serde(default, skip_serializing_if = "is_zero")]
        delay_ms: u64,
        /// wake 判定：这次命中是「攒够一批」而不是「某一条点到我」。
        /// 攒批唤醒投最旧的 `batch_head` 条正文，没有触发消息。
        #[serde(default, skip_serializing_if = "is_false")]
        batch: bool,
    },
    /// 投递批次：本轮的编号与跳过的中段；`plan = null` 表示用内置默认选批。
    Delivery {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<WireDeliveryPlan>,
    },
    /// 失败通知动作列表。空数组表示「什么都不做」。
    Actions {
        #[serde(default)]
        actions: Vec<WireTurnErrorAction>,
    },
    /// 发送前钩子改写后的元素；空的 `elements` 会让 core 拒绝这次发送。
    Elements {
        #[serde(default)]
        elements: Vec<Element>,
    },
    Sent {
        platform_id: String,
        at: i64,
    },
    Call {
        #[serde(default)]
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        record: Option<WireOutgoingRecord>,
    },
    /// 连接枚举的回执（对 [`CoreMessage::ConnectionsList`]）。
    /// 全量集合 = 注册语义；缺了的 id 由 core 按注销处理。
    Connections {
        #[serde(default)]
        connections: Vec<WireConnection>,
    },
    Error {
        error: WireError,
    },
}

/// 适配器在 hello 里声明的入站能力。外置进程自己决定发什么事件、打什么标签。
#[derive(Debug, Clone, Default, PartialEq, DeriveSerialize, Deserialize)]
pub struct WireAdapterInfo {
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub event_tags: Vec<String>,
    #[serde(default)]
    pub supports_at: bool,
    #[serde(default)]
    pub supports_reply: bool,
    /// 单条文本消息的建议上限（字符）；None = 未声明，不等于无限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_limit: Option<usize>,
}

/// 适配器 → core。
///
/// `WireRecord` 天然比 hello/reply 大；这是行协议的解码结果，不做装箱以避免
/// 每条消息多一次分配。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, DeriveSerialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterMessage {
    /// 第一行。声明适配器名字、平台方法目录和入站能力。
    Hello {
        #[serde(default)]
        name: String,
        #[serde(default)]
        catalog: Vec<WireMethodSpec>,
        #[serde(default)]
        info: WireAdapterInfo,
    },
    Event {
        record: WireRecord,
    },
    Reply {
        id: u64,
        result: WireResult,
    },
    Log {
        #[serde(default)]
        level: String,
        message: String,
    },
    /// 连接注册推送：提供者宣布「我现在提供这些连接」。重启后连上即全量回放
    ///（提供者库是唯一事实来源）；运行时库变更可以再推增量（`removed` 点名
    /// 注销的 id）。两个列表都空 = 无操作。
    Connections {
        #[serde(default)]
        connections: Vec<WireConnection>,
        #[serde(default)]
        removed: Vec<String>,
    },
}

/// 把一条消息编码成一行 JSON（不含换行）。
pub fn encode_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

/// 从一行 JSON 里解码。
pub fn decode_line<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

fn default_extension() -> String {
    "platform_adapter".to_owned()
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn default_param_type() -> String {
    "string".to_owned()
}

fn default_target_role() -> String {
    "room".to_owned()
}

fn default_risk() -> String {
    "read".to_owned()
}

fn default_scope() -> String {
    "room".to_owned()
}

fn default_effect() -> String {
    "read".to_owned()
}

fn default_record() -> String {
    "none".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::RecordKind;

    #[test]
    fn core_messages_round_trip() {
        let send = CoreMessage::Send {
            id: 7,
            room: RoomId::new("demo", "group/1").expect("合法"),
            elements: vec![Element::text("你好")],
        };
        let line = encode_line(&send).expect("能编码");
        assert!(line.contains("\"type\":\"send\""));
        let back: CoreMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, send);

        let describe = CoreMessage::Describe { id: 9 };
        let line = encode_line(&describe).expect("能编码");
        assert!(line.contains("\"type\":\"describe\""));
        let back: CoreMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, describe);

        let shutdown: CoreMessage = decode_line(r#"{"type":"shutdown"}"#).expect("能解码");
        assert_eq!(shutdown, CoreMessage::Shutdown);
    }

    #[test]
    fn adapter_messages_round_trip() {
        let hello = AdapterMessage::Hello {
            name: "fixture".into(),
            catalog: vec![WireMethodSpec {
                name: "fixture.echo".into(),
                aliases: Vec::new(),
                summary: "回显".into(),
                tags: vec!["send".into()],
                risk: "read".into(),
                scope: "room".into(),
                params: vec![WireParamSpec {
                    name: "text".into(),
                    ty: "string".into(),
                    required: true,
                    desc: "正文".into(),
                    values: vec![],
                    example: None,
                }],
                targets: vec![],
                effect: "read".into(),
                record: "none".into(),
                permissions: None,
                verification: WireVerification::Verified {
                    source: "fixture docs".into(),
                },
            }],
            info: WireAdapterInfo {
                events: vec!["poke".into()],
                event_tags: vec!["poke".into()],
                supports_at: true,
                supports_reply: true,
                text_limit: Some(4000),
            },
        };
        let line = encode_line(&hello).expect("能编码");
        assert!(line.contains("\"type\":\"hello\""));
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, hello);

        let event = AdapterMessage::Event {
            record: WireRecord {
                room: RoomId::new("demo", "group/1").expect("合法"),
                kind: RecordKind::Message,
                sender: Sender {
                    id: "10001".into(),
                    name: "老王".into(),
                    role: None,
                },
                at: 1,
                elements: vec![Element::text("在吗")],
                at_self: false,
                platform_id: Some("m1".into()),
                reply_to_self: false,
                target_platform_id: None,
                target_labels: Vec::new(),
                suppressed: false,
                room_scope: Some("multi_party".into()),
                room_labels: Vec::new(),
            },
        };
        let line = encode_line(&event).expect("能编码");
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, event);
    }

    #[test]
    fn reply_shape_is_stable() {
        let reply = AdapterMessage::Reply {
            id: 3,
            result: WireResult::Error {
                error: WireError::BadParams {
                    detail: "缺参数".into(),
                },
            },
        };
        let line = encode_line(&reply).expect("能编码");
        assert!(line.contains("\"outcome\":\"error\""), "{line}");
        assert!(line.contains("\"kind\":\"bad_params\""), "{line}");
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, reply);
    }

    fn wire_connection(id: &str) -> WireConnection {
        WireConnection {
            id: id.into(),
            platform: "qq".into(),
            kind: "onebot11".into(),
            transport: "ws-client".into(),
            url: Some("ws://127.0.0.1:3001".into()),
            token: None,
            account: "42".into(),
            admin_qqs: vec!["10001".into()],
            options: serde_json::json!({}),
            token_secret: Some("onebot-token".into()),
            routes: serde_json::json!([]),
        }
    }

    #[test]
    fn connections_push_round_trip() {
        let push = AdapterMessage::Connections {
            connections: vec![wire_connection("qq2")],
            removed: vec!["qq3".into()],
        };
        let line = encode_line(&push).expect("能编码");
        assert!(line.contains("\"type\":\"connections\""), "{line}");
        assert!(line.contains("\"token_secret\":\"onebot-token\""), "{line}");
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, push);
    }

    #[test]
    fn connections_push_defaults_are_empty() {
        // 老实现不认识这两个字段也不该炸：缺省 = 空集合，配合双方都空就是无操作。
        let line = r#"{"type":"connections"}"#;
        let back: AdapterMessage = decode_line(line).expect("能解码");
        assert_eq!(
            back,
            AdapterMessage::Connections {
                connections: Vec::new(),
                removed: Vec::new(),
            }
        );
    }

    #[test]
    fn connections_list_round_trip() {
        let request = CoreMessage::ConnectionsList { id: 9 };
        let line = encode_line(&request).expect("能编码");
        assert!(line.contains("\"type\":\"connections_list\""), "{line}");
        let back: CoreMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, request);

        let reply = AdapterMessage::Reply {
            id: 9,
            result: WireResult::Connections {
                connections: vec![wire_connection("qq2")],
            },
        };
        let line = encode_line(&reply).expect("能编码");
        assert!(line.contains("\"outcome\":\"connections\""), "{line}");
        let back: AdapterMessage = decode_line(&line).expect("能解码");
        assert_eq!(back, reply);
    }

    #[test]
    fn wire_connection_routes_and_secret_ref_round_trip() {
        let line = encode_line(&wire_connection("qq2")).expect("能编码");
        // token 是 None：序列化时整个字段缺席，明文凭据不渗到线上。
        assert!(!line.contains("\"token\":"), "{line}");
        let back: WireConnection = decode_line(&line).expect("能解码");
        assert_eq!(back.token, None);
        assert_eq!(back.token_secret.as_deref(), Some("onebot-token"));
    }
}
