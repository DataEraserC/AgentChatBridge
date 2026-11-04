# 失联提示与带外通知（设计提案）

状态：提案（2026-11-04）；第①②③④⑤步已实现。
范围：turn 级失败通知、组件级连通性状态、`/healthz`、`/api/plugins`、带外通知（log / pull / ntfy）。
非目标：自动拉起 dsh/opencode、自动切换平台、在 core 里探测平台、为每个平台写特例。

> 实现进度（2026-11-04）：
> - 第①步：`crates/kernel/src/deps.rs` 的 `DependencyRegistry` 已接 harness 连接存在性；
>   状态迁移写 `[deps] transition=...` 边沿日志；`/api/dependencies?since=<seq>` 与
>   `/healthz.dependencies` 已暴露。表字段为 `state / since / lastOkAt / lastAttemptAt /
>   neverConnected / lastError / lastErrorClass / attempts`。
> - 初始状态为 `unknown`；没有“宽限期到点自动标 down”的定时器，等第一次连接/断开事件。
> - 第②步：适配器经 `AdapterHost.status` 上报平台 `connected / disconnected /
>   not_configured`；OneBot 与 process 已接。`AdapterFactory::health` 默认改为
>   `Health::Unknown`，不再假装 Healthy；平台连接细节看 `/api/dependencies`。
> - 第③步：`Bridge::end_turn` 对 `error` 文本取 FNV-1a 指纹，按
>   `(session, room, fingerprint)` 在 10 分钟窗口内只发首条；同一房间窗口内最多
>   3 条；一轮成功清空该 `(session, room)` 的历史失败。
> - 第④步：`turn_end` 增加可选 `failure { component, id?, class }`（协议 v7，
>   兼容 v6）；dsh 在模型/后端失败时填，opencode 只在明确线索时填。core 把
>   `model` 写进 `DependencyKind::Model`，`backend` 写进对应 harness；一轮成功
>   把该会话的模型与 harness 标回 `up`。有结构化分类时 in-band 去重键改为
>   `(session, component, id, class)`，不再看错误文本。
> - 第⑤步：`crates/plugins/process-tools` 新增 `ProcessNotifier`（kernel
>   `NoticeSink` 的进程实现），`crates/notifier-process` 的 `acb-notifier`
>   负责 ntfy 兼容 HTTP、去重、恢复配对和有界重试。`[notifications]` 不完整时
>   整层不 spawn；`[notifications.in_band]` 接管第③步的三个常量。

## 0. 结论

1. **turn 级错误通道只覆盖「有轮次 + harness 在 `turn_end` 里返回 error」**，层级在
   `endpoint → engine → TurnErrorProvider → outbound`。它可以复用底层发送管线，但**不能**
   当作组件级失联通道：没有轮次就没有通知，平台断开时通知也发不出去。
2. **三类对象必须分开**：后端（dsh/opencode harness）、平台（snowluma/OneBot 适配器）、
   模型（`model_unavailable`）。它们的连接方向、可观测点和可用通道不同，不能用同一套
   文案和同一套判定。
3. **平台侧断线一律走带外**：结构化 log、`/healthz`/`/api/dependencies`、可选 ntfy。
   「恢复后回到会话补一条」**不是主通道**，默认关闭；只在显式配置时作为补充，并且必须
   带原始时间区间与补发标记。
4. **状态全局、通知分层**：组件状态按稳定 id 全局记录；in-band 只做「用户正在等的那一次」
   首失败；状态迁移（down/up）走带外，一次故障一条。
5. **最小顺序**：core 依赖状态 + harness 存在性 → 适配器上报平台状态 → turn 失败去重/抑制
   → harness 结构化失败分类 → 外置 ntfy sidecar。平台概念只出现在 adapter/harness，
   core 只认通用状态与通用失败分类。

## 1. turn 级错误通道：现状、覆盖与复用边界

### 1.1 实现链路

| 环节 | 位置 | 行为 |
|---|---|---|
| 协议入口 | `crates/proto/src/frame.rs` 的 `Frame::TurnEnd { ticket, error }` | harness 在收工时上报自由文本错误；字段只有 `ticket` 与 `error` |
| 连接状态机 | `crates/kernel/src/endpoint.rs` 的 `Conn::handle` | 校验票归属，移除在跑票，调用 `Bridge::end_turn(&ticket, error.as_deref())`，并向 harness 回一条 `Log` 帧 |
| 引擎收尾 | `crates/kernel/src/engine.rs` 的 `Bridge::end_turn` | 记录 `kernel.engine.turn_end_received`；`error` 非空且非 `stop_requested` 时调用 `notify_turn_error`；随后作废票、写 `ended` 宽限记录、释放会话 |
| 通知决策 | `crates/kernel/src/engine.rs` 的 `Bridge::notify_turn_error` | 按会话 `turn_error` 解析 `TurnErrorProvider`；默认 provider 选择「会话 `on_turn_error` 非空则用它，否则全局 `default_on_turn_error`」；动作只有 `Log` 与 `Send { room }` |
| 默认策略 | `crates/kernel/src/plugin.rs` 的 `DefaultTurnErrorProvider` | 只做动作选择，不分类、不去重、不判断组件 |
| 出站 | `crates/kernel/src/engine.rs` 的 `Bridge::prepare_send` / `PreparedSend` | 失败通知走 `SendOrigin::TurnFailure`，会经过发送前钩子；组装后放进 `pending_notices` |
| 真正发送 | `crates/kernel/src/endpoint.rs` 的 `run_connection`（以及 `Bridge::flush_notices`） | 放下全局锁后 `relay()` + `execute()`；relay 或发送失败只记日志，不重试、不回队列 |
| 配置 | `crates/kernel/src/config.rs` 的 `Session::on_turn_error`、`default_on_turn_error` | 动作是 `log` 或 `send:<房间>`；没有「组件」「级别」「去重」这类字段 |
| 超时 | `crates/kernel/src/config.rs` 的 `turn_timeout_secs`，随 `Delivery.turn_timeout_ms` 下发 | **桥不自己掐超时**；由 harness 到点打断并把错误写进 `turn_end.error` |

