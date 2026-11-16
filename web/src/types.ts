/**
 * 控制台 API 的响应类型。
 *
 * 只声明页面**实际读**的字段，不追求与 Rust 结构逐字段对齐：
 * 多声明一个没人读的可选字段，只会让下一个人以为它被用到了。
 * 每个类型都注明了服务端出处（`crates/bridge/src/console.rs` 的哪个视图），
 * 改后端时能顺着找到这里。
 */

/**
 * `GET /api/status` —— `console.rs` 的 `status()`。
 *
 * 字段名与形态以实测响应为准（2026-11-16 对 `/api/status` 取回）：
 * `busy_sessions` 是**会话 id 数组**，不是计数；`staged` / `pending_reload`
 * 在当前实现里不出现，所以不声明——声明没人读的字段会误导下一个人。
 */
export interface StatusView {
  connections: number;
  sessions: number;
  harnesses: number;
  busy_sessions: string[];
  queued_turns: number;
  config_version: number;
  /** 每个配置分片贡献了多少条目。 */
  config_fragments?: Array<{
    path: string;
    sessions: number;
    connections: number;
    rules: number;
  }>;
  /** 依赖计数的快速摘要；明细走 `/api/dependencies`。 */
  dependencies?: { up: number; down: number; unknown: number; status: string };
  queues?: StatusQueues;
}

export interface QueueSnapshot {
  depth: number;
  capacity: number;
  highWatermark: number;
  warnPercent: number;
  rejected: number;
  warned: number;
  busy?: number;
}

/** `GET /api/status` 里的队列段。 */
export interface StatusQueues {
  inbound: QueueSnapshot;
  route: QueueSnapshot;
}

export type DependencyState = "up" | "down" | "unknown";

export interface DependencyEntry {
  id: string;
  state: DependencyState;
  since?: number;
  lastOkAt?: number;
  class?: string | null;
  detail?: string | null;
  hint?: string | null;
  attempts?: number;
}

/** `GET /api/dependencies` —— `deps.snapshot()`。 */
export interface DependenciesView {
  entries: DependencyEntry[];
  /** 本次启动以来的状态迁移（只增）。 */
  transitions?: Array<{ id: string; from: DependencyState; to: DependencyState; at: number }>;
}

export interface SubscriptionView {
  room: string;
  wake?: Record<string, unknown>;
}

/** `GET /api/sessions` 的单个会话。 */
export interface SessionView {
  id: string;
  name?: string;
  harness: string;
  persona?: string | null;
  model?: string | null;
  busy?: boolean;
  queued?: number;
  /**
   * 已订阅的房间，**字符串数组**（`qq:group/1` 形态）。
   *
   * 与 `/api/config` 里的对象形态不同：那个带 wake 策略，这个只是渲染用的
   * 房间串。以前这里写成了 `SubscriptionView[]`，与后端对不上。
   */
  subscriptions?: string[];
  /** 桥 ↔ harness 会话映射。 */
  mapping?: { session_id?: string; harness?: string } | null;
  /** 投过 ≥3 次仍未确认的房间（D7）。 */
  stuck_unread?: Array<{ room: string; count: number }>;
  /** 本会话本轮生效的后备链（内存快照，控制台展示以 model-fallbacks 接口为准）。 */
  model_fallbacks?: string[];
}

export interface SessionsView {
  sessions: SessionView[];
}

/** `GET /api/plugins` 的目录条目。 */
export interface PluginView {
  id: string;
  name?: string;
  version?: string;
  enabled: boolean;
  /** 插件作者声明的配置/资源/动作 schema（自描述面）。 */
  schema?: PluginSchema;
  /** 插件作者声明的对象，服务端已遮掉密钥。 */
  config?: Record<string, unknown>;
  resources?: Record<string, Array<Record<string, unknown>>>;
  actions?: string[];
  connections?: Array<{ platform: string; state: string }>;
}

/**
 * 插件自描述面（`plugin_manifest.rs` 的 `form_schema()`）。
 *
 * 这就是「acb 插件上报自描述接口」的现有落点：字段类型、枚举、范围、是否必填、
 * 是否密钥，全由插件声明，core 只照着渲染与校验。前端按它动态生成表单，
 * **不硬编码任何插件字段名**。将来若要扩成 OData 式的资源/关系模型，
 * 扩展点在这里，不在各 tab 组件里。
 */
export interface PluginSchema {
  config?: SchemaField[];
  resources?: SchemaResource[];
  actions?: SchemaAction[];
}

