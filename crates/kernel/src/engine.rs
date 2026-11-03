//! 回合调度：订阅 → 唤醒 → 投递 → 工具 → 确认。
//!
//! 一条入站记录进来，走的是同一条路（ADR-0005）：
//! 1. 写流水——永远发生，不管唤不唤醒；
//! 2. 对每个订阅了这个房间的会话，按它的唤醒策略判定要不要叫醒；
//! 3. 要叫醒就排进队列，**该会话**闲下来就开一轮，把未读打包投给 harness。
//!
//! 串行的单位是**会话**（harness 名 + 会话 id，ADR-0017）：同一会话同一时刻只有一轮，
//! 不同会话各自并发。记忆不再当串行锁——一个会话引用多份记忆、多份记忆可被多个会话
//! 共享，最终一致，互不排队。
//!
//! 一个 harness 连上来时声明「我是谁、接管哪些会话」。`send_message_to` 的可发目标 =
//! 当前会话订阅的所有房间；**跨会话发送被砍掉**——一个会话不能替别的人格发言。

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acb_proto::element::flatten_text;
use acb_proto::{
    Delivery, Element, ErrorCode, MemoryInfo, PersonaInfo, QuoteRef, Record, RecordKind, RoomId,
    Sender, SessionInfo, Ticket, ToolError, TurnReason,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

use crate::config::{ClockPolicy, Config, TurnErrorAction, WakeDecision, WakePolicy};
use crate::memory::{
    MemoryRepo, git_add, git_blame, git_branch, git_commit, git_diff, git_log, git_push,
    git_status, list_dir, read_file, search, write_file,
};
use crate::outbound::{
    CallError, CallOutcome, Effect, MethodSpec, Outbound, OutboundError, OutboundHandle, Outbounds,
    OutgoingKind, OutgoingRecord, RecordMode, Risk, Scope, Sent, TargetRole, expand_query,
};
use crate::plugin::{
    AccessAction, AccessDecision, AccessRequest, ClockContext, DefaultDeliveryComposer,
    DefaultTurnErrorProvider, DeliveryRequest, InterruptScope, PluginError, PluginManager,
    ProviderId, ReceiveDecision, ReceiveRequest, ReloadScope, RouteAction, RouteContext,
    RouteDecision, SendContext, SendHook, SendOrigin, TurnErrorRequest, WakeContext,
};
use crate::plugin_meta::PluginLifecycle;
use crate::render;
use crate::storage::{
    Cursor, MIN_KEPT, Note, NoteKey, NoteKind, PrunePlan, RecordDraft, StorageError, Store,
    TopicStatus, unread,
};
use crate::tools::{
    AckArgs, DEFAULT_COLUMNS, DEFAULT_PAGE_LIMIT, DEFAULT_WAIT_TIMEOUT_MS, DelNoteArgs,
    DescribePluginsArgs, EventsArgs, ExtensionsListArgs, ExtensionsReadArgs, ExtensionsReloadArgs,
    ExtensionsWriteArgs, FeedbackArgs, FetchRecordsArgs, HistoryArgs, MAX_PAGE_LIMIT,
    MAX_WAIT_TIMEOUT_MS, MemoryAddArgs, MemoryArgs, MemoryBlameArgs, MemoryCommitArgs,
    MemoryListArgs, MemoryLogArgs, MemorySearchArgs, MemoryWriteArgs, NotesArgs, PlatformCallArgs,
    PlatformMethodsArgs, RECORD_COLUMNS, SendArgs, SendToArgs, SetNoteArgs, TaskGetArgs,
    TaskListArgs, WaitMessageArgs, is_known,
};

/// 一轮最多投多少条。剩下的下一轮再投，免得一次塞爆上下文。
pub const MAX_DELIVERY: usize = 200;

/// authoring 工具读/写单个扩展文件的硬上限。
const MAX_EXTENSION_FILE_BYTES: u64 = 64 * 1024;

/// 进程内保留的任务记录上限；超出丢最旧的，避免无限增长。
const TASK_KEEP: usize = 200;
/// `task_list` 单页上限。
const TASK_LIST_LIMIT: usize = 100;

fn extension_namespace(namespace: Option<&str>) -> &str {
    namespace
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("dyn")
}

/// 唤醒延迟的上限。策略可以要求等待，但不能把轮次无限期挂起。
const MAX_WAKE_DELAY_MS: u64 = 60_000;

/// 查历史时的默认条数与上限。
const DEFAULT_HISTORY: usize = 20;
const MAX_HISTORY: usize = 200;

/// 轮次结束后票还留多久。留它是为了两件事：补确认（可能慢一步）、写反馈（随时想写）。
const TURN_GRACE: Duration = Duration::from_secs(600);

/// 轮次结束后多久之内还接受 `ack_messages`。
///
/// 只给 60 秒：再晚就说不清「这是补刚才那一轮」还是「你其实在处理新的一轮」，
/// 而新的一轮会正常投递、正常确认，不需要替旧票开太久的口子。
const ACK_GRACE: Duration = Duration::from_secs(60);
/// 轮次收工后仍允许补发一条 `send_message` 的窗口。长工具链跑完、模型才发出最后一条
/// 消息时，桥已经收到 harness 的 `turn_end`；给这段延迟一个有限的补发口，而不是把
/// 已经生成的内容直接丢掉。
const SEND_GRACE: Duration = Duration::from_secs(60);

/// 调度层错误：存储坏了，或者拿了一张不存在的票。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BridgeError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Plugin(#[from] PluginError),
    #[error("{}", acb_i18n::t!("kernel.engine.unknown_ticket", ticket = .0))]
    UnknownTicket(String),
}

/// 一条入站记录的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingest {
    /// 记下来了。`woke` 是被这条叫醒的会话（按配置顺序）。
    Recorded { id: u64, woke: Vec<String> },
    /// 没有任何会话订阅这个房间——什么都不做，连流水都不写。
    NotSubscribed { room: RoomId },
    /// 被连接的入站策略丢掉了——不写流水，也不叫醒任何人。
    Dropped { room: RoomId },
    /// 被路由插件处理了。`stored` 是命令本身有没有写流水（默认不写）；
    /// `woke` 是顺便叫醒的会话。需要 I/O 的动作已经排进 `pending_route_actions`，
    /// 打断请求排在 `pending_interrupts`，由 drain 线程在锁外执行。
    Handled {
        stored: Option<u64>,
        woke: Vec<String>,
    },
}

/// 一次清理里某个房间的战果，给日志用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub room: RoomId,
    /// 删掉多少条。
    pub deleted: u64,
    /// 还剩多少条。
    pub kept: u64,
}

/// 已经校验、等锁外 sleeping 的一次 `wait_message`。
pub(crate) struct PreparedWait {
    ticket: Ticket,
    room: RoomId,
    /// 只返回编号大于它的记录：本轮已投递/已读的最大编号，或水位。
    after: u64,
    limit: usize,
    timeout: Duration,
    columns: Vec<&'static str>,
}

impl PreparedWait {
    pub(crate) fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// 等着开轮次的（会话，房间）。
#[derive(Debug, Clone)]
struct Pending {
    session: String,
    room: RoomId,
    reason: TurnReason,
    wake_reason: Option<String>,
    /// 触发这一轮的发送者。管理员身份按它判定；clock 等无来源的轮次是 None。
    origin_sender: Option<String>,
    /// 最早什么时候允许开轮（Unix 毫秒）。等待期间的新消息照常入库，
    /// 到点 `open_turn` 会一起取走。
    not_before: i64,
}

/// 正在跑的一轮。挂在会话上：一个会话同一时刻只有这一轮。
#[derive(Debug, Clone)]
struct ActiveTurn {
    /// 这一轮替哪个会话说话（决定水位、决定默认发到哪个身份）。
    session: String,
    room: RoomId,
    /// 本轮投出去的编号。只有这些（以及本轮读过的）能被 `ack_messages` 确认。
    delivered: BTreeSet<u64>,
    /// 本轮通过 `get_history` / `fetch_records` / `get_platform_events` 读到的编号。
    /// 读到但没投递的内容也能确认——水位要能被补读推着走。
    read: BTreeSet<u64>,
    /// 开轮时冻结的配置。轮次中途热重载只影响下一轮；这一轮所有工具查询
    /// 都用这份快照，否则「轮次边界生效」只是投递批次的边界，不是真的边界。
    config: Arc<Config>,
    /// 触发这一轮的发送者；authoring 工具用它判定管理员。
    origin_sender: Option<String>,
    /// 管理员按了 `/stop`：这一轮是被主动叫停的，收工时的错误不该再按
    /// `on_turn_error` 发失败通知（那会把「你让我停」报成「我出错了」）。
    stop_requested: bool,
}

/// 刚刚收工的一轮。票不再有效，但留一小会儿：补确认、写反馈都要用到它。
#[derive(Debug, Clone)]
struct EndedTurn {
    session: String,
    room: RoomId,
    delivered: BTreeSet<u64>,
    read: BTreeSet<u64>,
    config: Arc<Config>,
    origin_sender: Option<String>,
    at: Instant,
}

/// 一次工具调用站在哪一轮上。票据既可能有效（`live`），也可能是刚收工的那一轮。
struct TurnCtx {
    session: String,
    room: RoomId,
    delivered: BTreeSet<u64>,
    read: BTreeSet<u64>,
    /// 这一轮开轮时冻结的配置快照。工具期间不读 `self.config`。
    config: Arc<Config>,
    /// 触发这一轮的发送者；没有来源（clock）时为 None。
    origin_sender: Option<String>,
    /// 这一轮还在跑吗。写反馈时记进现场，方便区分「干到一半卡住」和「收工后才想起说」。
    live: bool,
}

/// 已经做完所有校验、只差平台 I/O 的一次 `platform_call`。
///
/// `execute` 故意只拿句柄，不拿 `&mut Bridge`：endpoint 可以先把 core 的全局锁放下，
/// 真正等平台时只锁这条连接自己的出站口。
pub(crate) struct PreparedPlatformCall {
    room: RoomId,
    spec: MethodSpec,
    params: Value,
    args: PlatformCallArgs,
    /// 开轮时冻结的配置快照。结算写流水时要用它查账号、找订阅者。
    config: Arc<Config>,
    outbound: OutboundHandle,
}

impl PreparedPlatformCall {
    pub(crate) fn execute(&self) -> Result<CallOutcome, CallError> {
        self.outbound
            .call(&self.room, self.spec.name.as_ref(), &self.params)
    }
}

/// 已经做完校验、只差出站 I/O 的一次发送。
pub(crate) struct PreparedSend {
    room: RoomId,
    elements: Vec<Element>,
    config: Arc<Config>,
    outbound: OutboundHandle,
    trimmed: bool,
    relayed: Vec<(String, String)>,
    /// 这次发送替哪个会话说话（决定用哪个会话的发送钩子）。
    session: String,
    /// 工具正文还是失败通知。
    origin: SendOrigin,
    /// 发送成功后要跑的观察钩子。
    after_hooks: Vec<Arc<dyn SendHook>>,
    /// 改写正文的发送前钩子（provider id，按执行顺序）。回执里如实告诉 agent
    /// 「你写的正文被谁改过」，不然它以为发出去的是原文。
    modified_by: Vec<String>,
}

impl PreparedSend {
    pub(crate) fn execute(&self) -> Result<Sent, OutboundError> {
        self.outbound.send(&self.room, &self.elements)
    }
}

/// 路由动作要求打断的一个会话；drain 线程据此发 `control: interrupt`。
#[derive(Debug, Clone)]
pub(crate) struct PendingInterrupt {
    pub session: String,
    pub room: RoomId,
}

/// 需要 I/O 的路由动作：发送，或者后台任务。
pub(crate) enum PreparedRouteAction {
    Send(PreparedSend),
    Task(PreparedTask),
}

/// 一个已经确定房间/会话、但还没开始跑的后台任务。
pub(crate) struct PreparedTask {
    pub task_id: String,
    pub room: RoomId,
    pub session: String,
    pub config: Arc<Config>,
    pub handler: String,
    pub params: Value,
}

/// 后台任务状态。任务记录只活在本进程，重启后清空（持久化留给后续阶段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// 一条后台任务记录。给 `task_list` / `task_get` 和任务完成事件用。
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: String,
    pub session: String,
    pub room: RoomId,
    pub handler: String,
    pub params_summary: String,
    pub status: TaskStatus,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub delivery_error: Option<String>,
}

/// `begin_tool_call` 的两种结果：不需要平台 I/O 的已经做完了；需要平台的交给 endpoint
/// 在锁外执行，再回来 `finish`。
pub(crate) enum ToolCallStart {
    Done(Value),
    Platform(PreparedPlatformCall),
    Send(PreparedSend),
    /// 锁外等待新消息，再回锁里取结果。
    Wait(PreparedWait),
}

/// 某个房间的一个订阅者，判定唤醒时要的东西。
#[derive(Debug, Clone)]
struct Subscriber {
    session: String,
    /// 这条连接登录的账号：用来认出「这句是我自己说的」。
    account: String,
    wake: WakePolicy,
}

/// 桥的核心：配置 + 存储 + 出站，加上调度状态。
pub struct Bridge<S: Store> {
    config: Config,
    store: S,
    outbound: Outbounds,
    /// 扩展宿主（ADR-0026）。Phase 1 只承载 wake provider。
    plugins: PluginManager,
    pending: VecDeque<Pending>,
    /// 键是会话 id：一个会话同时只跑一轮。
    active: HashMap<String, ActiveTurn>,
    /// 票 → 会话。票是开轮时签发的，收工作废。
    by_ticket: HashMap<Ticket, String>,
    /// 刚收工的轮次。留 [`TURN_GRACE`]：补确认、写反馈还要用到票上的会话与房间。
    ended: HashMap<Ticket, EndedTurn>,
    ticket_seq: u64,
    /// 本桥实例的票前缀。和 `ticket_seq` 一起组成票面，跨重启不重复。
    ticket_prefix: String,
    /// 排队等着在锁外发出去的失败通知。endpoint 处理完一帧、放下全局锁后执行。
    pending_notices: Vec<PreparedSend>,
    /// 路由动作要求打断的会话；drain 线程取走后发 control 帧。
    pending_interrupts: Vec<PendingInterrupt>,
    /// 路由动作里需要 I/O 的发送/任务；drain 线程交给动作 worker。
    pending_route_actions: Vec<PreparedRouteAction>,
    /// (会话, 房间) → 上次时钟轮时间（Unix 毫秒）。只活在本进程，重启后重新计间隔。
    clock_last: HashMap<(String, RoomId), i64>,
    /// (会话, 房间) → 上次唤醒（任意原因开轮）时间。只活在本进程；
    /// 唤醒策略可以用它实现「多久没醒就醒一次」。
    wake_last: HashMap<(String, RoomId), i64>,
    /// 记忆仓库管理器：负责裸仓库 init 与 worktree checkout。
    memory: MemoryRepo,
    /// 文件中继。配了 `[file_relay]` 才有：发送/平台调用里的本机文件
    /// 换成平台取得到的 URL（ADR-0025）。
    relay: Option<crate::relay::FileRelay>,
    /// 暂存的新配置（管理员在控制台保存后放这，下一次开轮时换上——ADR-0009
    /// 轮次边界重载）。正在跑的轮次不受影响：投递批次在开轮时组装，组装完就不变。
    staged_config: Option<Config>,
    /// 等待在轮次边界执行的动态插件卸载。
    pending_unloads: Vec<ProviderId>,
    /// 等待在轮次边界执行的动态插件重新加载。
    pending_reloads: Vec<ProviderId>,
    /// 轮次边界卸载/重载失败的原因，控制台按 provider 显示。
    plugin_errors: HashMap<ProviderId, String>,
    /// 等待在轮次边界重载的 manager namespace（动态扩展目录用）。
    pending_manager_reloads: Vec<String>,
    /// manager 重载失败的原因，按 namespace 记录。
    manager_errors: HashMap<String, String>,
    /// 后台任务记录，按创建时间从旧到新；超过上限丢最旧的。
    tasks: VecDeque<TaskRecord>,
    /// 任务 id 序号；和票前缀一起保证进程内唯一。
    task_seq: u64,
    /// 配置版本号：每换一次配置 +1，控制台靠它展示「配置换过没有」。
    config_version: u64,
}

/// 票前缀：进程启动时间 + 进程号 + 进程内桥实例序号。
///
/// 以前票面是「会话-N」，序号从 0 数起，重启后又从 0 数起——上一进程的旧票可能正好
/// 撞上本进程的新票，被误认成「这一轮」的票。前缀让每个进程、每个桥实例的票空间不相交。
fn ticket_prefix() -> String {
    static INSTANCE: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let instance = INSTANCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{}-{instance:x}", std::process::id())
}

impl<S: Store> Bridge<S> {
    /// 只用内置 provider 的桥。测试和单适配器调用点用这个。
    pub fn new(config: Config, store: S, outbound: Box<dyn Outbound>) -> Self {
        Self::with_plugins(
            config,
            store,
            outbound,
            PluginManager::with_native_builtins(),
        )
    }

    /// 带一份已经装好 loader / manager 的扩展宿主建桥。
    ///
    /// 生产 main 用这个：树外 manager 的安装是启动顺序的一部分，装完再校验配置
    /// 引用的 provider（未知 provider 在启动时死，不等第一轮）。
    pub fn with_plugins(
        config: Config,
        store: S,
        outbound: Box<dyn Outbound>,
        plugins: PluginManager,
    ) -> Self {
        // 单实现调用点（测试/单适配器）走兜底；生产 main 传进来的本身就是 Outbounds，
        // 再包一层兜底只是多一次转发，不改变按连接分发。
        let outbound = Outbounds::fallback(outbound);
        let memory = MemoryRepo::new(config.worktrees_root.clone());
        let relay = config
            .file_relay
            .as_ref()
            .map(|cfg| crate::relay::FileRelay::new(&cfg.dir, &cfg.public_base));
        Self {
            config,
            store,
            outbound,
            plugins,
            pending: VecDeque::new(),
            active: HashMap::new(),
            by_ticket: HashMap::new(),
            ended: HashMap::new(),
            ticket_seq: 0,
            ticket_prefix: ticket_prefix(),
            pending_notices: Vec::new(),
            pending_interrupts: Vec::new(),
            pending_route_actions: Vec::new(),
            clock_last: HashMap::new(),
            wake_last: HashMap::new(),
            memory,
            relay,
            staged_config: None,
            pending_unloads: Vec::new(),
            pending_reloads: Vec::new(),
            plugin_errors: HashMap::new(),
            pending_manager_reloads: Vec::new(),
            manager_errors: HashMap::new(),
            tasks: VecDeque::new(),
            task_seq: 0,
            config_version: 0,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 启动时校验配置里的 provider 都存在。`main` 在起线程前调用；
    /// 未知 provider 应该在启动时死，而不是等第一轮才报。
    pub fn validate_plugins(&self) -> Result<(), PluginError> {
        self.plugins.validate_config(&self.config)
    }

    /// 插件目录的只读 JSON 视图（控制台用）。
    ///
    /// 在 `PluginInfo::to_json` 之上补齐控制台需要的生命周期字段：
    /// pending 状态、配置引用位置、轮次边界失败原因。
    pub fn plugin_catalog(&self) -> Vec<Value> {
        self.plugins
            .catalog()
            .into_iter()
            .map(|info| {
                let manager_pending = self
                    .pending_manager_reloads
                    .iter()
                    .any(|namespace| namespace == info.id.namespace());
                let pending = if manager_pending
                    || self
                        .pending_reloads
                        .iter()
                        .any(|pending| pending == &info.id)
                {
                    Some("reload")
                } else if self
                    .pending_unloads
                    .iter()
                    .any(|pending| pending == &info.id)
                {
                    Some("unload")
                } else {
                    None
                };
                let state = match pending {
                    Some("unload") => "pending_unload",
                    Some("reload") => "pending_reload",
                    _ => info.lifecycle.as_str(),
                };
                let refs = self.plugin_references(&info.id);
                let error = self
                    .plugin_errors
                    .get(&info.id)
                    .or_else(|| self.manager_errors.get(info.id.namespace()));
                let mut value = info.to_json();
                if let Value::Object(fields) = &mut value {
                    fields.insert("state".into(), json!(state));
                    fields.insert("pending".into(), json!(pending));
                    fields.insert("refs".into(), json!(refs));
                    fields.insert(
                        "lastError".into(),
                        error.map(|error| json!(error)).unwrap_or(Value::Null),
                    );
                }
                value
            })
            .collect()
    }

    /// 一个 provider 被当前生效配置引用的位置。引用中的插件不能卸载，
    /// 否则下一轮解析配置会失败。返回形如 `session:main wake qq:group/123` 的稳定标识。
    fn plugin_references(&self, id: &ProviderId) -> Vec<String> {
        let mut refs = Vec::new();
        for connection in &self.config.connections {
            if &connection.receive.provider == id {
                refs.push(format!("connection:{} receive", connection.id));
            }
            for route in &connection.routes {
                if &route.provider == id {
                    refs.push(format!("connection:{} route", connection.id));
                }
            }
        }
        for session in &self.config.sessions {
            if &session.access.provider == id {
                refs.push(format!("session:{} access", session.id));
            }
            if &session.delivery.provider == id {
                refs.push(format!("session:{} delivery", session.id));
            }
            if session.outbound.before.iter().any(|hook| hook == id)
                || session.outbound.after.iter().any(|hook| hook == id)
            {
                refs.push(format!("session:{} outbound", session.id));
            }
            if &session.turn_error.provider == id {
                refs.push(format!("session:{} turn_error", session.id));
            }
            for subscription in &session.subscriptions {
                if &subscription.wake.provider == id {
                    refs.push(format!("session:{} wake {}", session.id, subscription.room));
                }
                if &subscription.clock.provider == id {
                    refs.push(format!(
                        "session:{} clock {}",
                        session.id, subscription.room
                    ));
                }
            }
        }
        refs.sort();
        refs
    }

    fn plugin_lifecycle(&self, id: &ProviderId) -> Result<PluginLifecycle, BridgeError> {
        match self.plugins.info(id) {
            Some(info) => Ok(info.lifecycle),
            None => Err(BridgeError::Plugin(PluginError::Provider {
                id: id.to_string(),
                detail: acb_i18n::t!("kernel.engine.unknown_plugin", id = id),
            })),
        }
    }

    fn plugin_not_hot(&self, id: &ProviderId, lifecycle: PluginLifecycle) -> BridgeError {
        BridgeError::Plugin(PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!(
                "kernel.engine.plugin_not_unloadable",
                id = id,
                lifecycle = lifecycle.as_str()
            ),
        })
    }

    fn plugin_pending(&self, id: &ProviderId, action: &str) -> BridgeError {
        BridgeError::Plugin(PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!("kernel.engine.plugin_pending", id = id, action = action),
        })
    }