### 1.2 覆盖范围

覆盖：

- harness 在轮次内报告的任何错误：模型不可用、模型网关拒绝连接、harness 会话失效、
  prompt/工具链内部错误、harness 侧主动打断产生的失败。
- 会话配置了 `on_turn_error = ["send:<房间>"]` 或全局 `default_on_turn_error` 时，
  每个失败的轮次都会尝试发一条 `[turn failed] session ... (room): <error>`。
- `stop_requested` 的轮次被明确排除，只写 `turn_stopped_by_stop` 日志，不发通知。

**不覆盖**：

| 场景 | 现状 | 原因 |
|---|---|---|
| harness 从未连上 | 没有通知 | 没有 `turn_end`；`pending` 队列只是堆积 |
| harness 在轮次中间断开 | 没有通知 | `Conn::abandon` 调用 `end_turn(ticket, None)`；按「未确认、下轮重投」处理，不制造失败 |
| 平台适配器断开 | 没有通知 | 失联发生在 harness 之外；若此时没有轮次，连 `turn_end` 都不会产生 |
| 平台断开导致失败通知发不出去 | 只写日志 | `turn_error_notice_relay_failed` / `send_failed` 是终点，没有替代通道 |
| 桥启动失败/插件装载失败 | 启动失败或 `lastError` | 不在 turn 通道里；属于启动期错误 |
| 桥进程死亡 | 无 | 进程内通道随之消失，只能靠外部 supervisor/带外探针 |
| 组件恢复 | 无 | 失败通知没有配对恢复的概念 |

### 1.3 为什么不能直接复用为组件级通道

1. **触发条件不匹配**：组件级失联（尤其平台断开、harness 未连接）不一定产生轮次；
   没有轮次就没有 `turn_end`。
2. **语义单位不匹配**：`TurnErrorRequest` 的输入是 `session`、`room` 和一段自由文本；
   组件状态需要 `kind`、稳定 id、`since`、`last_ok_at`、`never_connected`、尝试次数。
3. **作用域不匹配**：`on_turn_error` 是会话级配置；一个平台连接可能服务多个会话，
   一个 harness 也可能服务多个会话。按会话发会造成 N 倍重复。
4. **没有边沿语义**：每次失败都发；长时间故障期间，时钟轮次、用户消息、重试都会把
   同一条故障反复刷进房间。
5. **没有恢复配对**：没有任何机制知道「这次失败属于哪次故障」，也就无法只发一条恢复。
6. **通道可能已死**：平台断开时，失败通知和普通消息走同一条 outbound；桥无法用坏掉的
   通道报告通道坏了。
7. **错误是自由文本**：`ECONNREFUSED 127.0.0.1:3080`、`session/model-unavailable`、
   工具链错误都只是字符串，core 无法可靠区分「模型网关连不通」「模型未注册」
   「harness 自己坏了」。
8. **文案与钩子耦合**：`[turn failed] ...` 是 core 内写死的格式；通知还要经过
   `SendHook::before_send`，钩子失败会导致通知失败，这与「告警必须留痕」冲突。

结论：**复用 `prepare_send`/`pending_notices` 的锁外发送管线可以；复用
`TurnErrorProvider` 的策略形状不可以。**

## 2. 三类对象的区别

| 维度 | 后端（dsh / opencode） | 平台（snowluma / OneBot / process adapter） | 模型（`model_unavailable`） |
|---|---|---|---|
| 谁连谁 | **harness 主动连桥**（`hello` + token）。桥不主动连 dsh/opencode | **桥主动连平台**（`ws-client`），或桥启动外置适配器进程 | **harness 连模型网关**；桥不直接连模型 |
| 失联表现 | 没有 hello；连接中途关闭；有连接但轮次反复失败；`queued_turns` 增长 | 连接被拒绝/超时/断开；适配器重连循环；outbound 返回 `Unavailable`；入站静默 | 只在轮次内暴露：`turn_end.error` 含模型不可用/网关拒绝连接 |
| 桥的可观测点 | endpoint 接受连接、握手、断开；`pending` 队列；`turn_end.error` | 只有适配器内部知道；core 目前没有逐连接状态 | 只有 `turn_end.error` 文本；桥无法主动探测 |
| 可用的通知通道 | 平台存活时可用 turn 通道；平台也挂时只能带外 | **平台自己的通道已不可用**：只能 log / pull / ntfy；其他平台连接只能作为显式配置的目标 | 平台通常存活，可走 turn 通道；重复失败必须抑制 |
| 判定归属 | 连接存在性属 core；harness 内部故障属 harness | 连接状态属 adapter；core 只存上报结果 | 失败分类属 harness；core 只存结构化结果 |

