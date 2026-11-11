//! 桥的核心。
//!
//! 这一层只放跨平台、跨 harness 都成立的东西：订阅、唤醒、流水、水位、笔记、回合调度。
//! 平台语义在适配器里，harness 语义在接入点后面。

pub mod adapter;
pub mod config;
pub mod config_objects;
pub mod crypto;
pub mod deps;
pub mod endpoint;
pub mod engine;
pub mod event_queue;
pub mod grants;
pub mod lkg;
pub mod memory;
pub mod notice;
pub mod outbound;
pub mod plugin;
pub mod plugin_action;
pub mod plugin_admin;
pub mod plugin_manifest;
pub mod plugin_meta;
pub mod plugin_registry;
pub mod policy;
pub mod query;
pub mod relay;
pub mod render;
mod route_worker;
pub mod sanitize;
pub mod secrets_db;
pub mod storage;
mod task;
pub mod tickets;
pub mod tools;
pub mod wire;

pub use adapter::{
    ADAPTER_API_VERSION, AdapterCapability, AdapterError, AdapterFactory, AdapterHost,
    AdapterInstance, AdapterManifest, AdapterStatus, carrier_create_failed, create_instance,
    manifest_supports_transport, run_adapter, validate_manifest,
};
pub use config::{
    AccessPolicy, CONSOLE_CONFIG_FILENAME, ClockPolicy, Config, ConfigSource, Connection,
    ConsoleConfig, DEFAULT_MAX_AGE_DAYS, DEFAULT_MAX_RECORDS_PER_ROOM, DEFAULT_STORAGE_PATH,
    DEFAULT_WORKTREES_ROOT, DeliveryPolicy, EndpointConfig, Harness, InBandNoticeConfig,
    KNOWN_MEMORY_KINDS, LkgConfig, LoadedConfig, Memory, NotificationsConfig, OutboundPolicy,
    Persona, PluginConfig, ProviderCallsConfig, QueueConfig, ReceivePolicy, RetentionConfig,
    RoutePolicy, SecretEntry, SecretStore, Session, StorageConfig, Subscription, ToolPluginConfig,
    Transport, TurnErrorAction, TurnErrorActions, TurnErrorPolicy, WakeDecision, WakeInput,
    WakePolicy, WakeVerdict,
};
pub use config_objects::{ConfigObject, ConfigObjectKind, ConfigObjects};
pub use deps::{DependencyKind, DependencyRegistry, PlatformVersionInfo};
pub use endpoint::{Conn, ControlRegistry, EndpointError, handshake, run_connection, serve};
pub use engine::{
    Bridge, BridgeError, Ingest, MAX_DELIVERY, Pruned, RoomFailureStat, TaskRecord, TaskStatus,
};
pub use event_queue::{
    EventQueue, EventReceiver, QueueSnapshot, QueueStats, SubmitError, drain_events,
};
pub use grants::{
    AccessTarget, AccessTargetKind, AccountGrant, BotAdminGrant, GrantCapability, GrantDecision,
    GrantRule, GrantScope, GrantSubject, GrantsConfig, Place, RoomGrant, RoomScope,
    SystemAdminGrant, Trust,
};
pub use lkg::{LkgEntry, LkgInfo, LkgState, PluginLkg};
pub use notice::{NoticeEvent, NoticeSink};
pub use outbound::{
    CallError, CallOutcome, Effect, MethodSpec, Outbound, OutboundError, Outbounds, OutgoingKind,
    OutgoingRecord, ParamSpec, ParamType, PlatformTag, RecordMode, Risk, Scope, Sent, TargetRole,
    TargetSpec, Verification,
};
pub use plugin::{
    AcceptAllReceiveProvider, AccessAction, AccessDecision, AccessProvider, AccessRequest,
    CLOCK_INTERVAL_IDLE_PROVIDER, Capability, ClockContext, ClockOffProvider, ClockProvider,
    DEFAULT_ACCESS_PROVIDER, DEFAULT_CLOCK_PROVIDER, DEFAULT_DELIVERY_PROVIDER,
    DEFAULT_RECEIVE_PROVIDER, DEFAULT_TURN_ERROR_PROVIDER, DefaultDeliveryComposer,
    DefaultTurnErrorProvider, DefaultWakeProvider, DeliveryComposer, DeliveryPlan, DeliveryRequest,
    ExtensionPoint, GRANTS_ACCESS_PROVIDER, Health, InterruptScope, IntervalIdleClockProvider,
    LoaderKind, LoaderManifest, MAX_MANAGER_DEPTH, ManagerHealth, ManagerManifest, ManagerPlugin,
    NativeLoader, PLUGIN_API_VERSION, PluginArtifact, PluginError, PluginLoader, PluginManager,
    PromptContext, PromptDraft, PromptProvider, ProviderDescriptor, ProviderId, ProviderInstance,
    ProviderOptions, ReceiveDecision, ReceiveProvider, ReceiveRequest, RecordObserver, ReloadScope,
    RouteAction, RouteContext, RouteDecision, RouteProvider, SendContext, SendHook, SendOrigin,
    SubscribedAccessProvider, ToolAccessContext, ToolAccessProvider, ToolAccessVerdict,
    ToolCallContext, ToolLoader, ToolProvider, ToolSpec, TurnErrorProvider, TurnErrorRequest,
    WakeContext, WakeProvider, default_delivery_plan,
};
pub use plugin_action::PluginActionRunner;
pub use plugin_manifest::{
    ActionDecl, ConfigField, FieldType, PluginManifest, ResourceDecl, check_config_assignment,
    looks_like_secret_key, looks_like_secret_value,
};
pub use plugin_meta::{PluginEffect, PluginInfo, PluginLifecycle, PluginMetadata, PluginSource};
pub use plugin_registry::{
    CommandKind, CommandStatus, DataClass, Lifecycle, LocationKind, PluginCapabilityDecl,
    PluginKind, PluginPolicyConfig, PluginStorageConfig, RegistryPlugin, ScopeKind, StorageKind,
    check_command, command_fingerprint,
};
pub use policy::{CorePolicy, Policy, PolicyError};
pub use relay::{FileRelay, RelayError};
pub use route_worker::RouteWorker;
pub use storage::{
    Counts, Cursor, Deliveries, FEEDBACK_CAP, Feedback, FeedbackDraft, FeedbackReply, Feedbacks,
    MIN_KEPT, MemoryStore, ModelFallbacks, Note, NoteKey, NoteKind, Notes, Policies, PolicyEffect,
    PrunePlan, RecordDraft, Recorded, SCHEMA_VERSION, SessionMap, SessionMaps, SqliteStore,
    StorageError, Store, StuckUnread, Tasks, TopicStatus, Transcript, Watermarks, unread,
};
pub use task::{TaskHandler, TaskOutcome};
pub use tools::{
    ON_DEMAND_TOOLS, RESIDENT_TOOLS, TOOL_CATALOG, TOOL_NAMES, ToolCatalogEntry, catalog_entry,
    is_on_demand, is_resident,
};

/// 协议类型顺手转出去：下游（接入点、适配器）依赖 kernel 一个 crate 就够。
pub use acb_proto::{
    Delivery, Element, ErrorCode, Frame, Omitted, PersonaInfo, QuoteExcerpt, QuoteUnavailable,
    Record, RecordKind, RoomId, Sender, SenderRole, Ticket, ToolError, TurnFailure, TurnReason,
};