    fn reject_if_referenced(&self, id: &ProviderId) -> Result<(), BridgeError> {
        let refs = self.plugin_references(id);
        if refs.is_empty() {
            return Ok(());
        }
        Err(BridgeError::Plugin(PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!(
                "kernel.engine.plugin_in_use",
                id = id,
                refs = refs.join(", ")
            ),
        }))
    }

    /// 请求在下一个轮次边界卸载一个动态插件。不会在轮次进行中直接 dlclose。
    pub fn queue_unload(&mut self, id: &str) -> Result<(), BridgeError> {
        let id = ProviderId::parse(id)?;
        let lifecycle = self.plugin_lifecycle(&id)?;
        if lifecycle != PluginLifecycle::Unloadable {
            return Err(self.plugin_not_hot(&id, lifecycle));
        }
        self.reject_if_referenced(&id)?;
        if self.pending_reloads.iter().any(|pending| pending == &id) {
            return Err(self.plugin_pending(&id, "reload"));
        }
        if !self.pending_unloads.iter().any(|pending| pending == &id) {
            self.pending_unloads.push(id);
        }
        Ok(())
    }

    /// 请求在下一个轮次边界重新加载一个动态插件。
    ///
    /// 已卸载的直接排重载；已加载且可卸载的同时排卸载和重载，净效果是一次安全点
    /// 内的「卸载 → 重新加载」。
    pub fn queue_reload(&mut self, id: &str) -> Result<(), BridgeError> {
        let id = ProviderId::parse(id)?;
        let lifecycle = self.plugin_lifecycle(&id)?;
        match lifecycle {
            PluginLifecycle::Resident => return Err(self.plugin_not_hot(&id, lifecycle)),
            // 已加载时重载包含卸载，仍被配置引用会中断下一轮，必须拒绝。
            PluginLifecycle::Unloadable => self.reject_if_referenced(&id)?,
            // 已卸载时重载是在恢复服务；即使配置又引用它，也应该允许。
            PluginLifecycle::Unloaded => {}
        }
        if !self.pending_reloads.iter().any(|pending| pending == &id) {
            self.pending_reloads.push(id.clone());
        }
        if lifecycle == PluginLifecycle::Unloadable
            && !self.pending_unloads.iter().any(|pending| pending == &id)
        {
            self.pending_unloads.push(id);
        }
        Ok(())
    }

    /// 请求在下一个轮次边界重载一棵 manager 子树（动态扩展目录用）。
    ///
    /// 与 provider 级 unload/reload 共用同一个安全点；重载失败只记录错误，
    /// 不替换线上模块快照（由 manager 自己保证）。
    pub fn queue_extension_reload(&mut self, namespace: &str) -> Result<(), BridgeError> {
        if !self.plugins.has_manager(namespace) {
            return Err(BridgeError::Plugin(PluginError::Manager {
                namespace: namespace.to_owned(),
                detail: acb_i18n::t!("extensions.manager_missing", namespace = namespace),
            }));
        }
        if !self
            .pending_manager_reloads
            .iter()
            .any(|pending| pending == namespace)
        {
            self.pending_manager_reloads.push(namespace.to_owned());
        }
        Ok(())
    }

    /// 动态扩展目录的只读视图：pending 状态、最近错误、模块级信息。
    pub fn extension_catalog(&self, namespace: &str) -> Value {
        let pending = self
            .pending_manager_reloads
            .iter()
            .any(|item| item == namespace);
        let error = self.manager_errors.get(namespace);
        json!({
            "namespace": namespace,
            "installed": self.plugins.has_manager(namespace),
            "pendingReload": pending,
            "lastError": error,
            "details": self.plugins.manager_details(namespace),
        })
    }

    fn create_task_record(
        &mut self,
        room: &RoomId,
        session: &str,
        handler: &str,
        params: &Value,
    ) -> String {
        self.task_seq = self.task_seq.saturating_add(1);
        let id = format!("task-{}-{}", self.ticket_prefix, self.task_seq);
        self.tasks.push_back(TaskRecord {
            id: id.clone(),
            session: session.to_owned(),
            room: room.clone(),
            handler: handler.to_owned(),
            params_summary: summarize_params(params),
            status: TaskStatus::Queued,
            created_at: now_millis(),
            started_at: None,
            finished_at: None,
            result: None,
            error: None,
            delivery_error: None,
        });
        while self.tasks.len() > TASK_KEEP {
            self.tasks.pop_front();
        }
        id
    }

    pub(crate) fn mark_task_running(&mut self, task_id: &str) {
        if let Some(record) = self.tasks.iter_mut().find(|record| record.id == task_id) {
            record.status = TaskStatus::Running;
            record.started_at = Some(now_millis());
        }
    }

    pub(crate) fn finish_task(
        &mut self,
        task_id: &str,
        result: Option<Value>,
        error: Option<String>,
        delivery_error: Option<String>,
    ) {
        if let Some(record) = self.tasks.iter_mut().find(|record| record.id == task_id) {
            record.status = if error.is_some() || delivery_error.is_some() {
                TaskStatus::Failed
            } else {
                TaskStatus::Succeeded
            };
            record.finished_at = Some(now_millis());
            record.result = result;
            record.error = error;
            record.delivery_error = delivery_error;
        }
    }

    /// 列当前会话的任务记录（最新在前）。任务可能在轮次结束后才完成，
    /// 所以这个查询不依赖任务触发时的票是否还活着。
    pub fn task_list(
        &self,
        session: &str,
        status: Option<&str>,
        limit: usize,
        offset: usize,
    ) -> Result<Value, ToolError> {
        let wanted = match status.map(str::trim).filter(|value| !value.is_empty()) {
            Some(text) => Some(TaskStatus::parse(text).ok_or_else(|| {
                ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("kernel.engine.task_bad_status", status = text),
                )
            })?),
            None => None,
        };
        let records: Vec<&TaskRecord> = self
            .tasks
            .iter()
            .rev()
            .filter(|record| record.session == session)
            .filter(|record| wanted.is_none_or(|status| record.status == status))
            .collect();
        let total = records.len();
        let limit = limit.clamp(1, TASK_LIST_LIMIT);
        let tasks: Vec<Value> = records
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(task_json)
            .collect();
        Ok(json!({
            "session": session,
            "total": total,
            "offset": offset,
            "returned": tasks.len(),
            "has_more": offset + tasks.len() < total,
            "tasks": tasks,
        }))
    }

    /// 按 id 查一条任务记录；只允许查询本会话的任务。
    pub fn task_get(&self, session: &str, task_id: &str) -> Result<Value, ToolError> {
        let record = self
            .tasks
            .iter()
            .rev()
            .find(|record| record.id == task_id && record.session == session)
            .ok_or_else(|| {
                ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("kernel.engine.task_not_found", id = task_id),
                )
            })?;
        Ok(task_json(record))
    }

    fn require_extension_admin(
        &self,
        config: &Config,
        room: &RoomId,
        origin_sender: Option<&str>,
    ) -> Result<(), ToolError> {
        let admin = origin_sender
            .map(|sender| config.is_admin(room, sender))
            .unwrap_or(false);
        if !admin {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                acb_i18n::t!("extensions.tool.admin_only"),
            ));
        }
        Ok(())
    }

    fn extension_root(&self, namespace: &str) -> Result<PathBuf, ToolError> {
        let root = self.plugins.manager_root(namespace).ok_or_else(|| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("extensions.manager_missing", namespace = namespace),
            )
        })?;
        fs::canonicalize(&root).map_err(|error| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "extensions.root.read_failed",
                    path = root.display(),
                    error = error
                ),
            )
        })
    }

    fn read_extension(&self, namespace: &str, path: &str) -> Result<Value, ToolError> {
        let target = self.extension_path(namespace, path, false)?;
        let metadata = fs::metadata(&target).map_err(|error| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "extensions.tool.read_failed",
                    path = target.display(),
                    error = error
                ),
            )
        })?;
        if metadata.len() > MAX_EXTENSION_FILE_BYTES {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "extensions.tool.too_large",
                    path = target.display(),
                    size = metadata.len(),
                    max = MAX_EXTENSION_FILE_BYTES
                ),
            ));
        }
        let text = fs::read_to_string(&target).map_err(|error| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "extensions.tool.read_failed",
                    path = target.display(),
                    error = error
                ),
            )
        })?;
        Ok(json!({
            "namespace": namespace,
            "path": path,
            "size": metadata.len(),
            "text": text,
        }))
    }

    fn write_extension(&self, namespace: &str, path: &str, text: &str) -> Result<Value, ToolError> {
        if text.len() as u64 > MAX_EXTENSION_FILE_BYTES {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "extensions.tool.too_large",
                    path = path,
                    size = text.len(),
                    max = MAX_EXTENSION_FILE_BYTES
                ),
            ));
        }
        let target = self.extension_path(namespace, path, true)?;
        let parent = target.parent().ok_or_else(|| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("extensions.tool.path_invalid", path = path),
            )
        })?;
        let file_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("manifest.toml");
        let temp = parent.join(format!(
            ".{file_name}.tmp-{}-{}",
            std::process::id(),
            now_millis()
        ));
        if let Err(error) = fs::write(&temp, text) {
            let _ = fs::remove_file(&temp);
            return Err(ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "extensions.tool.write_failed",
                    path = target.display(),
                    error = error
                ),
            ));
        }
        if let Err(error) = fs::rename(&temp, &target) {
            let _ = fs::remove_file(&temp);
            return Err(ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "extensions.tool.write_failed",
                    path = target.display(),
                    error = error
                ),
            ));
        }
        Ok(json!({
            "namespace": namespace,
            "path": path,
            "bytes": text.len(),
            "written": true,
            "note": acb_i18n::t!("extensions.tool.write_note"),
        }))
    }

    fn extension_path(
        &self,
        namespace: &str,
        path: &str,
        for_write: bool,
    ) -> Result<PathBuf, ToolError> {
        let path = path.trim();
        if path.is_empty() || Path::new(path).is_absolute() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("extensions.tool.path_invalid", path = path),
            ));
        }
        if !path.to_ascii_lowercase().ends_with(".toml") {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("extensions.tool.not_toml", path = path),
            ));
        }
        let components: Vec<Component<'_>> = Path::new(path).components().collect();
        if components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("extensions.tool.path_invalid", path = path),
            ));
        }
        let root = self.extension_root(namespace)?;
        let target = root.join(path);
        let mut current = root.clone();
        for (index, component) in components.iter().enumerate() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        return Err(ToolError::new(
                            ErrorCode::BadRequest,
                            acb_i18n::t!("extensions.tool.symlink", path = current.display()),
                        ));
                    }
                    if index + 1 < components.len() && !metadata.is_dir() {
                        return Err(ToolError::new(
                            ErrorCode::BadRequest,
                            acb_i18n::t!("extensions.tool.not_file", path = current.display()),
                        ));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!(
                            "extensions.tool.read_failed",
                            path = current.display(),
                            error = error
                        ),
                    ));
                }
            }
        }
        if for_write {
            let parent = target.parent().ok_or_else(|| {
                ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("extensions.tool.path_invalid", path = path),
                )
            })?;
            fs::create_dir_all(parent).map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!(
                        "extensions.tool.write_failed",
                        path = parent.display(),
                        error = error
                    ),
                )
            })?;
            let parent = fs::canonicalize(parent).map_err(|error| {
                ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!(
                        "extensions.tool.read_failed",
                        path = parent.display(),
                        error = error
                    ),
                )
            })?;
            if !parent.starts_with(&root) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("extensions.tool.path_escape", path = parent.display()),
                ));
            }
            if let Ok(metadata) = fs::symlink_metadata(&target) {
                if metadata.file_type().is_symlink() {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!("extensions.tool.symlink", path = target.display()),
                    ));
                }
                if !metadata.is_file() {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!("extensions.tool.not_file", path = target.display()),
                    ));
                }
            }
            Ok(target)
        } else {
            let metadata = fs::symlink_metadata(&target).map_err(|error| {
                ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!(
                        "extensions.tool.read_failed",
                        path = target.display(),
                        error = error
                    ),
                )
            })?;
            if metadata.file_type().is_symlink() {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("extensions.tool.symlink", path = target.display()),
                ));
            }
            if !metadata.is_file() {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("extensions.tool.not_file", path = target.display()),
                ));
            }
            let canonical = fs::canonicalize(&target).map_err(|error| {
                ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!(
                        "extensions.tool.read_failed",
                        path = target.display(),
                        error = error
                    ),
                )
            })?;
            if !canonical.starts_with(&root) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("extensions.tool.path_escape", path = canonical.display()),
                ));
            }
            Ok(canonical)
        }
    }

    fn apply_pending_plugin_actions(&mut self) {
        if !self.active.is_empty() {
            return;
        }
        if self.pending_unloads.is_empty()
            && self.pending_reloads.is_empty()
            && self.pending_manager_reloads.is_empty()
        {
            return;
        }
        let unloads = std::mem::take(&mut self.pending_unloads);
        let reloads = std::mem::take(&mut self.pending_reloads);
        let mut unload_failed = Vec::new();
        for id in unloads {
            match self.plugins.unload_provider(&id) {
                Ok(()) => {
                    self.plugin_errors.remove(&id);
                    eprintln!("{}", acb_i18n::t!("kernel.engine.unload_applied", id = id));
                }
                Err(error) => {
                    unload_failed.push(id.clone());
                    self.plugin_errors.insert(id.clone(), error.to_string());
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.engine.unload_failed", id = id, error = error)
                    );
                }
            }
        }
        for id in reloads {
            // 卸载失败说明旧版本还在；重载必然报「已加载」，不要把错误覆盖成更难懂的提示。
            if unload_failed.iter().any(|failed| failed == &id) {
                continue;
            }
            match self.plugins.reload_provider(&id) {
                Ok(()) => {
                    self.plugin_errors.remove(&id);
                    eprintln!("{}", acb_i18n::t!("kernel.engine.reload_applied", id = id));
                }
                Err(error) => {
                    self.plugin_errors.insert(id.clone(), error.to_string());
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.engine.reload_failed", id = id, error = error)
                    );
                }
            }
        }

        // manager 级重载（动态扩展目录）：和 provider 动作同一个安全点。
        let managers = std::mem::take(&mut self.pending_manager_reloads);
        for namespace in managers {
            let failures = self
                .plugins
                .reload_managers(&ReloadScope::Namespace(namespace.clone()));
            if failures.is_empty() {
                self.manager_errors.remove(&namespace);
                eprintln!("extensions: reloaded namespace {namespace}");
            } else {
                let detail = failures
                    .iter()
                    .map(|(name, error)| format!("{name}: {error}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                self.manager_errors
                    .insert(namespace.clone(), detail.clone());
                eprintln!("extensions: reload rejected for namespace {namespace}: {detail}");
            }
        }
    }

    /// 校验一份**还没生效**的配置（控制台保存时用）。暂存前先过这一关，
    /// 避免未知 provider 被写进磁盘、等到下一轮才炸。
    pub fn validate_plugins_for(&self, config: &Config) -> Result<(), PluginError> {
        self.plugins.validate_config(config)
    }

    /// 暂存一份新配置。**不立即生效**：下一次开轮（[`Self::poll_turn`]）时换上
    /// （ADR-0009 轮次边界重载）。
    pub fn stage_config(&mut self, config: Config) {
        self.staged_config = Some(config);
    }

    /// 有没有暂存着的配置还没生效。
    pub fn has_staged_config(&self) -> bool {
        self.staged_config.is_some()
    }

    /// 丢弃暂存的配置（管理员存错了想撤回）。返回之前有没有暂存。
    pub fn discard_staged_config(&mut self) -> bool {
        self.staged_config.take().is_some()
    }

    /// 配置版本号：每换一次 +1。
    pub fn config_version(&self) -> u64 {
        self.config_version
    }

    /// 轮次边界：把暂存的配置换上来。开轮前必调。
    ///
    /// 顺带重建从配置派生的两样东西：记忆仓库（worktrees_root 可能改了）、
    /// 文件中继发布端（public_base/dir 可能改了）。
    fn apply_staged_config(&mut self) {
        let Some(config) = self.staged_config.take() else {
            return;
        };
        self.memory = MemoryRepo::new(config.worktrees_root.clone());
        self.relay = config
            .file_relay
            .as_ref()
            .map(|cfg| crate::relay::FileRelay::new(&cfg.dir, &cfg.public_base));
        self.config = config;
        self.config_version += 1;
    }

    /// 只读看存储，给管理页和测试用。
    pub fn store(&self) -> &S {
        &self.store
    }

    /// 可变地看存储。只给控制台用（改反馈状态），调度自己不要借这个口绕过流水。
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    /// 记录会话映射：acb 会话 id → harness 内部会话 id（harness 回报时调）。
    pub fn upsert_session_map(
        &mut self,
        session: String,
        harness_session: String,
        meta: Value,
    ) -> Result<(), BridgeError> {
        self.store
            .upsert_session_map(crate::storage::SessionMap {
                session,
                harness_session,
                meta,
                updated_at: now_millis(),
            })
            .map_err(BridgeError::Storage)
    }

    /// 删会话映射（重置时用）。
    pub fn delete_session_map(&mut self, session: &str) -> Result<bool, BridgeError> {
        self.store
            .delete_session_map(session)
            .map_err(BridgeError::Storage)
    }

    /// 全部会话映射，给控制台展示用。
    pub fn session_maps(&self) -> Result<Vec<crate::storage::SessionMap>, BridgeError> {
        self.store.session_maps().map_err(BridgeError::Storage)
    }

    /// 哪几个会话正跑着轮次。
    pub fn busy_sessions(&self) -> Vec<String> {
        let mut sessions: Vec<String> = self.active.keys().cloned().collect();
        sessions.sort();
        sessions
    }

    /// 队列里还压着几个待开轮次（给日志看，没人接的时候提醒用）。
    pub fn queued_turns(&self) -> usize {
        self.pending.len()
    }

    /// 清一次旧流水，返回真的删了东西的房间。
    ///
    /// `now` 是「现在」的 Unix 毫秒——传进来而不是自己取，是为了让测试能造出「很久以前」。
    ///
    /// 三条规矩：
    /// 1. **只删读过的**：安全线取所有在册订阅者水位的最小值。有一个设备一条都没读到过，
    ///    这个房间这次就一条都不删——未读是投递的原料，删了就是真丢了。
    /// 2. **只删还在订阅的房间**：没人订的房间不归这里管，判据（谁在读）也不成立了。
    /// 3. **每房间至少留一条**：编号靠 `MAX+1` 推，删光了下一个又回到 1，而水位还在几百，
    ///    新消息会被当成已读直接消失（[`MIN_KEPT`]）。
    pub fn prune(&mut self, now: i64) -> Result<Vec<Pruned>, BridgeError> {
        let retention = self.config.retention;
        if !retention.enabled() {
            return Ok(Vec::new());
        }

        let older_than = (retention.max_age_days > 0).then(|| {
            now.saturating_sub((retention.max_age_days as i64).saturating_mul(86_400_000))
        });

        let mut report = Vec::new();
        for room in self.store.rooms()? {
            let sessions: Vec<String> = self
                .config
                .subscribers_of(&room)
                .into_iter()
                .map(|session| session.id.clone())
                .collect();
            if sessions.is_empty() {
                continue;
            }

            // 安全线：所有在册会话都已读到的那个位置。初值是「一条都没删」的边界。
            let mut below = self.store.latest(&room)?.map_or(0, |latest| latest + 1);
            for session in sessions {
                let cursor = Cursor::new(session, room.clone());
                below = below.min(self.store.watermark(&cursor)?);
            }
            if below == 0 {
                continue;
            }

            let before = self.store.count(&room)?;
            let deleted = self.store.prune(
                &room,
                &PrunePlan {
                    below,
                    keep_newest: retention.max_records_per_room.max(MIN_KEPT),
                    older_than,
                },
            )?;
            if deleted > 0 {
                report.push(Pruned {
                    room: room.clone(),
                    deleted,
                    kept: before - deleted,
                });
            }
        }
        Ok(report)
    }

    /// 一条入站记录进来。
    pub fn ingest(&mut self, draft: RecordDraft) -> Result<Ingest, BridgeError> {
        // 收消息也是配置消费点（唤醒判定走订阅），暂存的配置在这里也要换上——
        // 否则刚保存的新订阅会漏掉紧接着来的第一条消息。开轮（poll）处同样应用：
        // 保证投递批次组装时用的配置和开轮后工具查询看到的一致。
        self.apply_staged_config();
        let room = draft.room.clone();
        let subscribers = Self::subscribers_of(&self.config, &room);

        if subscribers.is_empty() {
            return Ok(Ingest::NotSubscribed { room });
        }

        // 入站记录先给路由插件看：命令、链接、任务可以在入库前被代码接管。
        // 路由只看 Text 段，避免把「@机器人 + /stop」里的 @ 拼进命令。
        let text = flatten_text(&draft.elements);
        let command = command_text(&draft.elements);
        let platform = self
            .config
            .connection(&room.connection)
            .map(|connection| connection.platform.clone())
            .unwrap_or_else(|| "unknown".to_owned());
        let account = self.config.account_of_room(&room);
        let reply_to_self = self.quotes_own_message(&draft, &account)?;
        // 平台把自己说的话回传回来：可以记流水，但绝不能再触发路由命令/链接，
        // 否则 bot 自己发的 /send 会被再执行一次。
        let from_self = !account.is_empty() && draft.sender.id == account;
        let mut draft = draft.with_reply_to_self(reply_to_self);
        let sessions: Vec<String> = subscribers
            .iter()
            .map(|subscriber| subscriber.session.clone())
            .collect();
        // 先拷成 owned 再借：后面要把 draft 移进 store.append，event/tags 还要在
        // 唤醒判定里继续用。
        let event = match &draft.kind {
            RecordKind::Event { event, .. } => Some(event.clone()),
            RecordKind::Message => None,
        };
        let tag_names: Vec<String> = match &draft.kind {
            RecordKind::Event { tags, .. } => tags.clone(),
            RecordKind::Message => Vec::new(),
        };
        let event_ref = event.as_deref();
        let tags_ref: Vec<&str> = tag_names.iter().map(String::as_str).collect();

        let route_policies = if from_self {
            Vec::new()
        } else {
            self.config
                .connection(&room.connection)
                .map(|connection| connection.routes.clone())
                .unwrap_or_default()
        };
        let mut handled = None;
        for policy in &route_policies {
            let provider = match self.plugins.resolve_route(&policy.provider, policy) {
                Ok(provider) => provider,
                Err(error) => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "kernel.engine.route_provider_resolve",
                            provider = policy.provider,
                            error = error
                        )
                    );
                    continue;
                }
            };
            let context = RouteContext {
                connection: &room.connection,
                platform: &platform,
                room: &room,
                sender: &draft.sender,
                is_admin: self.config.is_admin(&room, &draft.sender.id),
                kind: &draft.kind,
                elements: &draft.elements,
                text: &command,
                at_self: draft.at_self,
                reply_to_self: draft.reply_to_self,
                has_body: draft.has_body(),
                event: event_ref,
                tags: &tags_ref,
                sessions: &sessions,
            };
            match provider.decide(policy, &context) {
                RouteDecision::Pass => continue,
                decision => {
                    handled = Some(decision);
                    break;
                }
            }
        }

        let route_handled = handled.is_some();
        let mut suppressed = draft.suppressed;
        if let Some(decision) = handled {
            match decision {
                RouteDecision::Pass => unreachable!("pass must not be placed in handled"),
                RouteDecision::Drop { reason } => {
                    if let Some(reason) = reason {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "kernel.engine.route_dropped",
                                room = room,
                                reason = reason
                            )
                        );
                    }
                    return Ok(Ingest::Dropped { room });
                }
                RouteDecision::Handle {
                    store,
                    wake,
                    actions,
                } => {
                    for action in actions {
                        self.apply_route_action(action, &room, &sessions)?;
                    }
                    // 要叫醒 AI 就必须有记录可投；`wake` 隐含 `store`。
                    let store = store || wake;
                    if !store {
                        return Ok(Ingest::Handled {
                            stored: None,
                            woke: Vec::new(),
                        });
                    }
                    suppressed = suppressed || !wake;
                }
            }
        } else {
            // 入站策略：先问这条连接的 provider 收不收。被丢掉的记录不写流水、不叫醒，
            // 就像它从没到过桥——水位也不会因为它出现空洞。
            let receive_policy = self
                .config
                .connection(&room.connection)
                .map(|connection| connection.receive.clone())
                .unwrap_or_default();
            let receive_provider = self
                .plugins
                .resolve_receive(&receive_policy.provider, &receive_policy)?;
            let receive_context = ReceiveRequest {
                connection: &room.connection,
                platform: &platform,
                room: &room,
                sender: &draft.sender,
                kind: &draft.kind,
                elements: &draft.elements,
                text: &text,
                at_self: draft.at_self,
            };
            if receive_provider.decide(&receive_policy, &receive_context) == ReceiveDecision::Drop {
                return Ok(Ingest::Dropped { room });
            }
        }

        // 先把这一轮要用的 provider 都解析出来。放 append 之前是有意的：provider id 写错
        // 应该在入库前就报错，不能先写一条流水再发现叫不醒。
        let mut wake_providers = Vec::with_capacity(subscribers.len());
        for subscriber in &subscribers {
            wake_providers.push(
                self.plugins
                    .resolve_wake(&subscriber.wake.provider, &subscriber.wake)?,
            );
        }

        // 路由决定「只存不叫醒」时，要真的把 suppressed 落进记录：
        // 否则下一条消息叫醒时，这条命令会被当成未读一起投出去。
        draft.suppressed = suppressed;

        // 适配器可以声明「这条事件只进流水」：不投递、不唤醒、不计未读。
        // 也可以让它指向某条旧记录并追加标签（例如撤回标记）。core 只执行这两个
        // 通用动作，不解释标签或事件名的平台语义。
        let target = draft.target_platform_id.clone();
        let target_labels = draft.target_labels.clone();
        let record = self.store.append(draft)?;
        if let Some(target) = target {
            if !target_labels.is_empty() {
                self.store.add_labels(&room, &target, &target_labels)?;
            }
        }

        let mut woke = Vec::new();
        let now = now_millis();
        for (subscriber, provider) in subscribers.into_iter().zip(wake_providers) {
            let cursor = Cursor::new(subscriber.session.clone(), room.clone());

            // 只进流水的事件不参与唤醒；未读和确认路径会在存储层跳过它，
            // 不需要在这里按订阅者补 ack——那样语义就绑在「入库时谁在订阅」上了。
            if suppressed {
                continue;
            }

            // 平台把我们自己说的话回传回来：记下来，但不是未读，也不叫醒自己。
            if record.sender.id == subscriber.account {
                self.store.ack(&cursor, &[record.id])?;
                continue;
            }

            let pending = self
                .pending_records(&subscriber.session, &room, &subscriber.account)?
                .len() as u32;
            let last_wake_at = self
                .wake_last
                .get(&(subscriber.session.clone(), room.clone()))
                .copied()
                .unwrap_or(now);
            let context = WakeContext {
                session: &subscriber.session,
                room: &room,
                connection: &room.connection,
                platform: &platform,
                sender_id: &record.sender.id,
                text: &text,
                at_self: record.at_self,
                reply_to_self: record.reply_to_self,
                has_body: record.has_body(),
                event: event_ref,
                tags: &tags_ref,
                unread: pending,
                last_wake_at,
                now,
            };
            let verdict = provider.decide_with_reason(&subscriber.wake, &context);
            if verdict.decision == WakeDecision::Wake {
                woke.push(subscriber.session.clone());
                self.enqueue(
                    subscriber.session,
                    room.clone(),
                    TurnReason::Wake,
                    verdict.reason,
                    Some(record.sender.id.clone()),
                    now,
                    verdict.delay_ms,
                );
            }
        }

        if route_handled {
            Ok(Ingest::Handled {
                stored: Some(record.id),
                woke,
            })
        } else {
            Ok(Ingest::Recorded {
                id: record.id,
                woke,
            })
        }
    }

    /// 执行一个路由动作。打断和状态清理在这里立即做；发送/任务只做准备，
    /// 真正的 I/O 交给 drain/动作 worker 在锁外跑。
    fn apply_route_action(
        &mut self,
        action: RouteAction,
        room: &RoomId,
        sessions: &[String],
    ) -> Result<(), BridgeError> {
        match action {
            RouteAction::Interrupt { scope } => {
                self.interrupt(scope, room);
                Ok(())
            }
            RouteAction::Send {
                room: target,
                elements,
            } => {
                let target = target.unwrap_or_else(|| room.clone());
                let session = self
                    .config
                    .subscribers_of(&target)
                    .first()
                    .map(|session| session.id.clone());
                let Some(session) = session else {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.engine.route_send_no_subscribers", target = target)
                    );
                    return Ok(());
                };
                let config = Arc::new(self.config.clone());
                match self.prepare_send(config, &session, target, SendOrigin::Route, elements) {
                    Ok(prepared) => self
                        .pending_route_actions
                        .push(PreparedRouteAction::Send(prepared)),
                    Err(error) => eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "kernel.engine.route_send_assemble_failed",
                            error = format!("{error:?}")
                        )
                    ),
                }
                Ok(())
            }
            RouteAction::Task { handler, params } => {
                let Some(session) = sessions.first().cloned() else {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.engine.route_task_no_session", handler = handler)
                    );
                    return Ok(());
                };
                let task_id = self.create_task_record(&room, &session, &handler, &params);
                self.pending_route_actions
                    .push(PreparedRouteAction::Task(PreparedTask {
                        task_id,
                        room: room.clone(),
                        session,
                        config: Arc::new(self.config.clone()),
                        handler,
                        params,
                    }));
                Ok(())
            }
        }
    }

    /// 提前结束轮次。只清 `pending`，不动未读水位：被停掉的消息还在流水里，
    /// 之后任何一个新的唤醒（下一条消息、攒够未读、时钟）都会照常把它们带出来。
    fn interrupt(&mut self, scope: InterruptScope, room: &RoomId) {
        let mut sessions = Vec::new();
        match &scope {
            InterruptScope::Room => {
                for (session, turn) in &self.active {
                    if &turn.room == room {
                        sessions.push(session.clone());
                    }
                }
                self.pending.retain(|pending| &pending.room != room);
            }
            InterruptScope::Sessions(list) => {
                for session in list {
                    if self.active.contains_key(session) {
                        sessions.push(session.clone());
                    }
                }
                self.pending
                    .retain(|pending| !list.iter().any(|session| session == &pending.session));
            }
            InterruptScope::Connection => {
                for (session, turn) in &self.active {
                    if turn.room.connection == room.connection {
                        sessions.push(session.clone());
                    }
                }
                self.pending
                    .retain(|pending| pending.room.connection != room.connection);
            }
        }

        let mut interrupted = Vec::new();
        for session in sessions {
            if let Some(turn) = self.active.get_mut(&session) {
                turn.stop_requested = true;
                interrupted.push(PendingInterrupt {
                    session,
                    room: turn.room.clone(),
                });
            }
        }
        eprintln!(
            "{}",
            acb_i18n::t!(
                "kernel.engine.route_interrupt",
                scope = format!("{scope:?}"),
                room = room,
                count = interrupted.len()
            )
        );
        self.pending_interrupts.extend(interrupted);
    }

    /// drain 线程取走打断请求，锁外发 control 帧。
    pub(crate) fn take_interrupts(&mut self) -> Vec<PendingInterrupt> {
        std::mem::take(&mut self.pending_interrupts)
    }

    /// drain 线程取走需要 I/O 的路由动作，交给动作 worker。
    pub(crate) fn take_route_actions(&mut self) -> Vec<PreparedRouteAction> {
        std::mem::take(&mut self.pending_route_actions)
    }

    /// 动作 worker 用：按路由来源准备一次发送（走发送钩子和文件中继）。
    pub(crate) fn prepare_route_send(
        &mut self,
        config: Arc<Config>,
        session: &str,
        room: RoomId,
        elements: Vec<Element>,
    ) -> Result<PreparedSend, ToolError> {
        self.prepare_send(config, session, room, SendOrigin::Route, elements)
    }

    /// 这条消息里有没有「引用本合同一个账号发过的消息」。
    ///
    /// 查不到被引用那条（被清理了、对面没给编号）就当不是——宁可少叫醒一次，
    /// 也不要凭编号长相猜。平台把自己的消息编号给全了，这件事才判得准。
    fn quotes_own_message(&self, draft: &RecordDraft, account: &str) -> Result<bool, StorageError> {
        if account.is_empty() {
            return Ok(false);
        }
        for element in &draft.elements {
            let Element::Quote { message_id } = element else {
                continue;
            };
            let Some(sender) = self.store.sender_of(&draft.room, message_id)? else {
                continue;
            };
            if sender.id == account {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 取下一轮，不管是谁的。没有就返回 `None`。
    pub fn poll_turn(&mut self) -> Result<Option<Delivery>, BridgeError> {
        self.apply_pending_plugin_actions();
        self.apply_staged_config();
        let now = now_millis();
        let index = (0..self.pending.len()).find(|&index| {
            let pending = &self.pending[index];
            pending.not_before <= now && !self.active.contains_key(&pending.session)
        });
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("just found");
        self.open_turn(pending).map(Some)
    }

    /// 只给这一组会话取活。接入点用这个——一条连接只代表它自己那组会话。
    pub fn poll_turn_for(&mut self, sessions: &[String]) -> Result<Option<Delivery>, BridgeError> {
        self.apply_pending_plugin_actions();
        self.apply_staged_config();
        let now = now_millis();
        let index = (0..self.pending.len()).find(|&index| {
            let pending = &self.pending[index];
            if !sessions.iter().any(|session| session == &pending.session) {
                return false;
            }
            pending.not_before <= now && !self.active.contains_key(&pending.session)
        });
        let Some(index) = index else {
            return Ok(None);
        };
        let pending = self.pending.remove(index).expect("just found");
        self.open_turn(pending).map(Some)
    }

    /// 一轮收工。票作废，会话空出来。
    ///
    /// **不推水位**：没确认的消息仍然算未读，下一轮接着投。
    ///
    /// `error` 非空表示 harness 报告这轮失败了。这时按配置的 `on_turn_error` 动作
    /// 列表顺序执行（记日志 / 发到指定房间），让失败信息能被看见，而不是只吞进日志。
    pub fn end_turn(&mut self, ticket: &Ticket, error: Option<&str>) -> Result<(), BridgeError> {
        // 先把这一轮的信息取出来（room + session），remove 之前要用它做失败通知。
        let turn = self.active.get(
            self.by_ticket
                .get(ticket)
                .ok_or_else(|| BridgeError::UnknownTicket(ticket.to_string()))?,
        );
        let turn = turn.cloned();
        let Some(turn) = turn else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };

        // 收轮时记下时序：之后若有工具迟到，可与这条日志对齐，判断是传输竞态还是
        // harness 在 turn_end 之后还在调用。
        let error_suffix = error.map(|error| format!(": {error}")).unwrap_or_default();
        eprintln!(
            "{}",
            acb_i18n::t!(
                "kernel.engine.turn_end_received",
                session = turn.session,
                room = turn.room,
                ticket = ticket.to_string(),
                error = error_suffix
            )
        );

        if let Some(error) = error {
            if turn.stop_requested {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.engine.turn_stopped_by_stop",
                        session = turn.session,
                        room = turn.room,
                        error = error
                    )
                );
            } else {
                self.notify_turn_error(&turn.config, &turn.session, &turn.room, error);
            }
        }

        let Some(session) = self.by_ticket.remove(ticket) else {
            return Err(BridgeError::UnknownTicket(ticket.to_string()));
        };
        // 票作废，但留一小会儿：补确认、写反馈都还要知道「这是哪一轮、哪个房间」。
        // 以前是直接扔掉，于是「轮次结束后 ack 会报 unauthorized」——重复投递的一大来源。
        self.ended.insert(
            ticket.clone(),
            EndedTurn {
                session: turn.session.clone(),
                room: turn.room.clone(),
                delivered: turn.delivered.clone(),
                read: turn.read.clone(),
                config: turn.config.clone(),
                origin_sender: turn.origin_sender.clone(),
                at: Instant::now(),
            },
        );
        self.ended
            .retain(|_, ended| ended.at.elapsed() < TURN_GRACE);
        self.active.remove(&session);
        Ok(())
    }

    /// 取出排队等发的失败通知。endpoint 放下全局锁后执行它们，再回来结算。
    pub(crate) fn take_pending_notices(&mut self) -> Vec<PreparedSend> {
        std::mem::take(&mut self.pending_notices)
    }

    /// 把排队的失败通知一口气发完（测试和非 endpoint 调用点用）。
    ///
    /// endpoint 不走这里：它要先把全局锁放下，等平台的那段时间不挡别人。
    pub fn flush_notices(&mut self) {
        for prepared in self.take_pending_notices() {
            let sent = prepared.execute();
            if let Err(error) = self.finish_send(prepared, sent) {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.engine.turn_error_notice_send_failed",
                        error = format!("{error:?}")
                    )
                );
            }
        }
    }

    /// 交给会话的失败通知 provider 选动作，再按顺序执行。默认 provider 保持原行为：
    /// 会话动作非空就用它，否则回退全局 `default_on_turn_error`。
    /// 一个动作失败不打断后面的。
    fn notify_turn_error(
        &mut self,
        config: &Arc<Config>,
        session: &str,
        room: &RoomId,
        error: &str,
    ) {
        let policy = config
            .session(session)
            .map(|session| session.turn_error.clone())
            .unwrap_or_default();
        let provider = match self.plugins.resolve_turn_error(&policy.provider, &policy) {
            Ok(provider) => provider,
            Err(resolve_error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.engine.turn_error_policy_resolve",
                        provider = policy.provider,
                        error = resolve_error
                    )
                );
                Arc::new(DefaultTurnErrorProvider)
            }
        };
        let actions = provider.decide(
            &policy,
            &TurnErrorRequest {
                session,
                room,
                error,
                actions: config
                    .session(session)
                    .map(|session| session.on_turn_error.as_slice())
                    .unwrap_or(&[]),
                default_actions: &config.default_on_turn_error,
            },
        );
        for action in actions {
            match action {
                // `log` 由 endpoint 的 Log 帧负责（见 turn_end 处理），这里无需重复。
                TurnErrorAction::Log => {}
                TurnErrorAction::Send { room: target } => {
                    let text = format!("[turn failed] session {session} ({room}): {error}");
                    // 这里只组装，不发送：endpoint 会在放下全局锁之后真正等平台。
                    // 失败通知本身发不出去也不该影响主流程，但要把原因记进日志，
                    // 别静默吞掉——否则「为什么没人收到失败提示」会变成查不出的黑洞。
                    match self.prepare_send(
                        config.clone(),
                        session,
                        target,
                        SendOrigin::TurnFailure,
                        vec![Element::text(text)],
                    ) {
                        Ok(prepared) => self.pending_notices.push(prepared),
                        Err(err) => eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "kernel.engine.turn_error_notice_assemble_failed",
                                error = format!("{err:?}")
                            )
                        ),
                    }
                }
            }
        }
    }

    /// 调工具。
    ///
    /// 工具面一律按**会话**圈，不按连接圈（设备概念已删，2026-11-01 用户裁定）：
    /// 能读、能列、能发的房间 = 当前轮次会话订阅的房间。同一条连接上兄弟会话的
    /// 房间不可见——否则一条连接带两个会话时，agent 会自称管着对方会话的房间。
    /// 跨会话发送被砍掉（ADR-0017），读的边界同理。
    pub fn tool_call(
        &mut self,
        ticket: &Ticket,
        _connection_sessions: &[String],
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        if !is_known(tool) {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                acb_i18n::t!("kernel.engine.unknown_tool", tool = tool),
            ));
        }

        let ctx = self.turn_ctx(ticket, tool)?;
        let turn_session = ctx.session.clone();
        let turn_config = ctx.config.clone();
        let origin_sender = ctx.origin_sender.clone();
        // 下面的分支全用 `sessions` 当可见域——按会话圈，不是连接圈。
        let sessions: &[String] = std::slice::from_ref(&turn_session);
        let room = ctx.room.clone();
        let delivered = ctx.delivered.clone();
        let read = ctx.read.clone();
        let live = ctx.live;

        let value = match tool {
            // 不给目标：接着说当前这一轮的房间，用本轮的身份。
            "send_message" => {
                let args: SendArgs = parse(args)?;
                self.deliver_out(
                    turn_config.clone(),
                    &turn_session,
                    &room,
                    SendOrigin::Message,
                    args.elements.0,
                )
            }
            // 给了目标：跨会话发送已砍；目标房间由本会话的访问策略把关。
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                self.ensure_can_send(&turn_config, &turn_session, &target, &room)?;
                self.deliver_out(
                    turn_config.clone(),
                    &turn_session,
                    &target,
                    SendOrigin::Message,
                    args.elements.0,
                )
            }
            "ack_messages" => {
                let args: AckArgs = parse(args)?;
                self.ack(&turn_session, &room, &delivered, &read, args.ids)
            }
            "get_history" => {
                let args: HistoryArgs = parse(args)?;
                self.history(ticket, &turn_config, sessions, &room, args)
            }
            "fetch_records" => {
                let args: FetchRecordsArgs = parse(args)?;
                self.fetch_records(ticket, &turn_config, sessions, &room, args)
            }
            // 锁外等待的工具不能走这条同步路径；正式接入点用 `begin_tool_call` 的
            // `ToolCallStart::Wait`。这里只给直接调用 tool_call 的测试/开发 harness 一个明确错误。
            "wait_message" => Err(ToolError::new(
                ErrorCode::BadRequest,
                "[wait_message] this tool must be called through the real endpoint (it needs to wait \
                 outside the lock); the current call path would block the whole bridge.",
            )),
            "list_rooms" => self.list_rooms(&turn_config, sessions),
            "get_notes" => {
                let args: NotesArgs = parse(args)?;
                self.read_notes(&turn_config, sessions, &room, args)
            }
            "set_note" => {
                let args: SetNoteArgs = parse(args)?;
                self.write_note(&turn_config, sessions, &room, args)
            }
            "del_note" => {
                let args: DelNoteArgs = parse(args)?;
                self.delete_note(&turn_config, sessions, &room, args)
            }
            "get_platform_events" => {
                let args: EventsArgs = parse(args)?;
                self.events(ticket, &turn_config, sessions, &room, args)
            }
            // 平台能力：core 不认识任何一个方法名，只做校验和转交（ADR-0020）。
            "platform_methods" => {
                let args: PlatformMethodsArgs = parse(args)?;
                self.platform_methods(&turn_config, sessions, &room, args)
            }
            "platform_call" => {
                let args: PlatformCallArgs = parse(args)?;
                self.platform_call(ticket, args)
            }
            "describe_plugins" => {
                let args: DescribePluginsArgs = parse(args)?;
                self.describe_plugins(&turn_config, &turn_session, &room, args)
            }
            "extensions_list" => {
                let args: ExtensionsListArgs = parse(args)?;
                self.require_extension_admin(&turn_config, &room, origin_sender.as_deref())?;
                let namespace = extension_namespace(args.namespace.as_deref());
                Ok(self.extension_catalog(namespace))
            }
            "extensions_read" => {
                let args: ExtensionsReadArgs = parse(args)?;
                self.require_extension_admin(&turn_config, &room, origin_sender.as_deref())?;
                let namespace = extension_namespace(args.namespace.as_deref());
                self.read_extension(namespace, &args.path)
            }
            "extensions_write" => {
                let args: ExtensionsWriteArgs = parse(args)?;
                self.require_extension_admin(&turn_config, &room, origin_sender.as_deref())?;
                let namespace = extension_namespace(args.namespace.as_deref());
                self.write_extension(namespace, &args.path, &args.text)
            }
            "extensions_reload" => {
                let args: ExtensionsReloadArgs = parse(args)?;
                self.require_extension_admin(&turn_config, &room, origin_sender.as_deref())?;
                let namespace = extension_namespace(args.namespace.as_deref());
                self.queue_extension_reload(namespace)
                    .map_err(|error| ToolError::new(ErrorCode::Internal, error.to_string()))?;
                Ok(json!({
                    "queued": true,
                    "namespace": namespace,
                    "state": "pending_reload",
                    "note": acb_i18n::t!(
                        "bridge.console.extension_reload_queued",
                        namespace = namespace
                    ),
                }))
            }
            "task_list" => {
                let args: TaskListArgs = parse(args)?;
                self.task_list(
                    &turn_session,
                    args.status.as_deref(),
                    args.limit.unwrap_or(20),
                    args.offset.unwrap_or(0),
                )
            }
            "task_get" => {
                let args: TaskGetArgs = parse(args)?;
                self.task_get(&turn_session, &args.id)
            }
            // 反馈：诚实报告「这里卡住了」。这是出口，不是抱怨箱（ADR-0021）。
            "report_feedback" => {
                let args: FeedbackArgs = parse(args)?;
                self.report_feedback(ticket, &turn_session, &room, live, args)
            }
            // 记忆：文件层。
            "memory_read" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_read(&turn_config, &turn_session, args)
            }
            "memory_write" => {
                let args: MemoryWriteArgs = parse(args)?;
                self.memory_write(&turn_config, &turn_session, args)
            }
            "memory_list" => {
                let args: MemoryListArgs = parse(args)?;
                self.memory_list(&turn_config, &turn_session, args)
            }
            "memory_search" => {
                let args: MemorySearchArgs = parse(args)?;
                self.memory_search(&turn_config, &turn_session, args)
            }
            // 记忆：git 层。
            "memory_git_status" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_status(&turn_config, &turn_session, args)
            }
            "memory_git_log" => {
                let args: MemoryLogArgs = parse(args)?;
                self.memory_git_log(&turn_config, &turn_session, args)
            }
            "memory_git_diff" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_diff(&turn_config, &turn_session, args)
            }
            "memory_git_blame" => {
                let args: MemoryBlameArgs = parse(args)?;
                self.memory_git_blame(&turn_config, &turn_session, args)
            }
            "memory_git_add" => {
                let args: MemoryAddArgs = parse(args)?;
                self.memory_git_add(&turn_config, &turn_session, args)
            }
            "memory_git_commit" => {
                let args: MemoryCommitArgs = parse(args)?;
                self.memory_git_commit(&turn_config, &turn_session, args)
            }
            "memory_git_push" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_push(&turn_config, &turn_session, args)
            }
            "memory_git_branch" => {
                let args: MemoryArgs = parse(args)?;
                self.memory_git_branch(&turn_config, &turn_session, args)
            }
            other => Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                acb_i18n::t!("kernel.engine.unknown_tool", tool = other),
            )),
        }?;

        // 所有工具结果都从这一个口出去：超预算就分页，再不行就落盘（ADR-0022）。
        // 放在这里而不是各个工具里，是为了「新加的工具天然受保护」——
        // 以前每个接口各自截断，于是截断方式各不相同，最糟的一种是把 JSON 砍成半截字符串。
        //
        // `platform_call` 的唯一例外：它可能被 endpoint 拆成「锁内 prepare / 锁外
        // execute / 锁内 finish」，fit 已经在 finish 里做过一次；这里再 fit 会双重分页。
        if tool == "platform_call" {
            Ok(value)
        } else {
            Ok(render::fit(tool, value, &self.spill_dir()))
        }
    }

    /// endpoint 用的两段式入口：能在锁里做完的直接 `Done`，要等平台的返回
    /// `Platform`，由调用方放下 core 全局锁后执行，再回来 `finish_platform_call`。
    pub(crate) fn begin_tool_call(
        &mut self,
        ticket: &Ticket,
        sessions: &[String],
        tool: &str,
        args: Value,
    ) -> Result<ToolCallStart, ToolError> {
        match tool {
            "send_message" => {
                let args: SendArgs = parse(args)?;
                let ctx = self.turn_ctx(ticket, tool)?;
                let prepared = self.prepare_send(
                    ctx.config.clone(),
                    &ctx.session,
                    ctx.room.clone(),
                    SendOrigin::Message,
                    args.elements.0,
                )?;
                Ok(ToolCallStart::Send(prepared))
            }
            "send_message_to" => {
                let args: SendToArgs = parse(args)?;
                let target = resolve_room(&args.to)?;
                let ctx = self.turn_ctx(ticket, tool)?;
                self.ensure_can_send(&ctx.config, &ctx.session, &target, &ctx.room)?;
                let prepared = self.prepare_send(
                    ctx.config.clone(),
                    &ctx.session,
                    target,
                    SendOrigin::Message,
                    args.elements.0,
                )?;
                Ok(ToolCallStart::Send(prepared))
            }
            "platform_call" => {
                let args: PlatformCallArgs = parse(args)?;
                Ok(ToolCallStart::Platform(
                    self.prepare_platform_call(ticket, args)?,
                ))
            }
            "wait_message" => {
                let args: WaitMessageArgs = parse(args)?;
                let ctx = self.turn_ctx(ticket, tool)?;
                let room = match args.room.as_deref() {
                    Some(room) => resolve_room(room)?,
                    None => ctx.room.clone(),
                };
                if room != ctx.room {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] wait_message can only wait on the current turn's room {}; \
                             use get_history or fetch_records to inspect other rooms.",
                            ctx.room
                        ),
                    ));
                }
                let sessions = std::slice::from_ref(&ctx.session);
                self.ensure_readable(&ctx.config, sessions, &room, &ctx.room)?;

                let cursor = Cursor::new(ctx.session.clone(), room.clone());
                let watermark = self.store.watermark(&cursor).map_err(internal)?;
                let seen = ctx
                    .delivered
                    .iter()
                    .chain(ctx.read.iter())
                    .max()
                    .copied()
                    .unwrap_or(0);
                let after = watermark.max(seen);
                let timeout = Duration::from_millis(
                    args.timeout_ms
                        .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
                        .min(MAX_WAIT_TIMEOUT_MS),
                );
                let limit = args.limit.unwrap_or(DEFAULT_PAGE_LIMIT).min(MAX_PAGE_LIMIT);
                let columns = column_set(args.columns.as_deref())?;

                Ok(ToolCallStart::Wait(PreparedWait {
                    ticket: ticket.clone(),
                    room,
                    after,
                    limit,
                    timeout,
                    columns,
                }))
            }
            // 其余工具不碰外部 I/O（读流水/记忆/笔记都在本地），保持原路径。
            _ => {
                let value = self.tool_call(ticket, sessions, tool, args)?;
                Ok(ToolCallStart::Done(value))
            }
        }
    }

    /// 校验并组装一次 `platform_call`，但**不执行**平台 I/O。
    fn prepare_platform_call(
        &mut self,
        ticket: &Ticket,
        args: PlatformCallArgs,
    ) -> Result<PreparedPlatformCall, ToolError> {
        let ctx = self.turn_ctx(ticket, "platform_call")?;
        let sessions = std::slice::from_ref(&ctx.session);
        let room = self.resolve(&args.room, &ctx.room)?;
        let config = ctx.config.clone();

        let methods = self.outbound.catalog(&room.connection);
        if methods.is_empty() {
            return Err(no_catalog(&room.connection));
        }
        let Some(spec) = methods
            .iter()
            .find(|method| method.name.as_ref() == args.method.as_str())
        else {
            let hint = match methods
                .iter()
                .find(|method| method.group() == group_of_name(&args.method))
            {
                Some(near) => acb_i18n::t!(
                    "kernel.engine.unknown_method_did_you_mean",
                    name = near.name
                ),
                None => acb_i18n::t!(
                    "kernel.engine.unknown_method_supports",
                    methods = method_names(&methods)
                ),
            };
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "kernel.engine.unknown_method",
                    method = args.method,
                    hint = hint
                ),
            ));
        };

        match spec.scope {
            Scope::Room => {
                // 只读方法和写方法分开判：策略可以只放开读（或只放开写）。
                let action = if spec.effect == Effect::Read {
                    AccessAction::Read
                } else {
                    AccessAction::Send
                };
                self.ensure_room_access(&ctx.config, sessions, action, &room, &ctx.room)?;
            }
            Scope::Connection => {
                self.ensure_connection_readable(&ctx.config, sessions, &room.connection, &ctx.room)?
            }
        }

        // 要写流水的方法必须先知道“我是谁”。放在平台调用之前查：平台已经发出去了
        // 再发现记不了账，那条消息就会在自己的水位上留洞。
        if spec.record != RecordMode::None && config.account_of_room(&room).is_empty() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.engine.no_account", room = room),
            ));
        }

        if spec.risk == Risk::Destructive
            && !Self::allows_destructive(&ctx.config, &room.connection, spec.name.as_ref())
        {
            return Err(ToolError::new(
                ErrorCode::ToolNotAllowed,
                format!(
                    "[forbidden] {} is a destructive operation and connection {} does not allow it \
                     by default. Enable it in the [[connections]] section of config.toml with \
                     allow_destructive = true (or list only the methods to enable, e.g. \
                     allow_destructive = [\"{}\"])",
                    spec.name, room.connection, spec.name
                ),
            ));
        }

        check_params(spec, &args.params)?;
        check_target_params(spec, &room, &args.params)?;
        // params 里的本机文件路径换成中继 URL（ADR-0025）。这一步只碰本地文件，
        // 在锁里做没问题；真正的平台调用在锁外。
        let params = self.relay_params(args.params.clone())?;

        let outbound = self.outbound.handle_for(&room.connection).ok_or_else(|| {
            call_error(CallError::Unavailable(acb_i18n::t!(
                "kernel.engine.no_connection_call",
                connection = room.connection,
                room = room
            )))
        })?;

        Ok(PreparedPlatformCall {
            room,
            spec: spec.clone(),
            params,
            args,
            config,
            outbound,
        })
    }

    /// 平台结果回来之后：发送类调用先记账，再做查询/分页，最后 fit。
    ///
    /// 记账必须做：模型用 `platform_call` 发的消息也是「自己说的话」。不写流水、
    /// 不确认，它就会在自己的水位上留一个洞，洞后面的新消息永远推不过去
    /// （同 `finish_send` 里 2026-10-31 那条教训）。记录内容由适配器翻译
    /// （`OutgoingRecord`），core 不认识平台消息段。
    pub(crate) fn finish_platform_call(
        &mut self,
        prepared: PreparedPlatformCall,
        result: Result<CallOutcome, CallError>,
    ) -> Result<Value, ToolError> {
        let PreparedPlatformCall {
            room,
            spec,
            args,
            config,
            ..
        } = prepared;
        let CallOutcome { value, record } = result.map_err(call_error)?;

        if spec.record != RecordMode::None {
            if let Some(record) = record {
                self.record_outgoing(&config, &room, record)
                    .map_err(internal)?;
            }
        }

        let grouped = args.query.group_by.is_some();
        let result = crate::query::apply_to_value(value, &args.query)
            .map_err(|error| ToolError::new(ErrorCode::BadRequest, error.to_string()))?;
        let result = if grouped {
            result
        } else {
            let limit = args
                .limit
                .unwrap_or(DEFAULT_PAGE_LIMIT)
                .clamp(1, MAX_PAGE_LIMIT);
            let offset = args.offset.unwrap_or(0);
            render::slice(result, offset, limit)
        };
        Ok(render::fit("platform_call", result, &self.spill_dir()))
    }

    /// `wait_message` 的锁内重查：有新消息就返回并记为「本轮读过」；
    /// 没有就返回 `None`，endpoint 继续在锁外等。
    pub(crate) fn poll_wait(
        &mut self,
        prepared: &PreparedWait,
    ) -> Result<Option<Value>, ToolError> {
        let Some(session) = self.by_ticket.get(&prepared.ticket).cloned() else {
            return Err(no_active_round());
        };
        let Some(active) = self.active.get(&session) else {
            return Err(no_active_round());
        };
        if active.room != prepared.room {
            return Err(no_active_round());
        }
        let account = active.config.account_of_room(&prepared.room);
        let mut records = self
            .pending_records(&session, &prepared.room, &account)
            .map_err(internal)?;
        records.retain(|record| record.id > prepared.after);
        records.truncate(prepared.limit);
        if records.is_empty() {
            return Ok(None);
        }

        let ids: Vec<u64> = records.iter().map(|record| record.id).collect();
        self.mark_read(&prepared.ticket, &prepared.room, ids);
        self.fill_quote_refs(&prepared.room, &mut records);

        let rendered: Vec<Value> = records
            .iter()
            .map(|record| record_columns(record, &prepared.columns))
            .collect();
        Ok(Some(json!({
            "room": prepared.room,
            "after": prepared.after,
            "timed_out": false,
            "returned": rendered.len(),
            "records": rendered,
        })))
    }

    /// `wait_message` 超时：确认票仍有效后返回空。
    pub(crate) fn finish_wait(&mut self, prepared: &PreparedWait) -> Result<Value, ToolError> {
        if !self.by_ticket.contains_key(&prepared.ticket) {
            return Err(no_active_round());
        }
        Ok(json!({
            "room": prepared.room,
            "after": prepared.after,
            "timed_out": true,
            "returned": 0,
            "records": [],
            "note": "这段时间没有新消息。不要空转重试；可以继续处理手上的事，或稍后再拉一次。",
        }))
    }

    /// 直连 `tool_call` 的 `platform_call` 路径：校验 + 执行 + 结算，仍在调用方锁里。
    /// endpoint 走 `begin_tool_call` / `finish_platform_call` 的锁外版本。
    fn platform_call(
        &mut self,
        ticket: &Ticket,
        args: PlatformCallArgs,
    ) -> Result<Value, ToolError> {
        let prepared = self.prepare_platform_call(ticket, args)?;
        let result = prepared.execute();
        self.finish_platform_call(prepared, result)
    }

    /// 这张票能干什么。
    ///
    /// 分两种情况：轮次在跑（正常）；轮次刚结束但票还没过期（补确认、写反馈）。
    /// 后者是这次修的：以前票一作废，所有调用一律 unauthorized，agent 分不清
    /// 「轮次结束了」和「桥坏了」，于是反复重试、反复被重投。
    fn turn_ctx(&mut self, ticket: &Ticket, tool: &str) -> Result<TurnCtx, ToolError> {
        if let Some(session) = self.by_ticket.get(ticket) {
            let turn = self
                .active
                .get(session)
                .expect("a ticket always has an active turn");
            return Ok(TurnCtx {
                session: turn.session.clone(),
                room: turn.room.clone(),
                delivered: turn.delivered.clone(),
                read: turn.read.clone(),
                config: turn.config.clone(),
                origin_sender: turn.origin_sender.clone(),
                live: true,
            });
        }

        let Some(ended) = self.ended.get(ticket).cloned() else {
            return Err(no_active_round());
        };

        // 迟到调用统一留痕：工具名、票、距 turn_end 的毫秒数。排查「轮次结束后
        // harness 还在调工具」时，这条日志是唯一能区分传输竞态和越界的锚点。
        eprintln!(
            "{}",
            acb_i18n::t!(
                "kernel.engine.late_tool",
                tool = tool,
                session = ended.session,
                room = ended.room,
                ticket = ticket.to_string(),
                ms = ended.at.elapsed().as_millis()
            )
        );

        // 轮次结束了。只放行三件事——其余工具这轮真的用不了，说清楚让 agent 收手。
        match tool {
            "ack_messages" => {
                if ended.at.elapsed() > ACK_GRACE {
                    return Err(ToolError::new(
                        ErrorCode::Unauthorized,
                        format!(
                            "[ack_expired] more than {} seconds have passed since this turn ended; \
                             the late-acknowledgement window is closed. Those records will be \
                             delivered again as unread in the next turn (same ids); acknowledge them \
                             then. Do not retry now - retrying will not succeed.",
                            ACK_GRACE.as_secs()
                        ),
                    ));
                }
            }
            "send_message" => {
                if ended.at.elapsed() > SEND_GRACE {
                    return Err(ToolError::new(
                        ErrorCode::Unauthorized,
                        format!(
                            "[send_expired] more than {} seconds have passed since this turn ended; \
                             the late-send window is closed. Do not retry now - the message will not \
                             go out. If the content still matters, say it in the next turn.",
                            SEND_GRACE.as_secs()
                        ),
                    ));
                }
            }
            // 任务结果查询不依赖任务触发时的票：任务可能在本轮结束后才完成。
            "report_feedback" | "task_list" | "task_get" => {}
            _ => return Err(no_active_round()),
        }

        Ok(TurnCtx {
            session: ended.session,
            room: ended.room,
            delivered: ended.delivered,
            read: ended.read,
            config: ended.config,
            origin_sender: ended.origin_sender,
            live: false,
        })
    }

    /// 落盘兜底放在哪：状态库旁边（`state/spill/`），跟库一起备份、一起清。
    fn spill_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.config.storage.path)
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(|parent| parent.join("spill"))
            .unwrap_or_else(|| std::path::PathBuf::from("spill"))
    }

    // ---- 内部 ----

    /// 记下「本轮读过哪些编号」。只有当前轮房间的读取才算：`ack_messages`
    /// 只能确认当前轮房间，记到别的房间没有意义。
    fn mark_read(&mut self, ticket: &Ticket, room: &RoomId, ids: impl IntoIterator<Item = u64>) {
        let session = self.by_ticket.get(ticket).cloned();
        if let Some(session) = session {
            if let Some(turn) = self.active.get_mut(&session) {
                if &turn.room == room {
                    turn.read.extend(ids);
                }
            }
            return;
        }
        if let Some(turn) = self.ended.get_mut(ticket) {
            if &turn.room == room {
                turn.read.extend(ids);
            }
        }
    }

    /// 这个会话在这个房间里真正要处理的未读：水位之后、不在乱序确认里、且不是自己说的。
    ///
    /// 取全量再筛选，不再只取「最旧 200 条」——混投策略要同时拿到最旧一小段和最新一批，
    /// 只取最旧一段会让新消息永远排不上。
    fn pending_records(
        &self,
        session: &str,
        room: &RoomId,
        account: &str,
    ) -> Result<Vec<Record>, StorageError> {
        let cursor = Cursor::new(session, room.clone());
        let mut records = unread(&self.store, &cursor, usize::MAX)?;
        records.retain(|record| record.sender.id != account);
        Ok(records)
    }

    /// 把引用里的平台编号翻成本房间的水位编号（ADR-0024）。
    ///
    /// 平台给的是它自己的编号（`-1179324851`），模型手上是 `#621`。以前这一步得模型
    /// 自己做，做不了就只能猜；现在两个号码并排给出来。
    ///
    /// 查不到也留一个 `id: None` 的 `quote_ref`——「这个房间里没有那条」本身就是信息
    /// （被清理了，或者引用的是别的平台/别的房间的消息），比什么都不给强。
    fn fill_quote_refs(&self, room: &RoomId, records: &mut [Record]) {
        for record in records.iter_mut() {
            let Some(platform_id) = record.quote_of().map(str::to_owned) else {
                continue;
            };
            let id = self
                .store
                .located(room, &platform_id)
                .ok()
                .flatten()
                .map(|found| found.id);
            record.quote_ref = Some(QuoteRef { platform_id, id });
        }
    }

    fn subscribers_of(config: &Config, room: &RoomId) -> Vec<Subscriber> {
        let account = config.account_of_room(room);
        config
            .sessions
            .iter()
            .filter_map(|session| {
                session
                    .subscriptions
                    .iter()
                    .find(|subscription| &subscription.room == room)
                    .map(|subscription| Subscriber {
                        session: session.id.clone(),
                        account: account.clone(),
                        wake: subscription.wake.clone(),
                    })
            })
            .collect()
    }

    /// 用会话自己的访问策略判定一次访问。
    ///
    /// provider 只回答允许/拒绝；拒绝的形状（错误码、文案）由 core 统一组织。
    /// `sessions` 取并集：任何一个会话被允许就算通过（工具面目前只传当前会话一个）。
    fn check_access(
        &self,
        config: &Config,
        sessions: &[String],
        action: AccessAction,
        room: Option<&RoomId>,
        connection: &str,
        origin: Option<&RoomId>,
    ) -> Result<(), ToolError> {
        let mut denied = Vec::new();
        for id in sessions {
            let Some(session) = config.session(id) else {
                continue;
            };
            let provider = self
                .plugins
                .resolve_access(&session.access.provider, &session.access)
                .map_err(|error| {
                    ToolError::new(
                        ErrorCode::Internal,
                        acb_i18n::t!(
                            "kernel.engine.access_policy_unavailable",
                            provider = session.access.provider,
                            session = id,
                            error = error
                        ),
                    )
                })?;
            let subscriptions: Vec<RoomId> = session
                .subscriptions
                .iter()
                .map(|subscription| subscription.room.clone())
                .collect();
            let request = AccessRequest {
                session: id,
                connection,
                room,
                origin_room: origin,
                action,
                subscriptions: &subscriptions,
            };
            if provider.check(&session.access, &request) == AccessDecision::Allow {
                return Ok(());
            }
            denied.push(format!("{} ({})", id, session.access.provider));
        }

        let detail = match action {
            AccessAction::Connection => acb_i18n::t!(
                "kernel.engine.access_no_rooms",
                session = sessions.join(", "),
                connection = connection
            ),
            AccessAction::Read | AccessAction::Send => acb_i18n::t!(
                "kernel.engine.access_not_allowed",
                session = sessions.join(", "),
                room = room.map(ToString::to_string).unwrap_or_default()
            ),
        };
        let policy = if denied.is_empty() {
            String::new()
        } else {
            acb_i18n::t!(
                "kernel.engine.access_policy_suffix",
                denied = denied.join("; ")
            )
        };
        Err(ToolError::new(
            ErrorCode::RoomNotSubscribed,
            format!("{detail}{policy}"),
        ))
    }

    /// 能不能对 `room` 做 `action`。
    fn ensure_room_access(
        &self,
        config: &Config,
        sessions: &[String],
        action: AccessAction,
        room: &RoomId,
        origin: &RoomId,
    ) -> Result<(), ToolError> {
        self.check_access(
            config,
            sessions,
            action,
            Some(room),
            &room.connection,
            Some(origin),
        )
    }

    /// 读的工具默认要求「能读这个房间」。具体边界由会话的访问策略决定。
    fn ensure_readable(
        &self,
        config: &Config,
        sessions: &[String],
        room: &RoomId,
        origin: &RoomId,
    ) -> Result<(), ToolError> {
        self.ensure_room_access(config, sessions, AccessAction::Read, room, origin)
    }

    /// 当前会话能不能往这个房间发。`send_message_to` 的边界。
    fn ensure_can_send(
        &self,
        config: &Config,
        session: &str,
        room: &RoomId,
        origin: &RoomId,
    ) -> Result<(), ToolError> {
        let sessions = [session.to_owned()];
        self.ensure_room_access(config, &sessions, AccessAction::Send, room, origin)
    }

    /// 连接级权限：访问策略决定这条连接上有没有可用的房间。
    ///
    /// 好友列表、账号资料这类方法跟房间无关，所以不要求模型报出一个具体房间；
    /// 但也不能凭空调一个自己完全够不着的号。
    fn ensure_connection_readable(
        &self,
        config: &Config,
        sessions: &[String],
        connection: &str,
        origin: &RoomId,
    ) -> Result<(), ToolError> {
        self.check_access(
            config,
            sessions,
            AccessAction::Connection,
            None,
            connection,
            Some(origin),
        )
    }

    /// 时钟评估：检查每个订阅的时钟策略，到点就把（会话, 房间）排进队列。
    ///
    /// 只做廉价检查：解析 provider 时用当前配置快照，到点才开轮。返回本次排了几轮。
    pub fn tick_clock(&mut self, now: i64) -> usize {
        // 先把订阅整理出来，避免遍历 config 的同时改 self 的其他字段。
        let clocks: Vec<(String, Vec<(RoomId, ClockPolicy)>)> = self
            .config
            .sessions
            .iter()
            .map(|session| {
                (
                    session.id.clone(),
                    session
                        .subscriptions
                        .iter()
                        .map(|subscription| (subscription.room.clone(), subscription.clock.clone()))
                        .collect(),
                )
            })
            .collect();

        let mut queued = 0;
        for (session, subscriptions) in clocks {
            for (room, policy) in subscriptions {
                let key = (session.clone(), room.clone());
                let Some(last_clock_at) = self.clock_last.get(&key).copied() else {
                    // 第一次见到这个订阅：从这一 tick 开始计间隔，不立刻开轮。
                    self.clock_last.insert(key.clone(), now);
                    self.wake_last.entry(key).or_insert(now);
                    continue;
                };
                // 会话正在跑，或这个房间已经排着队：等它空下来再说，别叠轮次。
                if self.active.contains_key(&session)
                    || self
                        .pending
                        .iter()
                        .any(|pending| pending.session == session && pending.room == room)
                {
                    continue;
                }
                let provider = match self.plugins.resolve_clock(&policy.provider, &policy) {
                    Ok(provider) => provider,
                    Err(error) => {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "kernel.engine.clock_policy_resolve",
                                provider = policy.provider,
                                session = session,
                                room = room,
                                error = error
                            )
                        );
                        continue;
                    }
                };
                let last_record_at = match self.latest_record_at(&room) {
                    Ok(at) => at,
                    Err(error) => {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "kernel.engine.last_record_read_failed",
                                room = room,
                                error = error
                            )
                        );
                        None
                    }
                };
                let last_wake_at = self.wake_last.get(&key).copied().unwrap_or(now);
                let context = ClockContext {
                    session: &session,
                    room: &room,
                    now,
                    last_clock_at,
                    last_wake_at,
                    last_record_at,
                };
                if provider.is_due(&policy, &context) {
                    self.enqueue(
                        session.clone(),
                        room.clone(),
                        TurnReason::Clock,
                        None,
                        None,
                        now,
                        0,
                    );
                    self.clock_last.insert(key, now);
                    queued += 1;
                }
            }
        }
        queued
    }

    fn latest_record_at(&self, room: &RoomId) -> Result<Option<i64>, BridgeError> {
        let Some(id) = self.store.latest(room)? else {
            return Ok(None);
        };
        let Some(record) = self
            .store
            .since(room, id.saturating_sub(1), 1)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        Ok(Some(record.at))
    }

    fn enqueue(
        &mut self,
        session: String,
        room: RoomId,
        reason: TurnReason,
        wake_reason: Option<String>,
        origin_sender: Option<String>,
        at: i64,
        delay_ms: u64,
    ) {
        self.wake_last.insert((session.clone(), room.clone()), at);
        let not_before = at.saturating_add(delay_ms.min(MAX_WAKE_DELAY_MS) as i64);
        // 同一 (会话, 房间) 只排一轮。后来的唤醒只可能把它往前提，
        // 不会把已经等着的轮次再往后拖。
        if let Some(existing) = self
            .pending
            .iter_mut()
            .find(|pending| pending.session == session && pending.room == room)
        {
            existing.not_before = existing.not_before.min(not_before);
            if origin_sender.is_some() {
                existing.origin_sender = origin_sender;
            }
            return;
        }
        self.pending.push_back(Pending {
            session,
            room,
            reason,
            wake_reason,
            origin_sender,
            not_before,
        });
    }

    fn open_turn(&mut self, pending: Pending) -> Result<Delivery, BridgeError> {
        // 开轮时冻结配置：这一轮后续的工具查询都用它，热重载只影响下一轮。
        let turn_config = Arc::new(self.config.clone());
        let account = turn_config.account_of_room(&pending.room);
        self.heal_own_holes(&pending.session, &pending.room, &account)?;
        let mut records = self.pending_records(&pending.session, &pending.room, &account)?;
        // 引用里的平台编号翻成本房间编号，跟平台编号并排放（ADR-0024）。
        self.fill_quote_refs(&pending.room, &mut records);

        // 先选本轮真正要投的批次，再计数、再进 delivered：
        // 没投出去的记录不能算「投过」，否则 ack_messages 会放行没见过的编号，
        // 投递次数也会被白白加上一次。
        //
        // 消息过多时给最旧一小段（让水位能继续前进）+ 最新一批（让模型看得到
        // 眼前的话），中间那段跳过并明确告诉模型编号区间。
        let limit = if turn_config.delivery_limit == 0 {
            MAX_DELIVERY
        } else {
            turn_config.delivery_limit.min(MAX_DELIVERY)
        };
        // composer 只决定「投哪些编号」；它看不到 store、票和水位，选出来的编号
        // 还要在这里过一遍真实记录集合——未知编号、重复编号都不会凭空进 delivered。
        let delivery_policy = turn_config
            .session(&pending.session)
            .map(|session| session.delivery.clone())
            .unwrap_or_default();
        let composer = match self
            .plugins
            .resolve_delivery(&delivery_policy.provider, &delivery_policy)
        {
            Ok(composer) => composer,
            Err(error) => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.engine.delivery_policy_resolve",
                        provider = delivery_policy.provider,
                        error = error
                    )
                );
                Arc::new(DefaultDeliveryComposer)
            }
        };
        let plan = composer.compose(
            &delivery_policy,
            &DeliveryRequest {
                session: &pending.session,
                room: &pending.room,
                records: &records,
                limit,
            },
        );
        let omitted = plan.omitted;
        let more = omitted.map(|omitted| omitted.count);
        let mut by_id: HashMap<u64, Record> = records.into_iter().map(|r| (r.id, r)).collect();
        let mut selected = Vec::with_capacity(plan.keep.len());
        for id in &plan.keep {
            if let Some(record) = by_id.remove(id) {
                selected.push(record);
            }
        }
        records = selected;

        let mut delivered = BTreeSet::new();
        let cursor = Cursor::new(pending.session.clone(), pending.room.clone());
        for record in &mut records {
            // 计数落在存储里：重启之后同一个 N 接着数，不然看着像从没投过。
            let count = self.store.count_delivery(&cursor, record.id)?;
            if count > 1 {
                record.redelivery = Some(count);
                // 「第 N 次投递」和「怎么让它别再重投」必须跟着记录一起到模型眼前。
                // 这件事放在桥里做，不指望每个 harness 都记得补：harness 的职责只是
                // 把 records 塞进上下文，多一句话都不该由它发明（ADR-0010）。
                //
                // 注意写在 `note` 上，不塞进 elements：elements 是发送者说的话，
                // 系统的旁白混进去会让人（和模型）以为是对方打的字。
                record.note = Some(format!(
                    "第 {} 次投递；调 ack_messages 确认 #{} 之后就不再重复投递",
                    count, record.id
                ));
            }
            delivered.insert(record.id);
        }

        self.ticket_seq += 1;
        let ticket = Ticket::new(format!(
            "{}-{}-{}",
            pending.session, self.ticket_prefix, self.ticket_seq
        ))
        .expect("ticket must not be empty");

        self.by_ticket
            .insert(ticket.clone(), pending.session.clone());
        self.active.insert(
            pending.session.clone(),
            ActiveTurn {
                session: pending.session.clone(),
                room: pending.room.clone(),
                delivered,
                read: BTreeSet::new(),
                config: turn_config.clone(),
                origin_sender: pending.origin_sender.clone(),
                stop_requested: false,
            },
        );

        // 把这个会话引用的记忆连同 worktree 路径一起带给 harness。惰性初始化：worktree
        // 第一次用到才 checkout，之后复用。
        let memories = self.session_memories(&turn_config, &pending.session);

        // 会话的身份信息：harness 据此知道「以什么身份说话、用哪个模型」。
        let session_info = turn_config
            .session(&pending.session)
            .map(|session| SessionInfo {
                id: session.id.clone(),
                name: session.name.clone(),
                harness: session.harness.clone(),
                model: session.model.clone(),
            });
        // 人格卡：桥原样下发，怎么塞进模型上下文由 harness 适配器决定。
        let persona = turn_config
            .session(&pending.session)
            .and_then(|session| turn_config.persona(&session.persona))
            .map(|persona| PersonaInfo {
                id: persona.id.clone(),
                name: persona.name.clone(),
                text: persona.text.clone(),
            });

        // 批次已经由 composer 选好（默认是最旧一小段 + 最新一批），这里直接用它。
        Ok(Delivery {
            ticket,
            session: pending.session,
            room: pending.room,
            records,
            reason: pending.reason,
            wake_reason: pending.wake_reason,
            memories,
            session_info,
            persona,
            more,
            omitted,
            // 0 = 配置里关了超时，harness 收到就别自己掐。
            turn_timeout_ms: Some(turn_config.turn_timeout_ms()),
        })
    }

    /// 开轮前自愈：把水位之上「本连接账号自己发的」记录确认掉。
    ///
    /// 自己发的消息不算未读（投递时会被过滤），但水位只推连续前沿——这些记录
    /// 如果停在水位之上，就是前沿迈不过去的洞，洞后面的真消息会被反复重投。
    /// 历史上 deliver_out 只确认发送者自己的会话（2026-10-31 修），留下的旧洞
    /// 也靠这里在下次开轮时自动补上，不用手工修库。
    fn heal_own_holes(
        &mut self,
        session: &str,
        room: &RoomId,
        account: &str,
    ) -> Result<(), BridgeError> {
        if account.is_empty() {
            return Ok(());
        }
        let cursor = Cursor::new(session, room.clone());
        let watermark = self.store.watermark(&cursor)?;
        let own: Vec<u64> = self
            .store
            .since(room, watermark, usize::MAX)?
            .into_iter()
            .filter(|record| record.sender.id == account)
            .map(|record| record.id as u64)
            .collect();
        if own.is_empty() {
            return Ok(());
        }
        self.store.ack(&cursor, &own)?;
        Ok(())
    }

    /// 会话引用的记忆列表（id + worktree 路径）。worktree 惰性 checkout，失败这一轮就
    /// 开不成——记忆配错了要早点暴露，别让 harness 拿到一个碰不了的路径。
    fn session_memories(&self, config: &Config, session: &str) -> Vec<MemoryInfo> {
        let Some(session_cfg) = config.session(session) else {
            return Vec::new();
        };
        session_cfg
            .memories
            .iter()
            .filter_map(|memory_id| {
                let memory = config.memory(memory_id)?;
                let worktree = self
                    .memory
                    .ensure_worktree(&memory.location, memory_id, session)
                    .ok()?;
                Some(MemoryInfo {
                    id: memory_id.clone(),
                    worktree: worktree.display().to_string(),
                })
            })
            .collect()
    }

    fn deliver_out(
        &mut self,
        config: Arc<Config>,
        session: &str,
        room: &RoomId,
        origin: SendOrigin,
        elements: Vec<Element>,
    ) -> Result<Value, ToolError> {
        let prepared = self.prepare_send(config, session, room.clone(), origin, elements)?;
        let sent = prepared.execute();
        self.finish_send(prepared, sent)
    }

    /// 校验、切调用标记、跑发送前钩子、中继本机文件、查账号、取出发通道——但**不发送**。
    /// endpoint 可以拿着这个结果放下全局锁，真正等平台。
    fn prepare_send(
        &mut self,
        config: Arc<Config>,
        session: &str,
        room: RoomId,
        origin: SendOrigin,
        mut elements: Vec<Element>,
    ) -> Result<PreparedSend, ToolError> {
        // 先把混进正文的「调用标记」切掉。这不是洁癖：群里真的出现过带
        // `</mcp__acb__send_message>` 和特殊 token 的回复（见 sanitize 模块）。
        // 切完落空的文本段直接丢，免得往平台上发一个空段。
        let trimmed = crate::sanitize::trim_elements(&mut elements);
        if trimmed {
            elements.retain(
                |element| !matches!(element, Element::Text { text } if text.trim().is_empty()),
            );
        }
        if elements.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                if trimmed {
                    "[bad_params] your entire text consists of call markup (`</text>` etc.); nothing \
                     remains to send after stripping it. Write the message itself separately - put \
                     arguments in their own fields, not in the body."
                        .to_owned()
                } else {
                    acb_i18n::t!("kernel.engine.nothing_to_send")
                },
            ));
        }

        // 发送前钩子：按会话配置的顺序跑，可改写正文（例如加签名、脱敏）。
        // 钩子失败就让这次发送失败，绝不静默跳过——否则「以为挂了钩子」只是错觉。
        let outbound_policy = config
            .session(session)
            .map(|session| session.outbound.clone())
            .unwrap_or_default();
        let before_hooks = self
            .plugins
            .resolve_send_hooks(&outbound_policy.before)
            .map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!("kernel.engine.before_hook_resolve_failed", error = error),
                )
            })?;
        let after_hooks = self
            .plugins
            .resolve_send_hooks(&outbound_policy.after)
            .map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!("kernel.engine.after_hook_resolve_failed", error = error),
                )
            })?;
        let hook_context = SendContext {
            session,
            room: &room,
            origin,
        };
        let mut modified_by = Vec::new();
        for (id, hook) in outbound_policy.before.iter().zip(before_hooks.iter()) {
            let before = elements.clone();
            hook.before_send(&hook_context, &mut elements)
                .map_err(|error| {
                    ToolError::new(
                        ErrorCode::Internal,
                        acb_i18n::t!("kernel.engine.before_hook_failed", error = error),
                    )
                })?;
            // 只报告真的改了正文的钩子；没改的也在回执里列出来只会变成噪音。
            if elements != before {
                modified_by.push(id.to_string());
            }
        }
        if elements.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.engine.before_hook_cleared_body"),
            ));
        }

        // 本机文件换成平台取得到的 URL（ADR-0025）。配了 [file_relay] 才有动作；
        // 没配、或路径不是本机存在的文件，就原样透传——平台侧路径（共享卷等）不受影响。
        let relayed = self.relay_elements(&mut elements)?;

        let account = config.account_of_room(&room);
        if account.is_empty() {
            return Err(ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.engine.no_account", room = room),
            ));
        }

        let outbound = self.outbound.handle_for(&room.connection).ok_or_else(|| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "kernel.engine.no_connection_send",
                    connection = room.connection,
                    room = room
                ),
            )
        })?;

        Ok(PreparedSend {
            room,
            elements,
            config,
            outbound,
            trimmed,
            relayed,
            session: session.to_owned(),
            origin,
            after_hooks,
            modified_by,
        })
    }

    /// 出站结果回来之后：写流水、对所有订阅者 ack、拼回执。
    pub(crate) fn finish_send(
        &mut self,
        prepared: PreparedSend,
        result: Result<Sent, OutboundError>,
    ) -> Result<Value, ToolError> {
        let PreparedSend {
            room,
            elements,
            config,
            trimmed,
            relayed,
            session,
            origin,
            after_hooks,
            modified_by,
            ..
        } = prepared;

        let sent = match result {
            Ok(sent) => sent,
            Err(error) => {
                // 平台侧拉不到媒体 URL 时的报错只有一句干巴巴的 fetch failed
                // （平台内部自己的下载失败，桥拿不到它的 errno）。桥补得上的是
                // 上下文：哪些 URL、很可能只有发消息这侧可达、本机文件该怎么写。
                let mut tool_error = outbound_error(error);
                let urls: Vec<&str> = elements.iter().flat_map(|e| e.media_urls()).collect();
                if !urls.is_empty() {
                    tool_error.message.push_str(&format!(
                        "\n[hint] 这条消息带了媒体：{}。平台是自己去下载这个地址的，\
                         只有你这一侧可达的地址（本机端口、你刚生成的服务的地址）它会拉不到。\
                         本机文件直接把路径写进 url，桥会自动中继成平台取得到的地址。",
                        urls.join(", ")
                    ));
                }
                return Err(tool_error);
            }
        };

        // 发送成功后跑观察钩子。它们只拿到回执，不能改已经发出去的内容。
        let hook_context = SendContext {
            session: &session,
            room: &room,
            origin,
        };
        for hook in &after_hooks {
            hook.after_send(&hook_context, &sent);
        }

        let account = config.account_of_room(&room);
        let (record, watermark) = self
            .record_and_ack(
                &config,
                RecordDraft::message(
                    room.clone(),
                    Sender {
                        id: account.clone(),
                        name: account,
                        role: None,
                    },
                    sent.at,
                    elements,
                    false,
                )
                // 记下平台编号：下次有人引用这条，桥才反查得出「是我的话」。
                .with_platform_id(sent.platform_id.clone()),
            )
            .map_err(internal)?;

        // 回执要「像回执」：ok 摆在第一位，发出去了才给 platform_id / at。
        // 失败时这个函数直接返回 Err（调用方拿到的是 error），绝不会出现
        // 「一脸成功但平台没收到」的形状——模型据此判断能不能去群里说「已经发了」。
        let mut receipt = json!({
            "ok": true,
            "id": record.id,
            "platform_id": sent.platform_id,
            "at": sent.at,
            "room": room.to_string(),
            "watermark": watermark,
        });
        if trimmed {
            // 必须说：模型以为发出去的是全文，实际少了尾巴。不说它就永远不知道。
            receipt["trimmed"] = json!(true);
            receipt["note"] = json!(
                "你的 text 里混着调用标记（`</text>`、`<｜｜DSML｜｜` 之类），\
                 已经在第一个标记处截断，后面那截没发出去。\
                 那串东西是解析工具调用时漏进来的，不是正文；要传的参数放各自字段里，别写进 text。"
            );
        }
        if !relayed.is_empty() {
            // 也必须说：模型给的是本机路径，实际发出去的是中继 URL。
            receipt["relayed"] = json!(
                relayed
                    .iter()
                    .map(|(from, to)| json!({"from": from, "to": to}))
                    .collect::<Vec<_>>()
            );
        }
        if !modified_by.is_empty() {
            // 发送前钩子改过正文：不说，模型会以为群里看到的是自己写的原文。
            receipt["modified_by"] = json!(modified_by);
        }
        Ok(receipt)
    }

    /// 适配器翻译好的出站记录 → 流水。
    ///
    /// 只由 `finish_platform_call` 调用：发送类平台方法（`MethodSpec::record` 非空）
    /// 成功后由适配器带回实际发出去的内容。平台编号、时间缺省时用本地时间。
    fn record_outgoing(
        &mut self,
        config: &Config,
        room: &RoomId,
        record: OutgoingRecord,
    ) -> Result<(Record, u64), BridgeError> {
        let account = config.account_of_room(room);
        let sender = Sender {
            id: account.clone(),
            name: account,
            role: None,
        };
        let at = record.at.unwrap_or_else(now_millis);
        let mut draft = match record.kind {
            OutgoingKind::Message => {
                RecordDraft::message(room.clone(), sender, at, record.elements, false)
            }
            OutgoingKind::Event(event) => {
                RecordDraft::event(room.clone(), sender, at, event, record.elements, false)
            }
        };
        if let Some(platform_id) = record.platform_id {
            draft = draft.with_platform_id(platform_id);
        }
        self.record_and_ack(config, draft)
    }

    /// 写一条「自己发出去的」记录，并对**所有订阅者**确认。
    ///
    /// 必须给所有订阅者确认，不能只确认发消息的这个会话。血的教训（2026-10-31 真机）：
    /// 一个群被两个会话订阅时（dsh 和 dsh-group2），A 会话发的消息只在 A 的水位上确认，
    /// B 的水位上永远留一个洞——而水位只推连续前沿，洞后面的消息就永远算未读，
    /// 表现为「同一批积压反复重投」。
    fn record_and_ack(
        &mut self,
        config: &Config,
        draft: RecordDraft,
    ) -> Result<(Record, u64), BridgeError> {
        let room = draft.room.clone();
        let record = self.store.append(draft)?;
        let mut watermark = 0;
        for subscriber in Self::subscribers_of(config, &room) {
            let cursor = Cursor::new(subscriber.session, room.clone());
            watermark = self.store.ack(&cursor, &[record.id])?;
        }
        Ok((record, watermark))
    }

    /// 两段式发送结算后，endpoint 用同一个口做分页/落盘。
    pub(crate) fn fit_tool_result(&self, tool: &str, value: Value) -> Value {
        render::fit(tool, value, &self.spill_dir())
    }

    /// 把元素里的本机文件收进中继，换成平台取得到的 URL。返回 (原路径, URL) 列表。
    ///
    /// 只动「本机真实存在」的路径：本机没有的路径可能是平台那边才存在的
    /// （共享卷、平台自己的缓存），动它反而弄坏原本能用的写法。
    fn relay_elements(
        &mut self,
        elements: &mut [Element],
    ) -> Result<Vec<(String, String)>, ToolError> {
        let Some(relay) = &self.relay else {
            return Ok(Vec::new());
        };
        let mut relayed = Vec::new();
        for element in elements.iter_mut() {
            let url = match element {
                Element::Image { url, .. } | Element::Audio { url } | Element::Video { url } => url,
                Element::File { url, .. } => url,
                _ => continue,
            };
            let Some(local) = crate::relay::local_path(url) else {
                continue;
            };
            if !local.is_file() {
                continue;
            }
            match relay.publish(&local) {
                Ok(published) => {
                    relayed.push((url.clone(), published.clone()));
                    *url = published;
                }
                Err(error) => {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        format!(
                            "[bad_params] file {} exists locally, but the bridge cannot accept it: \
                             {error}. The platform cannot get this file. Check that [file_relay].dir \
                             is writable; small files (up to 1 MiB after base64) can also be inlined \
                             by writing the url as base64://...",
                            local.display()
                        ),
                    ));
                }
            }
        }
        Ok(relayed)
    }

    /// 把 params 里的本机文件路径换成中继 URL（ADR-0025）。
    ///
    /// 平台调用（upload_file 这类）的文件路径在 params 里，不在元素里，是同一个断层。
    /// 递归遍历，只动「本机真实存在」的绝对路径，其余字符串原样保留。
    fn relay_params(&self, params: serde_json::Value) -> Result<serde_json::Value, ToolError> {
        fn walk(
            relay: &crate::relay::FileRelay,
            value: serde_json::Value,
        ) -> Result<serde_json::Value, ToolError> {
            match value {
                serde_json::Value::String(text) => {
                    let Some(local) = crate::relay::local_path(&text) else {
                        return Ok(serde_json::Value::String(text));
                    };
                    if !local.is_file() {
                        return Ok(serde_json::Value::String(text));
                    }
                    relay
                        .publish(&local)
                        .map(serde_json::Value::String)
                        .map_err(|error| {
                            ToolError::new(
                                ErrorCode::BadRequest,
                                format!(
                                    "[bad_params] file {} in params exists locally, but the bridge \
                                     cannot accept it: {error}. Small files can be inlined with \
                                     base64:// instead.",
                                    local.display()
                                ),
                            )
                        })
                }
                serde_json::Value::Array(items) => items
                    .into_iter()
                    .map(|item| walk(relay, item))
                    .collect::<Result<Vec<_>, _>>()
                    .map(serde_json::Value::Array),
                serde_json::Value::Object(map) => map
                    .into_iter()
                    .map(|(key, value)| walk(relay, value).map(|value| (key, value)))
                    .collect::<Result<serde_json::Map<_, _>, _>>()
                    .map(serde_json::Value::Object),
                other => Ok(other),
            }
        }

        match &self.relay {
            Some(relay) => walk(relay, params),
            None => Ok(params),
        }
    }

    /// 确认一批记录。
    ///
    /// 三条规矩，都是为了让「确认」这件事**幂等**：
    /// 1. 已经确认过的编号（<= 当前水位）当成功跳过。重复确认不是错误——以前它报
    ///    bad_request，模型于是以为「确认失败了」，接着往下不再确认，那几条就被反复重投。
    /// 2. 只能确认「本轮投给过这个会话」或「本轮通过读工具读到过」的编号；别的编号
    ///    报 `not_delivered`。读到但没投递的也能确认，补读才能把水位推着走。
    /// 3. 确认是显式动作，桥不会替 agent 自动推水位。
    fn ack(
        &mut self,
        session: &str,
        room: &RoomId,
        delivered: &BTreeSet<u64>,
        read: &BTreeSet<u64>,
        ids: Vec<u64>,
    ) -> Result<Value, ToolError> {
        if ids.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.engine.bad_params_ids_empty"),
            ));
        }

        let cursor = Cursor::new(session, room.clone());
        let watermark = self.store.watermark(&cursor).map_err(internal)?;

        let mut fresh: Vec<u64> = Vec::new();
        let mut already: Vec<u64> = Vec::new();
        for id in ids {
            if id <= watermark {
                already.push(id);
                continue;
            }
            if !delivered.contains(&id) && !read.contains(&id) {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    format!(
                        "[not_delivered] #{id} was neither delivered to you nor read this turn, so it \
                         cannot be acknowledged. Delivered this turn: {delivered}; read: {read}. \
                         Ids already acknowledged need no further acknowledgement (just skip them).",
                        delivered = delivery_range(delivered),
                        read = if read.is_empty() {
                            "(none)".to_owned()
                        } else {
                            delivery_range(read)
                        }
                    ),
                ));
            }
            fresh.push(id);
        }

        if fresh.is_empty() {
            // 全都是确认过的：这是好消息，不是错误。
            return Ok(json!({
                "ok": true,
                "watermark": watermark,
                "acked": [],
                "already": already,
                "note": "these ids were already acknowledged; the watermark did not move. No need to acknowledge again; continue.",
            }));
        }

        let watermark = self.store.ack(&cursor, &fresh).map_err(internal)?;
        for id in fresh.iter().chain(already.iter()) {
            self.store.forget_delivery(&cursor, *id).map_err(internal)?;
        }

        Ok(json!({
            "ok": true,
            "watermark": watermark,
            "acked": fresh,
            "already": already,
        }))
    }

    fn history(
        &mut self,
        ticket: &Ticket,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: HistoryArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room, turn_room)?;

        let columns = column_set(args.columns.as_deref())?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_HISTORY);
        let mut records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| args.after.is_none_or(|after| record.id > after))
            .filter(|record| args.before.is_none_or(|before| record.id < before))
            .collect();

        // 总数一定要给：不给的话「还有多少条」只能靠猜，猜错就以为翻到底了。
        let total = records.len();
        // 两个方向：
        // - 给了 `after`：从旧往新翻（补看更新的消息），一次给最早的 limit 条；
        // - 没给 `after`：原来的行为，从新往旧翻（看更早的历史）。
        let (has_more, next_before, next_after) = if args.after.is_some() {
            records.truncate(limit);
            let has_more = total > records.len();
            let next_after = if has_more {
                records.last().map(|record| record.id)
            } else {
                None
            };
            (has_more, None, next_after)
        } else {
            let skip = total.saturating_sub(limit);
            records.drain(..skip);
            let has_more = skip > 0;
            // 往更早翻：把 oldest-1 当 before 传回来即可（不用记 offset，也不会因为
            // 中途有新消息进来而错位——新消息都在另一头）。
            let next_before = if has_more {
                records.first().map(|record| record.id)
            } else {
                None
            };
            (has_more, next_before, None)
        };
        // 读到的编号记进本轮 read set：补读过的未投递内容可以直接确认。
        self.mark_read(ticket, &room, records.iter().map(|record| record.id));
        self.fill_quote_refs(&room, &mut records);

        let rendered: Vec<Value> = records
            .iter()
            .map(|r| record_columns(r, &columns))
            .collect();
        Ok(json!({
            "room": room,
            "columns": columns,
            "total": total,
            "returned": rendered.len(),
            "has_more": has_more,
            "next_before": next_before,
            "next_after": next_after,
            "records": rendered,
        }))
    }

    /// 按水位编号精确查一批记录，只返回指定列。
    fn fetch_records(
        &mut self,
        ticket: &Ticket,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: FetchRecordsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room, turn_room)?;

        if args.ids.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.engine.bad_params_ids_empty"),
            ));
        }
        let columns = column_set(args.columns.as_deref())?;

        let wanted: std::collections::BTreeSet<u64> = args.ids.iter().copied().collect();
        let records: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| wanted.contains(&record.id))
            .collect();

        // 查出几条第几条，都算本轮读过：之后 `ack_messages` 可以确认它们。
        self.mark_read(ticket, &room, records.iter().map(|record| record.id));

        // 找出没查到的编号，一起回给调用方，别让 agent 以为「那些 id 不存在」。
        let mut records = records;
        self.fill_quote_refs(&room, &mut records);
        let found: std::collections::BTreeSet<u64> = records.iter().map(|r| r.id).collect();
        let missing: Vec<u64> = wanted.difference(&found).copied().collect();

        let rendered: Vec<Value> = records
            .iter()
            .map(|r| record_columns(r, &columns))
            .collect();
        Ok(json!({
            "room": room,
            "columns": columns,
            "total": wanted.len(),
            "returned": rendered.len(),
            "records": rendered,
            "missing": missing,
        }))
    }

    /// 这组会话订阅的所有房间，按（会话，房间）各列一行。
    ///
    /// 两个会话订同一个房间时会出现两行——这是实话：水位本来就是各记各的。
    fn list_rooms(&self, config: &Config, sessions: &[String]) -> Result<Value, ToolError> {
        let mut rooms = Vec::new();
        for id in sessions {
            let Some(session) = config.session(id) else {
                continue;
            };
            for subscription in &session.subscriptions {
                let room = &subscription.room;
                let account = config.account_of_room(room);
                let cursor = Cursor::new(id.clone(), room.clone());
                let watermark = self.store.watermark(&cursor).map_err(internal)?;
                let unread = self
                    .pending_records(id, room, &account)
                    .map_err(internal)?
                    .len();
                let connection = config.connection(&room.connection);
                rooms.push(json!({
                    "room": room,
                    "session": id,
                    "connection": room.connection,
                    "platform": connection.map(|connection| connection.platform.as_str()).unwrap_or_default(),
                    "kind": connection.map(|connection| connection.kind.as_str()).unwrap_or_default(),
                    "method_count": self.outbound.catalog(&room.connection).len(),
                    "watermark": watermark,
                    "unread": unread,
                }));
            }
        }
        let total = rooms.len();
        Ok(json!({ "rooms": rooms, "total": total }))
    }

    fn read_notes(
        &self,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: NotesArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room, turn_room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => Some(parse_note_kind(text)?),
            None => None,
        };
        let notes: Vec<Note> = self
            .store
            .notes_in(&room)
            .map_err(internal)?
            .into_iter()
            .filter(|note| kind.is_none_or(|wanted| note.key.kind == wanted))
            .collect();

        Ok(json!({
            "room": room,
            "total": notes.len(),
            "notes": notes.iter().map(note_json).collect::<Vec<_>>(),
        }))
    }

    fn write_note(
        &mut self,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: SetNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room, turn_room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let status = match args
            .status
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            Some("open") => Some(TopicStatus::Open),
            Some("closed") => Some(TopicStatus::Closed),
            Some(other) => {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!("kernel.engine.status_invalid", status = other),
                ));
            }
            None => None,
        };
        if status.is_some() && kind != NoteKind::Topic {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.engine.status_topic_only"),
            ));
        }

        let key = NoteKey::new(room.clone(), kind, args.key);
        let note = Note {
            key: key.clone(),
            value: args.value,
            label: args.label,
            status,
            corrected: false,
            updated_at: now_millis(),
        };
        self.store.upsert_note(note).map_err(internal)?;

        Ok(json!({ "ok": true, "kind": kind.as_str(), "key": key.key }))
    }

    fn delete_note(
        &mut self,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: DelNoteArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room, turn_room)?;

        let kind = match args.kind.as_deref() {
            Some(text) => parse_note_kind(text)?,
            None => NoteKind::Term,
        };
        let key = NoteKey::new(room, kind, args.key);
        let removed = self.store.delete_note(&key).map_err(internal)?;
        Ok(json!({ "removed": removed }))
    }

    fn events(
        &mut self,
        ticket: &Ticket,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: EventsArgs,
    ) -> Result<Value, ToolError> {
        let room = self.resolve(&args.room, turn_room)?;
        self.ensure_readable(config, sessions, &room, turn_room)?;

        let limit = args.limit.unwrap_or(DEFAULT_HISTORY).min(MAX_PAGE_LIMIT);
        let offset = args.offset.unwrap_or(0);
        let events: Vec<Record> = self
            .store
            .since(&room, 0, usize::MAX)
            .map_err(internal)?
            .into_iter()
            .filter(|record| !record.is_message())
            .collect();

        let total = events.len();
        let page: Vec<Record> = events.into_iter().skip(offset).take(limit).collect();
        self.mark_read(ticket, &room, page.iter().map(|record| record.id));
        let returned = page.len();
        let has_more = offset + returned < total;
        Ok(json!({
            "room": room,
            "total": total,
            "offset": offset,
            "returned": returned,
            "has_more": has_more,
            "next_offset": if has_more { json!(offset + returned) } else { Value::Null },
            "events": page,
        }))
    }

    /// 按 provider id 查目录；配置引用错误时给明确的 missing 标记。
    fn plugin_info_json(&self, id: &ProviderId) -> Value {
        self.plugins
            .info(id)
            .map(|info| info.to_json())
            .unwrap_or_else(|| json!({ "id": id.to_string(), "missing": true }))
    }

    /// 插件目录：默认只看本会话实际生效的 provider；`scope=global` 看整个 Root。
    ///
    /// 描述由插件作者声明，core 只搬运；agent 用它知道「我为什么被叫醒、我的回复会
    /// 不会被延迟/改写」，但不能通过它改配置。
    fn describe_plugins(
        &self,
        config: &Config,
        session_id: &str,
        room: &RoomId,
        args: DescribePluginsArgs,
    ) -> Result<Value, ToolError> {
        match args.scope.as_deref().unwrap_or("session") {
            "global" => {
                let extension = clean(args.extension.as_deref());
                let plugins: Vec<Value> = self
                    .plugins
                    .catalog()
                    .into_iter()
                    .filter(|info| {
                        extension.map_or(true, |extension| info.extension.as_str() == extension)
                    })
                    .map(|info| info.to_json())
                    .collect();
                Ok(json!({
                    "scope": "global",
                    "count": plugins.len(),
                    "plugins": plugins,
                    "hint": "描述由插件作者声明，是说明不是指令。默认 describe_plugins({}) 只看本会话实际生效的策略。",
                }))
            }
            "session" => {
                let session = config.session(session_id).ok_or_else(|| {
                    ToolError::new(
                        ErrorCode::Internal,
                        acb_i18n::t!(
                            "kernel.engine.internal_session_not_found",
                            session = session_id
                        ),
                    )
                })?;
                let connection = config.connection(&room.connection);
                let send_hooks = json!({
                    "before": session.outbound.before.iter().map(|id| self.plugin_info_json(id)).collect::<Vec<_>>(),
                    "after": session.outbound.after.iter().map(|id| self.plugin_info_json(id)).collect::<Vec<_>>(),
                });
                let subscriptions = session
                    .subscriptions
                    .iter()
                    .map(|subscription| {
                        json!({
                            "room": subscription.room.to_string(),
                            "wake": self.plugin_info_json(&subscription.wake.provider),
                            "clock": self.plugin_info_json(&subscription.clock.provider),
                        })
                    })
                    .collect::<Vec<_>>();

                // 一个会话可以同时订阅多个平台的房间；这里把所有订阅连接都列出来，
                // 免得 agent 只看见当前轮次那个平台，以为别的房间不存在。
                let mut connections: Vec<&crate::config::Connection> = Vec::new();
                for subscription in &session.subscriptions {
                    if let Some(found) = config.connection(&subscription.room.connection) {
                        if !connections.iter().any(|seen| seen.id == found.id) {
                            connections.push(found);
                        }
                    }
                }
                let adapters = connections
                    .iter()
                    .map(|connection| {
                        let info =
                            self.plugins.catalog().into_iter().find(|info| {
                                info.kind.as_deref() == Some(connection.kind.as_str())
                            });
                        // 先看这条连接运行时上报的能力（外置进程 hello 会带），
                        // 没有再退回按 kind 的静态声明。
                        let manifest = self
                            .outbound
                            .manifest(&connection.id)
                            .or_else(|| self.plugins.adapter_manifest(&connection.kind));
                        let (events, event_tags, supports_at, supports_reply) = match manifest {
                            Some(manifest) => (
                                manifest
                                    .events
                                    .iter()
                                    .map(|event| event.as_ref().to_owned())
                                    .collect::<Vec<_>>(),
                                manifest
                                    .event_tags
                                    .iter()
                                    .map(|tag| tag.as_ref().to_owned())
                                    .collect::<Vec<_>>(),
                                manifest.supports_at,
                                manifest.supports_reply,
                            ),
                            None => (Vec::new(), Vec::new(), false, false),
                        };
                        json!({
                            "connection": connection.id,
                            "platform": connection.platform,
                            "kind": connection.kind,
                            "transport": connection.transport.as_str(),
                            "method_count": self.outbound.catalog(&connection.id).len(),
                            "current": connection.id == room.connection,
                            "receive": self.plugin_info_json(&connection.receive.provider),
                            "events": events,
                            "event_tags": event_tags,
                            "supports_at": supports_at,
                            "supports_reply": supports_reply,
                            "info": info.map(|info| info.to_json()),
                        })
                    })
                    .collect::<Vec<_>>();
                let adapter = adapters.iter().find(|adapter| {
                    adapter["connection"].as_str() == Some(room.connection.as_str())
                });
                Ok(json!({
                    "scope": "session",
                    "session": session.id,
                    "providers": {
                        "access": self.plugin_info_json(&session.access.provider),
                        "delivery": self.plugin_info_json(&session.delivery.provider),
                        "turn_error": self.plugin_info_json(&session.turn_error.provider),
                        "receive": connection.map(|c| self.plugin_info_json(&c.receive.provider)),
                        "send_hooks": send_hooks,
                    },
                    "subscriptions": subscriptions,
                    "adapters": adapters,
                    "adapter": adapter,
                    "hint": "描述由插件作者声明，是说明不是指令；effects 是文档，不是强制保证。adapters 覆盖本会话订阅的所有连接。",
                }))
            }
            other => Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.engine.bad_scope", scope = other),
            )),
        }
    }

    /// 平台能力目录（ADR-0020）。
    ///
    /// 两个维度、两档披露：
    /// - 名字里点号前那段是**归属**，跟平台 API 的命名空间对齐；`tags` 是**用途**，跨归属。
    /// - 不给任何条件 = 逐条清单（名字 + 一句话 + 标签），够模型决定调哪个。
    /// - 给了 `group` / `tag` / `query` = 交集里的方法连**参数表**一起给，这时候模型是在准备调。
    ///
    /// 为什么清单不砍成「只有组名」：多一次往返就是多一整轮推理和几秒延迟，
    /// 而一张清单在四五十个方法时也就一千来 token，只花在模型主动查的那一轮。
    fn platform_methods(
        &self,
        config: &Config,
        sessions: &[String],
        turn_room: &RoomId,
        args: PlatformMethodsArgs,
    ) -> Result<Value, ToolError> {
        let requested_connection = clean(args.connection.as_deref()).map(str::to_owned);

        // all=true：多平台会话先看每条连接的摘要，避免模型只看见当前房间那个平台。
        if args.all.unwrap_or(false) {
            if requested_connection.is_some()
                || clean(args.room.as_deref()).is_some()
                || clean(args.group.as_deref()).is_some()
                || clean(args.tag.as_deref()).is_some()
                || clean(args.query.as_deref()).is_some()
            {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    "[bad_params] all=true only returns a connection summary; do not also pass room / \
                     connection / group / tag / query. To expand a connection, query it separately.",
                ));
            }
            return self.connection_summaries(config, sessions, turn_room);
        }

        let given_room = if clean(args.room.as_deref()).is_some() {
            Some(self.resolve(&args.room, turn_room)?)
        } else {
            None
        };

        // 目录本身就是连接级的东西：同一个号的哪个房间，能做的事都一样。
        // 所以这里不要求模型报出一个具体房间，只要它能访问这条连接就行。
        let connection = match requested_connection {
            Some(connection) => {
                if !config
                    .connections
                    .iter()
                    .any(|candidate| candidate.id == connection)
                {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!(
                            "kernel.engine.bad_params_no_connection",
                            connection = connection
                        ),
                    ));
                }
                if let Some(room) = &given_room {
                    if room.connection != connection {
                        return Err(ToolError::new(
                            ErrorCode::BadRequest,
                            acb_i18n::t!(
                                "kernel.engine.bad_params_room_not_on_connection",
                                room = room,
                                connection = connection
                            ),
                        ));
                    }
                }
                self.ensure_connection_readable(config, sessions, &connection, turn_room)?;
                connection
            }
            None => match &given_room {
                Some(room) => {
                    self.ensure_connection_readable(config, sessions, &room.connection, turn_room)?;
                    room.connection.clone()
                }
                None => {
                    self.ensure_connection_readable(
                        config,
                        sessions,
                        &turn_room.connection,
                        turn_room,
                    )?;
                    turn_room.connection.clone()
                }
            },
        };

        let room_text = given_room
            .map(|room| room.to_string())
            .or_else(|| (turn_room.connection == connection).then(|| turn_room.to_string()));

        let methods = self.outbound.catalog(&connection);
        if methods.is_empty() {
            return Err(no_catalog(&connection));
        }
        let platform = Self::platform_of(config, &connection).to_owned();

        let group = clean(args.group.as_deref());
        let tag = clean(args.tag.as_deref());
        let query = clean(args.query.as_deref());

        // 没设条件：清单。不带参数表——那样就等于把整张表倒出来。
        if group.is_none() && tag.is_none() && query.is_none() {
            let list: Vec<Value> = methods.iter().map(describe_brief).collect();
            return Ok(json!({
                "room": room_text,
                "connection": connection,
                "platform": platform,
                "total": methods.len(),
                "returned": list.len(),
                "methods": list,
                "tags": tags_of(&methods),
                "hint": "名字里点号前那段是归属（跟平台 API 命名空间对齐）；tags 是用途，跨归属。\
                         要某个方法的参数表，再调一次并给 group / tag / query，三者可叠加。\
                         scope=connection 的方法跟房间无关（好友列表、账号资料这类），调它不用给 room。",
            }));
        }

        // 缩了范围：命中的方法连参数表一起给。
        let terms = query.as_deref().map(expand_query).unwrap_or_default();
        let hits: Vec<&MethodSpec> = methods
            .iter()
            .filter(|method| group.map_or(true, |group| method.group() == group))
            .filter(|method| tag.map_or(true, |tag| method.has_tag(tag)))
            .filter(|method| terms.is_empty() || terms.iter().any(|term| method.matches(term)))
            .collect();

        if hits.is_empty() {
            return Err(no_match(&methods, group, tag, query.as_deref()));
        }

        Ok(json!({
            "room": room_text,
            "connection": connection,
            "platform": platform,
            "total": methods.len(),
            "matched": hits.len(),
            "methods": hits.iter().map(|method| describe_method(method)).collect::<Vec<_>>(),
            "hint": "每条都带一个 example：照它把 params 里的号/内容换掉，就能直接喂给 platform_call。",
        }))
    }

    /// 多平台会话的连接摘要：每条订阅连接一条，带平台、kind、方法数和事件能力。
    ///
    /// 只做发现，不倒方法表——方法表按连接查，免得一次把几个平台的目录都塞进上下文。
    fn connection_summaries(
        &self,
        config: &Config,
        sessions: &[String],
        origin: &RoomId,
    ) -> Result<Value, ToolError> {
        let mut ids: Vec<String> = Vec::new();
        for id in sessions {
            let Some(session) = config.session(id) else {
                continue;
            };
            for subscription in &session.subscriptions {
                if !ids.contains(&subscription.room.connection) {
                    ids.push(subscription.room.connection.clone());
                }
            }
        }

        let mut connections = Vec::new();
        for id in ids {
            let Some(connection) = config.connection(&id) else {
                continue;
            };
            self.ensure_connection_readable(config, sessions, &id, origin)?;
            let methods = self.outbound.catalog(&id);
            // 同 describe_plugins：连接运行时 manifest 优先，静态声明兜底。
            let manifest = self
                .outbound
                .manifest(&id)
                .or_else(|| self.plugins.adapter_manifest(&connection.kind));
            let (events, event_tags, supports_at, supports_reply) = match manifest {
                Some(manifest) => (
                    manifest
                        .events
                        .iter()
                        .map(|event| event.as_ref().to_owned())
                        .collect::<Vec<_>>(),
                    manifest
                        .event_tags
                        .iter()
                        .map(|tag| tag.as_ref().to_owned())
                        .collect::<Vec<_>>(),
                    manifest.supports_at,
                    manifest.supports_reply,
                ),
                None => (Vec::new(), Vec::new(), false, false),
            };
            connections.push(json!({
                "connection": connection.id,
                "platform": connection.platform,
                "kind": connection.kind,
                "transport": connection.transport.as_str(),
                "method_count": methods.len(),
                "tags": tags_of(&methods),
                "events": events,
                "event_tags": event_tags,
                "supports_at": supports_at,
                "supports_reply": supports_reply,
            }));
        }

        let total = connections.len();
        Ok(json!({
            "scope": "connections",
            "connections": connections,
            "total": total,
            "hint": "先看每条连接的摘要，再带 connection 查具体方法；events 是适配器声明会产生的平台事件，event_tags 是跨平台语义标签。",
        }))
    }

    /// 记一条反馈（ADR-0021）。
    ///
    /// 取向是**写建议，不是填表**：正文一个字段，想说多少说多少（上限
    /// [`MAX_FEEDBACK_BODY`] 字，超了截断并告知）；出处、次数、类型都可以不填。
    /// 桥自己会补现场（会话、harness、房间、模型、这一轮还在不在跑），
    /// agent 不用把「我是谁、我在哪」再抄一遍。
    ///
    /// 为什么要这个工具：agent 卡住时的唯一出路曾经是「自己想办法」——有的直接绕过桥
    /// 去戳平台端口。那种绕道没有订阅校验、没有审计，也没人知道它撞了什么墙。
    /// 给一个正式的出口，绕道才会自己消失。
    fn report_feedback(
        &mut self,
        ticket: &Ticket,
        session: &str,
        room: &RoomId,
        live: bool,
        args: FeedbackArgs,
    ) -> Result<Value, ToolError> {
        // 同上：dsh 的解析器实测会把整段 `<invoke>`（含后面的参数标签）塞进 text。
        // 不切的话，反馈列表上看到的就是「正文 + 一串标签」，人根本没法读。
        let (body, markup_trimmed) = crate::sanitize::trim_call_markup(args.text.trim());
        if body.is_empty() {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                if markup_trimmed {
                    "[bad_params] your entire text consists of call markup (`</text>` etc.); nothing \
                     remains after stripping it. Describe what blocked you in the body, and put \
                     arguments in their own fields."
                } else {
                    "[bad_params] text must not be empty; describe what blocked you in a sentence or two."
                },
            ));
        }

        let kind = match args.kind.as_deref().map(str::trim) {
            None | Some("") => "friction",
            Some(kind) => {
                let kind = kind.to_ascii_lowercase();
                if !FEEDBACK_KINDS.contains(&kind.as_str()) {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!(
                            "kernel.engine.feedback_kind_invalid",
                            kinds = FEEDBACK_KINDS.join(" / "),
                            kind = kind
                        ),
                    ));
                }
                // 字符串要活得比函数长：转成静态表里的那个值。
                FEEDBACK_KINDS
                    .iter()
                    .find(|known| **known == kind)
                    .copied()
                    .unwrap_or("friction")
            }
        };

        let severity = match args.severity.as_deref().map(str::trim) {
            None | Some("") => default_severity(kind),
            Some(severity) => {
                let severity = severity.to_ascii_lowercase();
                if !FEEDBACK_SEVERITIES.contains(&severity.as_str()) {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!(
                            "kernel.engine.feedback_severity_invalid",
                            severities = FEEDBACK_SEVERITIES.join(" / "),
                            severity = severity
                        ),
                    ));
                }
                FEEDBACK_SEVERITIES
                    .iter()
                    .find(|known| **known == severity)
                    .copied()
                    .unwrap_or("major")
            }
        };

        // 第一条非空行当标题，剩下的当正文。想写日记也行，列表里只显示第一行。
        let (title_raw, _) = split_first_line(&body);
        let (title, title_clipped) = clip(title_raw, MAX_FEEDBACK_TITLE);
        let (body, body_clipped) = clip(&body, MAX_FEEDBACK_BODY);
        let suggestion = args
            .suggestion
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty());
        let (suggestion, suggestion_clipped) = match suggestion {
            Some(text) => {
                let (clipped, cut) = clip(text, MAX_FEEDBACK_SUGGESTION);
                (Some(clipped), cut)
            }
            None => (None, false),
        };

        // 出处：清掉空串、限个数、限长度。这里不校验「这个编号存不存在」——
        // 有时候最该记下来的正是「那个编号查不到」。
        let mut refs: Vec<String> = Vec::new();
        for item in args.refs.iter().take(MAX_FEEDBACK_REFS) {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (item, _) = clip(item, MAX_FEEDBACK_REF_CHARS);
            if !refs.contains(&item) {
                refs.push(item);
            }
        }
        let refs_dropped = args.refs.len().saturating_sub(MAX_FEEDBACK_REFS);

        let count = args.count.unwrap_or(1).clamp(1, MAX_FEEDBACK_OCCURRENCES);

        let harness = self
            .config
            .session(session)
            .map(|session| session.harness.clone());
        let model = self
            .config
            .session(session)
            .and_then(|session| session.model.clone());
        let at = now_millis();
        let log_title = title.clone();

        let draft = crate::storage::FeedbackDraft {
            fingerprint: feedback_fingerprint(&title),
            title,
            body,
            suggestion,
            kind: kind.to_owned(),
            severity: severity.to_owned(),
            refs,
            occurrences: count,
            tool: args
                .tool
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            session: session.to_owned(),
            harness,
            room: Some(room.to_string()),
            context: json!({
                "ticket": ticket.as_str(),
                "model": model,
                "turn_running": live,
                "at": at,
            }),
            at,
        };

        let recorded = self.store.record_feedback(draft).map_err(internal)?;

        // 也打进桥日志：一份被人看见的反馈，不该只躺在库里等人去查表。
        eprintln!(
            "{}",
            acb_i18n::t!(
                "kernel.engine.feedback_log",
                id = recorded.id,
                kind = kind,
                severity = severity,
                session = session,
                title = log_title,
                occurrences = recorded.occurrences,
                repeats = recorded.repeats
            )
        );

        let mut truncated = serde_json::Map::new();
        if title_clipped {
            truncated.insert("title".into(), json!(MAX_FEEDBACK_TITLE));
        }
        if body_clipped {
            truncated.insert("text".into(), json!(MAX_FEEDBACK_BODY));
        }
        if suggestion_clipped {
            truncated.insert("suggestion".into(), json!(MAX_FEEDBACK_SUGGESTION));
        }
        if markup_trimmed {
            // 单独标一个键：这不是「写太长被截」，是「参数写错了地方」。
            truncated.insert("text_markup".into(), json!(true));
        }

        let mut note = if recorded.duplicate {
            "这条已经提过（同样的标题），次数已累加，不用再提。接着干活就行。".to_owned()
        } else {
            "记下了。这是出口，不是许愿池：写清「在哪一步卡住」比写「希望支持 X」有用得多。"
                .to_owned()
        };
        if markup_trimmed {
            note.push_str(
                " 另外：你的 text 里混着一串调用标记（`</text>` 之类），已经在第一个标记处截断——\
                 那是解析工具调用时漏进来的，不是正文。参数请放在各自的字段里，别写进 text。",
            );
        }

        Ok(json!({
            "ok": true,
            "id": recorded.id,
            "duplicate": recorded.duplicate,
            "repeats": recorded.repeats,
            "occurrences": recorded.occurrences,
            "truncated": truncated,
            "refs_dropped": refs_dropped,
            "note": note,
        }))
    }

    /// 连接名 → 平台名。只给展示用，查不到就说 unknown，不要因此拒绝服务。
    fn platform_of<'a>(config: &'a Config, connection: &str) -> &'a str {
        config
            .connections
            .iter()
            .find(|item| item.id == connection)
            .map(|item| item.platform.as_str())
            .unwrap_or("unknown")
    }

    /// 这条连接的配置放不放行这个破坏性方法。查不到连接 = 不放行。
    fn allows_destructive(config: &Config, connection: &str, method: &str) -> bool {
        config
            .connections
            .iter()
            .find(|item| item.id == connection)
            .is_some_and(|item| item.allow_destructive.allows(method))
    }

    /// 参数里没给房间就用本轮的房间；给了就解析。
    fn resolve(&self, given: &Option<String>, turn_room: &RoomId) -> Result<RoomId, ToolError> {
        match given {
            Some(text) => resolve_room(text),
            None => Ok(turn_room.clone()),
        }
    }

    // ---- 记忆工具：文件层 ----

    /// 定位当前会话在 `memory` 上的 worktree。校验「这个会话确实引用了该记忆」，
    /// 并惰性初始化 worktree。
    fn memory_worktree(
        &self,
        config: &Config,
        session: &str,
        memory: &str,
    ) -> Result<std::path::PathBuf, ToolError> {
        let Some(session_cfg) = config.session(session) else {
            return Err(ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.engine.session_not_found", session = session),
            ));
        };
        self.memory
            .resolve_for(session, memory, &session_cfg.memories, &|memory_id| {
                config.memory(memory_id).map(|m| m.location.clone())
            })
    }

    fn memory_read(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let content = read_file(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "content": content }))
    }

    fn memory_write(
        &self,
        config: &Config,
        session: &str,
        args: MemoryWriteArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        write_file(&worktree, &args.path, &args.content)?;
        Ok(json!({ "path": args.path, "written": true }))
    }

    fn memory_list(
        &self,
        config: &Config,
        session: &str,
        args: MemoryListArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let entries = list_dir(&worktree, &args.path)?;
        let total = entries.len();
        let offset = args.offset.unwrap_or(0);
        let limit = args
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
        let page: Vec<String> = entries.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + page.len() < total;
        Ok(json!({
            "path": args.path,
            "total": total,
            "offset": offset,
            "returned": page.len(),
            "has_more": has_more,
            "next_offset": if has_more { json!(offset + page.len()) } else { Value::Null },
            "entries": page,
        }))
    }

    fn memory_search(
        &self,
        config: &Config,
        session: &str,
        args: MemorySearchArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let hits = search(&worktree, &args.path, &args.needle)?;
        let total = hits.len();
        let offset = args.offset.unwrap_or(0);
        let limit = args
            .limit
            .unwrap_or(DEFAULT_PAGE_LIMIT)
            .clamp(1, MAX_PAGE_LIMIT);
        let page: Vec<String> = hits.into_iter().skip(offset).take(limit).collect();
        let has_more = offset + page.len() < total;
        Ok(json!({
            "needle": args.needle,
            "total": total,
            "offset": offset,
            "returned": page.len(),
            "has_more": has_more,
            "next_offset": if has_more { json!(offset + page.len()) } else { Value::Null },
            "hits": page,
        }))
    }

    // ---- 记忆工具：git 层 ----

    fn memory_git_status(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let status = git_status(&worktree)?;
        Ok(json!({ "status": status }))
    }

    fn memory_git_log(
        &self,
        config: &Config,
        session: &str,
        args: MemoryLogArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let log = git_log(&worktree, args.limit.unwrap_or(10));
        Ok(json!({ "log": log }))
    }

    fn memory_git_diff(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let diff = git_diff(&worktree)?;
        Ok(json!({ "diff": diff }))
    }

    fn memory_git_blame(
        &self,
        config: &Config,
        session: &str,
        args: MemoryBlameArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let blame = git_blame(&worktree, &args.path)?;
        Ok(json!({ "path": args.path, "blame": blame }))
    }

    fn memory_git_add(
        &self,
        config: &Config,
        session: &str,
        args: MemoryAddArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        git_add(&worktree, &args.path)?;
        Ok(json!({ "staged": if args.path.is_empty() { "全部" } else { &args.path } }))
    }

    fn memory_git_commit(
        &self,
        config: &Config,
        session: &str,
        args: MemoryCommitArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let result = git_commit(&worktree, &args.message)?;
        Ok(json!({ "result": result }))
    }

    fn memory_git_push(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let result = git_push(&worktree)?;
        Ok(json!({ "result": result }))
    }

    fn memory_git_branch(
        &self,
        config: &Config,
        session: &str,
        args: MemoryArgs,
    ) -> Result<Value, ToolError> {
        let worktree = self.memory_worktree(config, session, &args.memory)?;
        let branches = git_branch(&worktree)?;
        Ok(json!({ "branches": branches }))
    }
}