### 2.1 后端：区分「某次请求失败」与「后端整体宕机」

- **某次请求失败**：harness 连着，`turn_end` 带 error。用户在等这一次，应该收到 turn
  级通知（现有通道）。
- **后端整体宕机**：harness 没有连接（进程退出、端口不可达、未启动），或者 harness
  明确声明「后端不可用」。这不是某次请求的问题，应走带外 + 单条告警；**不能每个 turn
  都发**。
- 介于两者之间：harness 连着，但同一后端错误在多个会话/多轮重复出现。**只有 harness
  给出结构化 `component=backend` 时，core 才能把重复失败升级为组件级 down；**
  仅凭自由文本重复，core 不做「后端宕机」断言，只做会话级去重。

### 2.2 平台：断开时没有入站通道，也没有出站通道

- 平台断开后，桥既收不到消息，也发不出消息；任何 in-band 通知都不可靠。
- **入站静默本身不是 down 的证据**：群里没人说话也会静默。只有适配器报告的连接状态
  或明确错误才进入 `down`；不把「一段时间没有入站」自动判成故障。
- 唯一可靠的是**带外层**：结构化 log、可拉取的 `/healthz`、外部 ntfy。
- 若配置了另一条存活连接（如另一个平台/账号）作为告警目标，可以显式指定；core 不
  自动选择、不静默跨平台广播。

### 2.3 模型：桥只能看到 harness 的结论

- 模型网关是 harness 的依赖，不是桥的依赖。桥能看到的只有 `turn_end.error`。
- 必须由 harness 分类：`model_unavailable`（网关不可用/拒绝连接）、
  `model_unconfigured`（模型未在该 harness 注册）、`auth_failed`（凭据）、
  `internal`（其它）。在 harness 没有给出结构化分类前，core 只能显示原始文本，不能声称
  「模型网关连不通」。
- 模型失败天然是 per-turn 的；首失败可以走 turn 通道，重复失败必须去重并升级为
  OOB，避免每个轮次刷一条。

## 3. 设计

### 3.1 通用依赖状态模型

core 新增一个**与平台无关**的依赖注册表（建议 `crates/kernel/src/deps.rs`，独立锁，
不放进 `Bridge` 的大锁）：

```text
ComponentKey := kind + id
  kind = harness | platform | model
  id   = harness id / connection id / 模型引用

DependencyStatus {
  key, kind, id,
  configured: bool,          // 配置里存在
  expected: bool,            // 需要它提供服务
  state: "unknown" | "up" | "down",   // unknown 仅表示启动宽限期内尚未观测
  reason: None | {
    class: "not_started" | "unreachable" | "auth_failed" | "closed"
         | "model_unavailable" | "model_unconfigured" | "rate_limited" | "internal",
    detail: String,           // 已脱敏，可直接展示
    at: i64,                  // Unix ms
  },
  since: i64,                // 当前状态开始时间
  last_ok_at: Option<i64>,   // null = 本次桥进程内从未连上
  last_attempt_at: Option<i64>,
  attempts: u64,
  never_connected: bool,     // last_ok_at.is_none()
  reconnecting: bool,        // 适配器仍在重试；不等于 up
  sessions: [session_id],    // harness 服务的会话；platform 不填
  platform, connection,      // platform 专用展示字段
  model_ref, session,        // model 专用展示字段
}
```

状态机有 `unknown` / `up` / `down` 三态；`unknown` 只出现在启动宽限期内，
稳定态是 `up` / `down`，其余都是 `reason` 与附属字段：

- **未配置/`unconfigured`**：不是运行时状态。配置引用不存在的 harness、未知 adapter
  kind、会话引用了不存在的连接，属于启动校验失败；桥要么退出，要么启动时校验报错，
  不进入依赖表，也不发运行时通知。
- **未启动/`not_started`**：配置要求它服务，但启动宽限期（默认 30s）内没有出现过任何
  连接/尝试。宽限期结束后仍为 `down`，`reason.class=not_started`，
  `never_connected=true`、`attempts=0`。
- **连不通/`unreachable`**：有过尝试但从未成功（`never_connected=true`、
  `attempts>0`），或成功过但当前连接失败（`last_ok_at` 有值）。
- **曾经连上后断开/`closed`**：`last_ok_at` 有值、当前 `down`，`reason.class=closed`
  （平台适配器也可用 `unreachable`，由适配器区分「主动关闭/网络错误/拒绝连接」）。
- **恢复**：收到一次成功连接/成功 turn 后 `down → up`，`since` 更新，`reason=None`；
  `last_ok_at` 更新。

时间与身份约定：

- 所有时间由 core 打戳（Unix ms + 展示时格式化为带时区 ISO 8601）。适配器只传
  `state`、`class`、`detail`、`attempts`，不传时钟。