export interface SchemaField {
  key: string;
  label?: string | null;
  description?: string | null;
  type: "string" | "integer" | "number" | "boolean" | "array" | "object";
  required?: boolean;
  default?: unknown;
  enum?: unknown[];
  min?: number | null;
  max?: number | null;
  secret?: boolean;
}

export interface SchemaResource {
  name: string;
  label?: string | null;
  id_field?: string | null;
  fields?: SchemaField[];
}

export interface SchemaAction {
  name: string;
  label?: string | null;
  description?: string | null;
}

export interface PluginsView {
  plugins: PluginView[];
}

/** `GET /api/feedback` 的条目。 */
export interface FeedbackView {
  id: number;
  title?: string;
  body?: string;
  status: string;
  created_at?: number;
  notes?: string;
  session?: string;
}

export interface FeedbacksView {
  feedbacks: FeedbackView[];
  total?: number;
}

/** `GET /api/policy` —— 权限规则、房间信任、管理员、审批路由。 */
export interface PolicyView {
  rules?: Array<Record<string, unknown>>;
  rooms?: Record<string, Record<string, unknown>>;
  accounts?: Record<string, Record<string, unknown>>;
  admins?: string[];
  roles?: Array<Record<string, unknown>>;
  assignments?: Array<Record<string, unknown>>;
}

/** `GET /api/model-fallbacks[?session=]` —— `console.rs` 的 `model_fallbacks_view`。 */
export interface ModelFallbacksView {
  /** `database` = 直接读库（保存后立即可见）；`memory` = 读库失败退回本轮快照。 */
  source: "database" | "memory";
  session: string | null;
  models: string[];
  /** 仅会话范围可能为 true：没有自己的链，回落全局。 */
  inherited: boolean;
  harness?: string | null;
  /** 该 harness 的模型列表；空 = 未配置，不限制。 */
  available: string[];
  pending_reload: boolean;
  config_version: number;
}

/** `GET /api/extensions` 的条目。 */
export interface ExtensionView {
  id: string;
  kind?: string;
  state?: string;
  detail?: string;
}

/** `POST /api/model-fallbacks` 的成功响应。 */
export interface ModelFallbacksSaved {
  ok: boolean;
  session: string | null;
  models?: string[];
}

/** `GET /api/i18n?locale=` —— 服务端词条目录。 */
export interface I18nCatalog {
  locale: string;
  messages: Record<string, string>;
  supported?: string[];
}

/**
 * 失败惩罚策略（`GET /api/model-penalty` 的 `policy`）。
 *
 * 与 `crates/kernel/src/model_penalty.rs` 的 `FallbackPolicy` 一一对应。
 * 语义：同一个模型在 `failure_window_secs` 内失败满 `failure_threshold` 次，
 * 就从下一轮起被跳过 `penalty_secs` 秒。`failure_threshold = 0` = 关闭惩罚。
 */
export interface PenaltyPolicy {
  enabled: boolean;
  failure_threshold: number;
  failure_window_secs: number;
  penalty_secs: number;
}

/** 正在被惩罚的一个模型（内存态，桥重启即空）。 */
export interface ActivePenalty {
  session: string | null;
  model: string;
  class: string;
  remaining_secs: number;
}

/** `GET /api/model-penalty[?session=]` —— `console.rs` 的 `model_penalty_view`。 */
export interface PenaltyView {
  session: string | null;
  policy: PenaltyPolicy;
  /** 会话范围且没单独配置时为 true：正在用全局策略。 */
  inherited: boolean;
  penalties: ActivePenalty[];
  pending_reload: boolean;
  config_version: number;
}

/**
 * `GET /api/rooms?connection=X` —— 这条连接上可选的所有房间（含未订阅）。
 *
 * `supported = false` 与 `rooms` 为空是**两件事**：前者是这个适配器不实现枚举
 * （界面保留手输框），后者是实现了但确实没有可选房间。合并会让人去查群设置
 * 而不是换适配器。
 */
export interface RoomsView {
  connection: string;
  supported: boolean;
  /** 本次结果是否命中服务端缓存（TTL 由 ACB_CONSOLE_ROOMS_TTL_SECS 控制，0 = 不缓存）。 */
  cached: boolean;
  rooms: RoomChoice[];
}

/** 一条可选的房间。`label` 只给人看，不参与路由。 */
export interface RoomChoice {
  room: string;
  label: string;
  /** 平台侧分类提示：`group` / `private` 等，core 只透传。 */
  kind: string;
}