/// 「本轮投给你的是哪一段」——错误信息里要能直接照抄的答案。
fn delivery_range(delivered: &BTreeSet<u64>) -> String {
    match (delivered.iter().next(), delivered.iter().next_back()) {
        (Some(first), Some(last)) if first == last => format!("#{first}"),
        (Some(first), Some(last)) => acb_i18n::t!(
            "kernel.engine.delivery_range",
            first = first,
            last = last,
            count = delivered.len()
        ),
        _ => acb_i18n::t!("kernel.engine.delivery_range_empty"),
    }
}

/// 票用不了了。**说清「这一轮不会再有工具可用」**——这是让 agent 收手的关键一句。
///
/// 以前这里只有「这张票无效或本轮已结束」：模型分不清是自己调错了、还是桥坏了，
/// 于是继续试别的工具，一路试到轮次真的结束。错误文案本身就是接口的一部分。
fn no_active_round() -> ToolError {
    ToolError::new(
        ErrorCode::Unauthorized,
        "[no_active_round] the turn for this ticket has ended; **no tools are available for this \
         turn - end the turn instead of retrying**. Unacknowledged messages are still there and will \
         be delivered again as unread next turn (same ids); acknowledge them then. Only a few \
         things remain possible: late acknowledgement within 60 seconds (ack_messages), a late \
         send within 60 seconds (send_message), and reporting feedback at any time (report_feedback).",
    )
}