- `harness` 的 key 来自配置 `[[harnesses]].id`；`platform` 的 key 来自
  `[[connections]].id`；`model` 的 key 优先用 `session.model`，为空时用
  `model:<harness>/default`，避免把「harness 默认模型」误算成全局模型。
- 组件集合以当前配置为界；配置热重载后，下一轮次边界裁剪不再被引用的条目。
  **被移除的依赖直接删除条目**：不留 `down` 幽灵，也不写迁移环——迁移环只记录
  运行时状态变化，配置增删用 `[deps] removed` 日志表达。
- 成功 turn 可更新 harness 的 `last_ok_at`；只有会话配置了具体 `model` 时，才更新
  该 model 的 `last_ok_at`，`model` 为空时不更新具体模型条目。
- 表大小有上界（建议 256，超出淘汰最久未更新的 `up` 条目）；不随消息/轮次无限增长。

### 3.2 通知分层

| 层 | 用途 | 触发 | 目的地 | 是否依赖平台可用 |
|---|---|---|---|---|
| in-band turn | 用户正在等的那一次失败 | `turn_end.error`，每个 (session, component, class) 首次 | 会话 `on_turn_error` / 全局默认 | 是 |
| OOB log | 状态迁移审计 | `up→down` / `down→up` 边沿 | stderr → 本机日志（当前部署为 `state/logs/bridge.log`） | 否 |
| OOB pull | 当前状态与历史迁移 | 任意时刻 | `/healthz`、`/api/dependencies` | 否（只要桥进程与 HTTP 在） |
| OOB push | 主动提醒 | 迁移边沿 | ntfy / 同类 HTTP 推送 | 否（外置 sidecar） |

按对象决定 in-band 策略：

| 对象 | 首失败 in-band | 组件已 down 时 in-band | 状态迁移 | 恢复 |
|---|---|---|---|---|
| 后端 harness（请求级） | 每个会话首次 1 条 | 抑制 | OOB | OOB；默认不补发 in-band |
| 后端整体宕机 | 不发（没有轮次） | 不发 | OOB 单条 | OOB 单条 |
| 平台 | **永不** | **永不** | OOB 单条 | OOB 单条 |
| 模型 | 每个 (session, room, model) 首次 1 条 | 抑制 | OOB 单条 | OOB；可选带时间区间的补发 |

in-band 是否真的发出仍取决于会话的 `on_turn_error` / 全局 `default_on_turn_error`；
默认配置是 `[]`，即只记日志。OOB 层不依赖这项配置，因此组件级告警不受它影响。

### 3.3 带外层

#### 3.3.1 结构化日志

core 在每次状态迁移时写**一行**，不是每次重试都写：

```text
[deps] transition=up->down kind=platform id=qq state=down since=2026-11-04T03:12:00+08:00 last_ok=2026-11-04T03:01:00+08:00 class=unreachable attempts=12 detail="connect ECONNREFUSED 127.0.0.1:53001"
```

约定：

- `[deps]` 前缀与 `key=value` 字段名是给日志解析/检索用的稳定协议；人类可读句子走
  i18n 键（例如 `kernel.deps.transition`），不在代码里写死中文/英文文案。
- 一行只描述一次迁移；适配器自己的重连细节日志保留，但不作为告警依据。
- `detail` 必须脱敏：去掉 URL 的 `user:pass@`、`token=` 查询参数、Authorization 值。
- 当前部署的 systemd 单元把 `StandardOutput`/`StandardError` append 到
  `state/logs/bridge.log`；通用部署可走 journald。日志轮转由外部（logrotate/systemd）
  负责，core 不重建日志设施。
- 桥进程死亡时本层失效；这是带外 sidecar 与 systemd 的职责，不是 core 能自证的场景。

#### 3.3.2 可拉取状态

- `GET /healthz`（仅 loopback + 控制台 token）返回依赖摘要与 `state`；
  只要进程活着就返回 200，不因为远端平台宕机而让 systemd 重启桥。
- `GET /api/dependencies?since=<seq>` 返回完整依赖列表与最近迁移环；供控制台和
  ntfy sidecar 使用。
- 读快照只读缓存状态，**不在请求路径里联系适配器、不执行 I/O**；否则健康探针会被
  平台故障拖死。
- 若没有配置 `[console]`，就没有 HTTP 面；此时只剩 log（以及外置 sidecar 自行决定
  是否 tail 日志）。

#### 3.3.3 ntfy / 同类 HTTP 推送：位置与边界

**结论：不属于 core。** 实现为 bridge 管理的独立通知子进程 `acb-notifier`：
bridge 用 stdio 行 JSON-RPC 把依赖边沿与 turn 抑制事件交给它，HTTP 推送、去重、
重试全在子进程里。

理由：

- core 当前不做外部 I/O；在 core 里加 HTTP 客户端、重试队列、凭据管理，会把新的
  失败模式和锁外等待带进核心。
- 子进程独立崩溃/网络失败与桥隔离；ntfy 故障不会拖累消息处理。
- 复用 `process-tools` 的 stdio 子进程形态，但**不注册成 ToolProvider**：通知没有
  工具面，且依赖边沿发生在多个线程，不能走同步工具调用路径。

形态：

```text
acb-bridge
  └─ ProcessNotifier（kernel NoticeSink 的进程实现，有界队列 128）
       └─ acb-notifier（stdio 行 JSON-RPC，只收 notify）
            ├─ 去重：同一依赖边沿 / 抑制事件 min_interval_secs 内只推一次
            ├─ 恢复配对：只有推过 down 才推 up，且带 down_since + duration
            └─ POST {url}/{topic}（ntfy 兼容，首次失败后最多重试 3 次，退避 250/500/1000ms）
```

配置放在桥的 `config.toml`，不再单独起 sidecar 配置文件：

```toml
[notifications]
enabled = true
url = "http://127.0.0.1:8080"
topic = "acb-ops"
token = "${NTFY_TOKEN}"
min_interval_secs = 600

[notifications.in_band]
window_secs = 600
room_limit = 3
max_entries = 256
```

- `url` / `topic` 任一为空或 `enabled = false` 时，bridge **完全不注册**这一层：
  不 spawn、不建线程、不打启动日志；`/api/dependencies` 等其余行为不变。
- 需要把 `acb-notifier` 与 `acb-bridge` 一起部署（同目录）；测试/特殊部署可用
  `ACB_NOTIFIER_BIN` 覆盖路径。
- `[notifications.in_band]` 接管了原先 in-band 失败通知的三个硬编码常量；
  省略时默认值与旧行为一致。

消息格式（字段稳定，值为数据）：

```text
Title: [acb] platform/qq is down
Priority: high
Tags: acb,platform,down

component=platform/qq
transition=up->down
state=down since=2026-11-04T03:12:00Z
last_ok=2026-11-04T03:01:00Z
class=unreachable
lastError=connect ECONNREFUSED 127.0.0.1:53001
attempts=12
```

恢复：

```text
Title: [acb] platform/qq recovered
Priority: default
Tags: acb,platform,up

component=platform/qq
transition=down->up
state=up since=2026-11-04T03:20:00Z
last_ok=2026-11-04T03:20:00Z
attempts=12
down_since=2026-11-04T03:12:00Z
duration=8m0s
down_class=unreachable
down_lastError=connect ECONNREFUSED 127.0.0.1:53001
```

去重与限流：

- notifier 只消费 `NoticeSink` 送来的迁移边沿与 turn 抑制事件，不轮询状态、不读
  harness 内部状态；重启后**不重放历史**。
- 同一依赖边沿（`component + from + to + class`）在 `min_interval_secs`（默认 600s）
  内只推一次；turn 抑制事件按 `(session, room, component, class, reason)` 同样去重。
- `up` 只有在该组件 `down` **成功推送过**时才配对推送；`down` 没推出去就不推 `up`。
  恢复正文固定带 `down_since`、`duration`、`down_class`、`down_lastError`。
- 连续抖动由边沿去重吸收：`down → up → down` 的第二次 down 若仍在窗口内不会重复。
- HTTP 4xx/5xx/网络错误：首次失败后最多重试 3 次，退避 250/500/1000ms；仍失败只记日志与失败计数，
  丢弃本条，不阻塞后续事件，不无限重试。
- 没有全局 6 条/10 分钟速率上限；按边沿去重 + 恢复配对已经能限制刷屏。若后续需要，
  再加独立的总量上限。

失败降级：

- 子进程与桥是两个进程；ntfy 失败不影响桥的任何路径。
- core 只做有界队列（128 条）`try_send`，满了丢弃并计数；不等待网络、不建重试队列。
- 如果子进程停止，log 与 `/healthz` 仍然可用；这是可接受的降级。

#### 3.3.4 平台断开时的完整路径

1. 适配器检测到连接失败/断开，向 core 上报 `down`（带 class/detail/attempts）。
2. core 生成迁移：写一行结构化日志、更新依赖表、把迁移放进有界环。
3. `/healthz` 立即返回 `state=degraded` 与 `dependencies` 中的该连接；
   `/api/dependencies?since=N` 可拉到迁移。
4. 若配置了 `[notifications]`，事件立即进入 notifier 子进程并推送一条 down。
5. 平台恢复后，适配器上报 `up`；core 再写一行、更新状态；notifier 推送一条带
   `down_since` / `duration` 的恢复。
6. **默认不向原房间补发**。若显式开启 `in_band_backfill`，只在恢复后发**一条**
   汇总，且必须带 `down_since` 与 `duration`，明确标注为补发；不得把积压的
   down 消息逐条重放。

### 3.4 去重、窗口与恢复语义

- **边沿触发**：只在 `up→down`、`down→up` 发通知；重试次数增加不产生新通知。
- **启动宽限**：默认 30s；宽限期内 `not_started` 只进状态表，不告警，避免桥先于
  dsh/opencode 启动时误报。
- **抖动抑制**：down 后 `recover_after_secs`（默认 60s）内恢复的，不推送 up（除非
  已推送过 down，则仍推送恢复，避免“只报坏不报好”）。
- **恢复配对**：只有已发出/已排队的 down 才配对 up；从未送达的 down 不产生 up。
- **in-band 抑制**：组件进入 `down` 后，同一 `(session, component, class)` 的后续
  turn 失败不再发消息；只更新依赖表与 `attempts`。