fn resolve_room(text: &str) -> Result<RoomId, ToolError> {
    RoomId::parse(text).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!("kernel.engine.resolve_room", room = text, error = error),
        )
    })
}

/// 校验/归一列名，返回按文档顺序排好的列名列表。
///
/// 没指定（`None`）就给默认精简子集；指定了但含非法列名就报错，并把全部合法列名
/// 列出来，让 agent 能照着改。
fn column_set(given: Option<&[String]>) -> Result<Vec<&'static str>, ToolError> {
    let Some(given) = given else {
        return Ok(DEFAULT_COLUMNS.to_vec());
    };
    let mut columns: Vec<&'static str> = Vec::with_capacity(given.len());
    for name in given {
        let name = name.trim();
        match RECORD_COLUMNS.iter().find(|known| **known == name) {
            Some(known) => {
                if !columns.contains(known) {
                    columns.push(known);
                }
            }
            None => {
                return Err(ToolError::new(
                    ErrorCode::BadRequest,
                    acb_i18n::t!(
                        "kernel.engine.unknown_column",
                        column = name,
                        columns = RECORD_COLUMNS.join(", ")
                    ),
                ));
            }
        }
    }
    if columns.is_empty() {
        return Ok(DEFAULT_COLUMNS.to_vec());
    }
    Ok(columns)
}