- **按房间去重**：同一组件故障在同一个房间只发首条；多个会话共享一个房间时不会放大。
- **状态独立于送达**：通知是否成功不影响依赖状态；状态迁移仍然可拉取、可审计。
- **有界**：迁移环固定长度（建议 128）；依赖表固定上限（建议 256）；harness/platform
  条目随配置重载裁剪，被移除的直接删除。

### 3.5 文案分类

所有文案通过 i18n 键渲染，代码里不写死中英文。建议的 `class` 与语义：

| class | 含义 | 典型触发 |
|---|---|---|
| `not_started` | 配置要求，但启动宽限期内从未出现 | harness 未启动、适配器进程未拉起 |
| `unreachable` | 尝试过但连不上（拒绝连接/超时/网络） | 平台端口未开、宿主网络不可达 |
| `auth_failed` | 身份/令牌不匹配 | harness token 错、平台 401 |
| `closed` | 曾经连上后断开 | 平台进程退出、harness 进程退出 |
| `rate_limited` | 平台 429/限流 | 适配器上报 `unavailable` + 429 |
| `model_unavailable` | harness 报告模型不可用（网关/上游） | dsh `session/model-unavailable` + ECONNREFUSED |
| `model_unconfigured` | 模型未在 harness 侧注册/配置 | harness catalog 不认这个 model |
| `internal` | 其它内部错误 | 无法归类的 harness 错误 |

文案区分要求：

- **未配置**：启动期错误，不产生运行时通知；由配置校验/启动日志/控制台配置视图表达。
- **未启动**：`not_started` + `never_connected=true` + `attempts=0`（或宽限期后仍无
  尝试）。
- **连不通**：`unreachable` + `attempts>0`；`never_connected` 区分「从未连上」。
- **曾经连上后断开**：`last_ok_at` 有值 + `state=down` + `class=closed/unreachable`。

示例键（实现时补全中英两份 locale，不在代码里内联文案）：

- `kernel.deps.transition`
- `notice.component.down.not_started`
- `notice.component.down.unreachable`
- `notice.component.down.auth_failed`
- `notice.component.down.closed`
- `notice.component.down.model_unavailable`
- `notice.component.down.model_unconfigured`
- `notice.component.up`
- `notice.platform.out_of_band_only`（平台断开时明确说明“in-band 不可用”）
- `notice.recovered.delayed`（可选补发时说明延迟）

### 3.6 防刷屏与 429

- 组件状态不进入 wake 管道：不发合成事件、不触发 `WakeProvider`、不唤醒 agent。
- in-band 通知按 `(component, room, class)` 去重；组件 `down` 期间抑制后续 turn 失败。
- 每个房间的 in-band 告警设置独立上限（建议 3 条/10 分钟）；超限只写日志。
- turn 失败通知复用现有 `prepare_send`，但不重试；`Unavailable`/`Timeout` 只记日志。
- 平台适配器返回 429 时，适配器负责退避；core 不按错误文本做重试循环。
- ntfy 的 429 由 sidecar 按 `Retry-After` 退避，与桥无关。
- 恢复通知只有一条；不补发历史 down 列表。

### 3.7 全局 vs 每会话

- **状态全局**：harness 按 `[[harnesses]].id`、平台按 `[[connections]].id`、模型按
  `session.model`（或 `model:<harness>/default`）记录；同一组件的多个使用者共享一份
  状态。
- **OOB 全局**：一份告警面向运维目标（ntfy 主题 / 控制台），不按会话复制。
- **in-band 按会话**：只用于「用户正在等的那一次」；按 `(session, room, component,
  class)` 去重。模型失败若多个会话落在同一房间，只发一条。
- **不自动跨平台广播**：告警目标必须显式配置；core 不猜「哪个平台还活着」。

## 4. `/healthz` 与 `/api/plugins`：现状与缺口

### 4.1 现状

| 接口 | 当前返回 | 说明 |
|---|---|---|
| `GET /healthz` | `drain_snapshot()`：`state`（`ready`/`draining`）、`active_turns`、`queued_turns`、`queued_tasks`、`running_tasks`、`pending_route_actions`、`plugin_errors`（仅计数）、`schema_version`、`version` | 仅 loopback + 控制台 token；没有逐依赖状态、`lastError`、`since` |
| `GET /api/plugins` | `{count, plugins: [...], wakeAudit}` | 每条插件有 `id`、`extension`、`capabilities`、`kind`、`source`、`health {state, detail}`、`lifecycle`、`metadata`；`Bridge::plugin_catalog` 追加 `state`、`pending`、`refs`、`lastError` |
| `GET /api/status` | `connections` 数量、`sessions` 数量、`harnesses` 数量、`busy_sessions`、`queued_turns`、`config_version`、`staged_config` | 没有逐连接/逐 harness 状态 |
| `GET /api/extensions` | namespace 的 `installed`、`pendingReload`、`lastError`、`details` | 仅动态扩展目录 |

### 4.2 缺口

1. **平台适配器没有逐连接健康状态**。`PluginManager::catalog` 对平台适配器只读
   `AdapterFactory::health()`；OneBot 与 process 工厂都使用默认 `Health::Healthy`，
   所以平台断开时 `/api/plugins` 仍显示健康。动态适配器只能报「句柄标死」，
   也不是逐连接连接状态。
2. **`lastError` 语义不完整**。现有 `lastError` 主要是实例化失败、生命周期失败、
   provider 解析失败；没有「最近一次运行时连接失败」。
3. **没有 `since` / `lastOkAt` / `neverConnected`**，无法区分「从未连上」与「曾经连上
   后断开」。
4. **`/healthz` 的 `plugin_errors` 只是计数**，看不到是哪一个组件。
5. **没有迁移历史**，sidecar 无法基于序号增量消费。
6. **没有 harness 连接状态**：`ControlRegistry` 只知道「哪些会话有控制通道」，
   且没有对外只读接口；`/api/status` 只有配置数量。
7. **没有区分「配置了但未启动」与「启动了但连不通」**。

### 4.3 建议补充

`GET /healthz`（保持 200；不因远端依赖宕机而让 systemd 重启）：

```json
{
  "state": "ready|degraded|draining",
  "dependencies": {
    "up": 3,
    "down": 1,
    "unknown": 1,
    "items": [
      {
        "kind": "platform",
        "id": "qq",
        "platform": "qq",
        "connection": "qq",
        "state": "down",
        "reason": {
          "class": "unreachable",
          "detail": "connect ECONNREFUSED 127.0.0.1:53001",
          "at": 1793000000000
        },
        "since": 1793000000000,
        "lastOkAt": 1792999000000,
        "neverConnected": false,
        "attempts": 12,
        "reconnecting": true
      }
    ]
  },
  "noticeSeq": 42,
  "schema_version": 11,
  "version": "0.1.0"
}
```

`GET /api/dependencies?since=<seq>`（控制台与 sidecar 使用）：

```json
{
  "seq": 42,
  "dependencies": [ ... 同上 ... ],
  "transitions": [
    {
      "seq": 41,
      "at": 1793000000000,
      "kind": "platform",
      "id": "qq",
      "from": "up",
      "to": "down",
      "class": "unreachable",
      "detail": "connect ECONNREFUSED 127.0.0.1:53001",
      "since": 1793000000000,
      "lastOkAt": 1792999000000
    }
  ]
}
```

`GET /api/plugins` 的补充：

- 保留现有插件目录（代码插件、provider、生命周期）。
- 增加 `connections` 摘要（或在 `/api/dependencies` 中提供，避免把两件事混在一个接口）：
  `id`、`platform`、`kind`、`adapter`、`state`、`since`、`lastOkAt`、`lastError`、
  `neverConnected`。
- `lastError` 明确区分 `instantiate` / `lifecycle` / `runtime` 三种来源；不要把
  「插件未实例化」与「平台连不通」合并成同一个字段。

`/api/status` 增加聚合计数：`dependencies: {up, down, unknown}`、
`platforms: {up, down}`、`harnesses: {up, down}`。不新增探测逻辑。

## 5. 最小实现顺序

| 步骤 | 归属 | 改动 | 验收 | 可回退 |
|---|---|---|---|---|
| 1 | **core** | 依赖注册表 + harness 存在性：endpoint 握手/断开时更新 `harness` 状态；`/healthz` 与 `/api/dependencies` 暴露；每次迁移写一行结构化日志 | 停止 dsh/opencode 的 harness 连接：`/healthz` 出现 `harness down`，`since`/`lastOkAt`/`neverConnected` 正确；重启后恢复 `up`；`queued_turns` 可见 | 删除注册表与两个接口即可；不影响 turn 通道 |
| 2 | **core + adapter** | `AdapterHost` 增加健康上报句柄；OneBot/process/dynamic 适配器在连接建立/断开/重启失败时上报；`/healthz` 与依赖接口展示逐连接状态 | 停 snowluma：平台条目 `down` + `lastError`，不再显示 `Healthy`；恢复后 `up`；无任何 in-band 消息 | 删除上报字段与适配器调用即可；适配器其余行为不变 |
| 3 | **core** | turn 失败去重/抑制：按 `(session, room, 错误指纹)` 首失败；错误指纹是原始 `error` 文本的稳定哈希，不做语义解析；组件级抑制在拿到第 4 步的 `component` 信号后才启用；in-band 限流 | 连续 10 次相同故障：每会话最多 1 条 in-band；第 4 步落地后组件级只留 1 条状态迁移；无刷屏 | 去掉去重表与抑制分支即可恢复现有行为 |
| 4 | **protocol + harness** | `turn_end` 增加可选结构化 `failure { component, id, class }`；dsh/opencode 在模型/后端失败时填；core 消费并更新 `model`/`backend` 状态 | 模型网关断开：首轮 in-band 1 条，状态迁移 1 条；恢复后 1 条 up；`model_unconfigured` 与 `model_unavailable` 可区分 | 字段可选，但需协议版本过渡（v6 harness 无 `failure` 时走现有路径）；去掉字段即回退 |
| 5 | **外置子进程（非 core）** | `ProcessNotifier` + `acb-notifier`：stdio 行 JSON-RPC 收依赖边沿与 turn 抑制事件；ntfy 兼容 POST；按边沿去重、恢复配对、3 次有界重试 | 平台断开 → 立即收到 down；恢复 → 带 down_since/duration 的 up；同一故障 10 分钟内只推一次；ntfy 停止 → 桥不受影响 | 不配置 `[notifications]` 或 `enabled=false` 即可整层不加载 |