/// 把一条记录按指定列序列化，并附带「每列取没取」的状态。
///
/// 返回形如 `{ "id": 237, "columns": { "id": true, "platform_id": false } }`。
/// 这样 agent 能一眼看出哪列是它要的、哪列没取，不会把「没取」误当「没有」。
///
/// 状态值是布尔，不是描述文字：「查了 / 没查」这种中文值只能给人看，机器要判断得再猜一遍。
fn record_columns(record: &Record, columns: &[&str]) -> Value {
    let mut status = serde_json::Map::new();
    let mut out = serde_json::Map::new();
    for column in RECORD_COLUMNS {
        let taken = columns.contains(column);
        status.insert((*column).to_owned(), json!(taken));
        if !taken {
            continue;
        }
        let value = match *column {
            "id" => json!(record.id),
            "room" => json!(record.room),
            "kind" => match &record.kind {
                RecordKind::Message => json!("message"),
                RecordKind::Event {
                    event,
                    tags,
                    payload,
                    summary,
                    body,
                } => json!({
                    "event": event,
                    "tags": tags,
                    "payload": payload,
                    "summary": summary,
                    "body": body,
                }),
            },
            "sender" => json!(record.sender),
            "at" => json!(record.at),
            "elements" => json!(record.elements),
            "platform_id" => json!(record.platform_id),
            "quote_ref" => json!(record.quote_ref),
            "at_self" => json!(record.at_self),
            "reply_to_self" => json!(record.reply_to_self),
            "labels" => json!(record.labels),
            _ => continue,
        };
        out.insert((*column).to_owned(), value);
    }
    out.insert("columns".to_owned(), Value::Object(status));
    Value::Object(out)
}

fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, ToolError> {
    serde_json::from_value(args).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!("kernel.engine.invalid_params", error = error),
        )
    })
}

fn parse_note_kind(text: &str) -> Result<NoteKind, ToolError> {
    NoteKind::parse(text).ok_or_else(|| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!("kernel.engine.note_kind_invalid", kind = text),
        )
    })
}

fn note_json(note: &Note) -> Value {
    json!({
        "kind": note.key.kind.as_str(),
        "key": note.key.key,
        "value": note.value,
        "label": note.label,
        "status": note.status.map(|status| match status {
            TopicStatus::Open => "open",
            TopicStatus::Closed => "closed",
        }),
        "corrected": note.corrected,
        "updated_at": note.updated_at,
    })
}

fn task_json(record: &TaskRecord) -> Value {
    json!({
        "id": record.id,
        "session": record.session,
        "room": record.room.to_string(),
        "handler": record.handler,
        "params_summary": record.params_summary,
        "status": record.status.as_str(),
        "created_at": record.created_at,
        "started_at": record.started_at,
        "finished_at": record.finished_at,
        "result": record.result,
        "error": record.error,
        "delivery_error": record.delivery_error,
    })
}

/// 任务参数摘要：只留够识别任务的一小段，避免把命令环境/长 JSON 原样塞回模型。
fn summarize_params(params: &Value) -> String {
    if let Some(command) = params.get("command").and_then(Value::as_array) {
        let joined = command
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| item.to_string())
            })
            .collect::<Vec<_>>()
            .join(" ");
        return truncate_chars(&joined, 200);
    }
    if let Some(send) = params.get("send") {
        return truncate_chars(&format!("send={send}"), 200);
    }
    truncate_chars(
        &serde_json::to_string(params).unwrap_or_else(|_| "{}".to_owned()),
        200,
    )
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = text.chars().take(max).collect::<String>();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn internal(error: impl std::fmt::Display) -> ToolError {
    ToolError::new(ErrorCode::Internal, error.to_string())
}