可选的后续（不阻塞前五步）：

- 控制台依赖面板（读 `/api/dependencies`）。
- `in_band_backfill` 配置（默认 off；仅平台恢复后一条带时间区间的补发）。
- 迁移历史持久化到 state（跨重启保留 `lastOkAt` 与迁移）；当前建议内存即可。
- 若将来需要第三方推送通道，可复用已落地的 `NoticeSink` 扩展点，不再改 core。

### 5.1 为什么是这个顺序

- 第 1 步先让「后端未连接」从只有 `queued_turns` 变成可读状态，零出站风险。
- 第 2 步才碰平台；平台状态只能由适配器上报，core 不探测、不解释平台协议。
- 第 3 步先止住 turn 通道的刷屏，再做组件级抑制；否则先做告警会放大现有洪峰。
- 第 4 步需要协议演进；在没有结构化分类前，core 不应从自由文本猜「模型/后端宕机」。
- 第 5 步是外置 sidecar；它不改变 core，失败隔离最好，也最容易回退。

## 6. 未决与风险

1. **桥自身死亡**：进程内 OOB 全部失效；只能靠 systemd + 外部 sidecar 的
   「health 拉取失败」告警。这属于 supervisor 边界，core 不承诺自证。
2. **没有 `[console]` 就没有 pull**：`/healthz` 挂在控制台 HTTP 端口；不配控制台时
   只剩日志。若需要无控制台的 pull，需另加 loopback 健康监听（会增加一个端口与
   token 文件），首期不做。
3. **日志轮转**：当前单元使用 `append:` 文件，systemd 不自动轮转；需要外部
   logrotate。core 不重建日志设施。
4. **协议版本**：`version_compatible` 当前只认完全相等；`turn_end` 加字段需要升到
   协议 7。实现时应允许 v6 过渡（旧 harness 无 `failure` 字段时走现有路径），
   否则所有 harness 必须同时升级。
5. **后端整体宕机的判定**：没有结构化 `component=backend` 时，只能确认「harness
   不在线」；不能从自由文本断言 dsh/opencode 内部宕机。第 4 步之前不要过度承诺。
6. **模型默认值**：`session.model` 为空时，模型失败只能归到
   `model:<harness>/default`；不能归到某个具体模型。
7. **告警目标**：跨平台告警必须显式配置；core 不自动挑选存活连接，避免把平台策略
   写进 core。
8. **数值待拍板**：启动宽限 30s、down 冷却 600s、恢复阈值 60s、每房间 3 条/10 分钟、
   依赖表 256、迁移环 128、ntfy 每 10 分钟 6 条，都是实现默认，可按运维经验调整。
9. **in-band 恢复**：按当前口径默认关闭；若用户以后要求“平台恢复后在会话里说一声”，
   把它当作可选补充，不作为可靠性前提。

## 7. 参考

- 协议：[04-protocol.md](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/04-protocol.md)
- 适配器：[05-adapters.md](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/05-adapters.md)
- supervisor 与健康探针：[native-reload-and-supervisor.md](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/plan/native-reload-and-supervisor.md)
- 队列可观测性：[queue-backpressure.md](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/plan/queue-backpressure.md)
- 平台边界：[platform-boundary-proposal.md](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/plan/platform-boundary-proposal.md)
- 扩展点目录：[extension-point-catalog.md](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/docs/plan/extension-point-catalog.md)

关键代码位置（当前工作区）：

- turn 收尾与通知：`crates/kernel/src/engine.rs` 的 `end_turn`、`notify_turn_error`、
  `prepare_send`、`flush_notices`。
- 协议入口与断连：`crates/kernel/src/endpoint.rs` 的 `Conn::handle`、`Conn::abandon`、
  `run_connection`、`handshake`。
- 失败通知策略：`crates/kernel/src/plugin.rs` 的 `TurnErrorProvider`、
  `DefaultTurnErrorProvider`、`SendOrigin`、`PluginManager::catalog`。
- 配置：`crates/kernel/src/config.rs` 的 `Session::on_turn_error`、
  `default_on_turn_error`、`turn_timeout_secs`。
- 健康/控制台：`crates/bridge/src/console.rs` 的 `/healthz`、`/api/plugins`、
  `/api/status`；`crates/kernel/src/engine.rs` 的 `drain_snapshot`、`plugin_catalog`。
- 适配器：`crates/kernel/src/adapter.rs` 的 `AdapterHost`、`AdapterFactory::health`；
  `crates/adapters/onebot11/src/link.rs` 的 `supervise`；
  `crates/adapters/process/src/lib.rs` 的 `supervise`；
  `crates/adapters/dynamic/src/lib.rs` 的 `DylibAdapterFactory::health`。
- harness 侧错误上报：`harnesses/dsh/src/driver.mjs` 的 `turn_end` 收尾、
  `#pinModel`；`harnesses/opencode/plugin/acb-wire.mjs` 的重连与错误传播。
- 本机日志去向：`deploy/systemd/acb-bridge.service` 的 `StandardOutput` /
  `StandardError`。