fn outbound_error(error: OutboundError) -> ToolError {
    match error {
        OutboundError::Rejected(detail) => {
            ToolError::new(ErrorCode::Internal, with_file_hint(detail))
        }
        OutboundError::Unsupported(detail) => {
            ToolError::new(ErrorCode::BadRequest, with_file_hint(detail))
        }
        OutboundError::Unavailable(detail) => {
            ToolError::new(ErrorCode::Internal, with_file_hint(detail))
        }
        OutboundError::Timeout(detail) => {
            ToolError::new(ErrorCode::Timeout, with_file_hint(detail))
        }
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 只把文本段拼起来：命令判定不该被 @ / 引用 / 图片占位符干扰。
fn command_text(elements: &[Element]) -> String {
    elements
        .iter()
        .filter_map(Element::as_text)
        .collect::<String>()
}

// ── 反馈（ADR-0021）────────────────────────────────────────────────────

/// 反馈的类型。四种够表达「哪里不对」，再多就是让人纠结了。
const FEEDBACK_KINDS: &[&str] = &["bug", "gap", "friction", "idea"];

/// 影响面。三者足够排序：干不下去 / 很烦 / 小毛病。
const FEEDBACK_SEVERITIES: &[&str] = &["blocker", "major", "minor"];

/// 正文上限（字符）。4000 字大约能装一次完整的失败对话 + 复现步骤。
///
/// 为什么有上限而不是不限：一条反馈是给人看的，超过四千字人就不会看了；
/// 真想长篇大论，写进文件再贴路径（refs 里放路径）。
const MAX_FEEDBACK_BODY: usize = 4000;

/// 标题上限（字符）：列表里一行扫过去的东西。
const MAX_FEEDBACK_TITLE: usize = 120;

/// 建议上限（字符）。
const MAX_FEEDBACK_SUGGESTION: usize = 1000;

/// 出处最多记多少条。
const MAX_FEEDBACK_REFS: usize = 20;

/// 单条出处的长度上限（字符）。
const MAX_FEEDBACK_REF_CHARS: usize = 120;

/// 自称发生次数的上限。比这更大就不像「撞了几次」，像在灌数。
const MAX_FEEDBACK_OCCURRENCES: u32 = 100_000;

/// 没写 severity 时按类型推：坏了或缺能力默认 major，别扭和愿望默认 minor。
fn default_severity(kind: &str) -> &'static str {
    match kind {
        "bug" | "gap" => "major",
        _ => "minor",
    }
}

/// 第一行（标题），其余（正文）。空行跳过。
fn split_first_line(text: &str) -> (&str, &str) {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let rest = &text[offset + line.len()..];
            return (trimmed, rest.trim());
        }
        offset += line.len();
    }
    (text.trim(), "")
}

/// 按字符截断（不是字节，别把中文劈成半个）。返回（截断后的文本, 是否截过）。
fn clip(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_owned(), false);
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    (out, true)
}

/// 去重指纹：归一化标题的 FNV-1a。
///
/// 只认标题，不认类型：同一个坑这次报 gap、下次报 bug，还是同一个坑。
/// 合并时保留第一次的类型（人已经看过那一行，不该被后来的报告悄悄改分类）。
///
/// 不引哈希库：这个东西只用来「同一条别再建一行」，不需要抗碰撞强度——
/// 撞了最多是两条不同的反馈合并成一条（次数加起来），不会丢内容也不会出错。
fn feedback_fingerprint(title: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in title
        .to_lowercase()
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

// ── 平台能力（ADR-0020）────────────────────────────────────────────────

fn no_catalog(connection: &str) -> ToolError {
    ToolError::new(
        ErrorCode::ToolNotAllowed,
        acb_i18n::t!(
            "kernel.engine.unsupported_platform",
            connection = connection
        ),
    )
}

/// 空串当没给：模型有时会把「我不填」写成空字符串，别让它变成一次必然落空的过滤。
fn clean(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|text| !text.is_empty())
}

/// 清单档的一条：够模型判断「这是我要的」，但不带参数表。
fn describe_brief(spec: &MethodSpec) -> Value {
    let mut item = json!({
        "name": spec.name,
        "summary": spec.summary,
        "tags": spec.tags,
        "risk": spec.risk.as_str(),
    });
    // 房间级是默认，不写；只有「跟房间无关」的那些才标一下，
    // 顺便告诉模型这些方法不用为了它编一个房间出来。五十来行清单里只多十处。
    if spec.scope == Scope::Connection {
        item["scope"] = json!(spec.scope.as_str());
    }
    item
}

/// 详情档的一条：多一张参数表（JSON Schema，模型最熟的形式）+ 一个能照抄的样例。
///
/// 参数表说「该填什么」，样例说「填好长这样」。只给前者，模型还得把 Schema 翻译成
/// 一次调用；翻译错就要多试一轮，而试错正是反馈里最多的摩擦。
fn describe_method(spec: &MethodSpec) -> Value {
    json!({
        "name": spec.name,
        "summary": spec.summary,
        "tags": spec.tags,
        "risk": spec.risk.as_str(),
        "effect": spec.effect.as_str(),
        "scope": spec.scope.as_str(),
        "params": spec.params_schema(),
        "example": spec.example_call(),
    })
}

/// 这台连接用到的标签，按首次出现的顺序。
fn tag_names<'a>(methods: &'a [MethodSpec]) -> Vec<&'a str> {
    let mut names: Vec<&'a str> = Vec::new();
    for method in methods {
        for tag in method.tags.iter() {
            let tag = tag.as_ref();
            if !names.contains(&tag) {
                names.push(tag);
            }
        }
    }
    names
}

/// 标签概览，带各几个方法。
fn tags_of(methods: &[MethodSpec]) -> Vec<Value> {
    tag_names(methods)
        .into_iter()
        .map(|tag| {
            json!({
                "tag": tag,
                "count": methods.iter().filter(|method| method.has_tag(tag)).count(),
            })
        })
        .collect()
}

/// 过滤后一个都没命中：说清楚条件是什么、手上有哪些可选项。
///
/// 这类错只有一种正确写法——带上可选项，让模型一轮就改对，而不是换个词再试三次。
fn no_match(
    methods: &[MethodSpec],
    group: Option<&str>,
    tag: Option<&str>,
    query: Option<&str>,
) -> ToolError {
    let mut given = Vec::new();
    if let Some(group) = group {
        given.push(format!("group={group}"));
    }
    if let Some(tag) = tag {
        given.push(format!("tag={tag}"));
    }
    if let Some(query) = query {
        given.push(format!("query={query}"));
    }

    let mut hint = acb_i18n::t!(
        "kernel.engine.no_match_groups",
        groups = groups_of(methods).join(" / ")
    );
    let known_tags = tag_names(methods);
    if !known_tags.is_empty() {
        hint.push_str(&acb_i18n::t!(
            "kernel.engine.no_match_tags",
            tags = known_tags.join(" / ")
        ));
    }

    ToolError::new(
        ErrorCode::BadRequest,
        acb_i18n::t!(
            "kernel.engine.no_match",
            filters = given.join(" + "),
            hint = hint
        ),
    )
}

fn method_names(methods: &[MethodSpec]) -> String {
    methods
        .iter()
        .map(|method| method.name.as_ref())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 有哪些分组，按声明顺序（不是字母序）：适配器写得顺，模型看得也顺。
fn groups_of<'a>(methods: &'a [MethodSpec]) -> Vec<&'a str> {
    let mut groups: Vec<&'a str> = Vec::new();
    for method in methods {
        let group = method.group();
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    groups
}

fn group_of_name(method: &str) -> &str {
    method.split('.').next().unwrap_or(method)
}

/// 调之前把参数形状核一遍。
///
/// 未声明的参数也拒：既是白名单精神，也能帮模型发现字段名拼错了——平台那边回的错通常看不懂。
fn check_params(spec: &MethodSpec, params: &Value) -> Result<(), ToolError> {
    let empty = serde_json::Map::new();
    let given = match params {
        Value::Null => &empty,
        Value::Object(map) => map,
        _ => {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.engine.params_not_object", method = spec.name),
            ));
        }
    };

    for param in spec.params.iter() {
        match given.get(param.name.as_ref()) {
            None | Some(Value::Null) => {
                if param.required {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!(
                            "kernel.engine.params_missing_required",
                            method = spec.name,
                            parameter = param.name,
                            desc = param.desc
                        ),
                    ));
                }
            }
            Some(value) => {
                if !param.ty.matches(value) {
                    return Err(ToolError::new(
                        ErrorCode::BadRequest,
                        acb_i18n::t!(
                            "kernel.engine.params_type_mismatch",
                            method = spec.name,
                            parameter = param.name,
                            expected = param.ty.json_type(),
                            value = value
                        ),
                    ));
                }
                if !param.values.is_empty() {
                    let allowed = value.as_str().is_some_and(|text| {
                        param
                            .values
                            .iter()
                            .any(|candidate| candidate.as_ref() == text)
                    });
                    if !allowed {
                        let choices = param
                            .values
                            .iter()
                            .map(|candidate| candidate.as_ref())
                            .collect::<Vec<_>>()
                            .join(" / ");
                        return Err(ToolError::new(
                            ErrorCode::BadRequest,
                            acb_i18n::t!(
                                "kernel.engine.params_choice_mismatch",
                                method = spec.name,
                                parameter = param.name,
                                choices = choices,
                                value = value
                            ),
                        ));
                    }
                }
            }
        }
    }

    for key in given.keys() {
        if !spec
            .params
            .iter()
            .any(|param| param.name.as_ref() == key.as_str())
        {
            let known = if spec.params.is_empty() {
                acb_i18n::t!("kernel.engine.no_params")
            } else {
                acb_i18n::t!(
                    "kernel.engine.available_params",
                    params = spec
                        .params
                        .iter()
                        .map(|param| param.name.as_ref())
                        .collect::<Vec<_>>()
                        .join(" / ")
                )
            };
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "kernel.engine.unknown_param",
                    method = spec.name,
                    parameter = key,
                    known = known
                ),
            ));
        }
    }

    Ok(())
}

/// 房间目标参数校验：`group_id` / `user_id` 这类参数只能等于当前房间。
///
/// `MethodSpec.targets` 由适配器声明（ADR-0027）。core 不认识平台词，只做两件事：
/// - 当前房间类型匹配的 `Room` 目标：参数给了就必须等于房间 id；
/// - 其它房间类型的 `Room` 目标：给了就拒绝——除非同一参数在当前房间类型里
///   被声明成 `Member`（如 `poke.send` 在群里的 `user_id` 是“拍谁”）。
fn check_target_params(spec: &MethodSpec, room: &RoomId, params: &Value) -> Result<(), ToolError> {
    let Some(given) = params.as_object() else {
        // check_params 已经处理过非对象；这里只兜底。
        return Ok(());
    };
    if given.is_empty() || spec.targets.is_empty() {
        return Ok(());
    }

    let room_kind = room.path.first().map(String::as_str).unwrap_or("");
    let room_id = room.path.get(1).map(String::as_str).unwrap_or("");

    for target in spec.targets.iter() {
        if target.role != TargetRole::Room {
            continue;
        }
        let Some(value) = given.get(target.param.as_ref()) else {
            continue;
        };
        let given_id = value.as_str().unwrap_or_default();

        if target.room_kind.as_ref() == room_kind {
            if given_id != room_id {
                return Err(bad_target(spec, room, target, given_id));
            }
            continue;
        }

        // 这个参数指向另一种房间。只有“同一参数在当前房间类型里是成员目标”
        // 的情况可以放过（poke.send 群里的 user_id）。
        let is_member_here = spec.targets.iter().any(|other| {
            other.role == TargetRole::Member
                && other.param.as_ref() == target.param.as_ref()
                && other.room_kind.as_ref() == room_kind
        });
        if !is_member_here {
            return Err(bad_target(spec, room, target, given_id));
        }
    }

    Ok(())
}

fn bad_target(
    spec: &MethodSpec,
    room: &RoomId,
    target: &crate::outbound::TargetSpec,
    given_id: &str,
) -> ToolError {
    let suggestion = format!(
        "{}:{}/{}",
        room.connection,
        target.room_kind.as_ref(),
        given_id
    );
    ToolError::new(
        ErrorCode::BadRequest,
        format!(
            "[bad_params] {} has {} = {given_id}, but the current room is {room}. \
             **Do not retarget in params**; to operate on it, change platform_call's room to \"{suggestion}\" \
             (and make sure this session is subscribed to it).",
            spec.name, target.param
        ),
    )
}

/// 适配器报的错 → 工具错。分类写进 message 里，模型靠它判断该改参数还是该放弃。
fn call_error(error: CallError) -> ToolError {
    let (code, kind) = match &error {
        CallError::UnknownMethod { .. } => (ErrorCode::BadRequest, "unknown_method"),
        CallError::BadParams(_) => (ErrorCode::BadRequest, "bad_params"),
        CallError::UnsupportedPlatform(_) => (ErrorCode::ToolNotAllowed, "unsupported_platform"),
        CallError::NotImplemented(_) => (ErrorCode::ToolNotAllowed, "not_implemented"),
        CallError::Forbidden(_) => (ErrorCode::ToolNotAllowed, "forbidden"),
        CallError::Unavailable(_) => (ErrorCode::Internal, "unavailable"),
        CallError::Timeout(_) => (ErrorCode::Timeout, "timeout"),
    };
    ToolError::new(code, with_file_hint(format!("[{kind}] {error}")))
}

/// 平台说「文件没有」时，多半不是路径写错：agent 和平台常常不在同一个文件系统
/// 命名空间里（平台在容器里），同一个文件两边看到的路径不一样。裸 ENOENT 会把
/// 排查方向带偏到拼写/权限/软链上——把三条出路附在错误后面。
fn with_file_hint(detail: String) -> String {
    let lower = detail.to_ascii_lowercase();
    let missing = lower.contains("enoent")
        || lower.contains("no such file")
        || lower.contains("realpath")
        || lower.contains("image not found")
        || lower.contains("file not found")
        || lower.contains("not found in cache");
    if !missing {
        return detail;
    }
    format!(
        "{detail}\n[file issue?] The platform's \"not found\" is probably not a wrong path: the agent \
         and the platform often live in different filesystem namespaces (the platform runs in a \
         container). Three options: \
         (1) configure [file_relay] and retry - files that exist locally are picked up and replaced \
         with a URL; \
         (2) for small files (raw content up to 750 KB, single-frame limit 1 MiB), write the element \
         url as base64://...; \
         (3) place the file in a shared directory mounted on both sides and pass the platform-side \
         path. Do not start by checking path spelling and permissions."
    )
}
