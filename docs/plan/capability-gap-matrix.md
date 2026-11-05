# 能力差距矩阵：astrbot / chatluna / bearbot vs ACB

状态：对照 + 判定（2026-11-04）。**只出文档，不改代码、不跑构建、不重启服务。**

输入：

- `feature-inventory.md`：三项目 151 条功能、39 条扩展点（文件/行号见原文）。
- `docs/plan/extension-point-catalog.md`：ACB 当前扩展点、core 不变量、context budget、判断规则。
- `docs/plan/platform-boundary-proposal.md`、`docs/plan/native-reload-and-supervisor.md`：边界与 supervisor 口径。
- 核实代码：`crates/kernel/src/plugin.rs`、`adapter.rs`、`tools.rs`、`outbound.rs`、`config.rs`、`engine.rs`。

评估口径（沿用 2026-11-04 用户裁定）：

1. **稳态运行成本**：每轮固定开销、随数据规模线性增长的返回体、额外往返。
2. **可观测性 / 可审计性**：日志能否对账、错误能否定位、是否丢失细节。
3. **接口面增长速度**：每加一个功能，是插件自己长，还是 core 又多一个入口/工具/字段。
4. **可变更性**：今天定下的 schema/协议会不会锁死后续改法。

**开发难度不作否决理由。不默认上省 token 优化。**

---

## 0. 结论先行

### 结论 1：平台适配器不进 core 可行，ACB 只在注册处出现平台名字

证据不是推测：

- chatluna 把 15 个 LLM 平台适配器做成独立包（`packages/adapter-*`，每个包 `plugin.registerClient(...)`），新增平台不改 core；这是“适配器全外置”的现成样本。
- ACB 已经走同一方向：`AdapterFactory` + `AdapterManifest` 在 `crates/kernel/src/adapter.rs`，适配器实现分别在 `crates/adapters/{onebot11,process,dynamic}`；core 只按配置的 `kind` 取工厂、校验 manifest、把入站队列交给适配器。
- core 对平台的认知只剩**数据**：`AdapterManifest.platform/kind/transports/capabilities/events/event_tags/supports_at/supports_reply`，以及 `WakeContext.platform`、`RouteContext.platform` 这类上下文字段。

严格核对后，core 业务文件里目前还有 2 处“平台名字/平台命名空间”出现，但都不是业务分支：

| 位置 | 现状 | 判断 |
|---|---|---|
| `crates/kernel/src/config.rs` `validate_wake_events` | 校验 `qq.*` 事件前缀与连接 `platform` 一致 | 平台命名空间声明，不是平台语义；可保留，但应由适配器提供事件名校验，避免继续长平台规则 |
| `crates/kernel/src/engine.rs` `platform_of` | 连接名 → 平台名，展示用 | 展示数据，允许 |

原先 `crates/kernel/src/outbound.rs` 的 `sample_string`（按 `group_id` / `uin` / `qq` 猜示例值）已改为 `ParamSpec.example` 并合入（commit `2a722af`），这项已清账：平台参数样例由适配器自带，core 不再猜语义。

判据不是“core 里出现平台字样为零”，而是：**新增一个平台，只写 adapter crate + 配置 + 注册，core 业务文件零改动。** 当前架构满足这条；上面 2 处列入清账清单，不要新增第 3 处。

### 结论 2：能力必须是可查表的数据，禁止 `get_platform_name()` 式分支与继承式差异

三个外部项目的反例正好说明两种错误做法：

- astrbot：`PlatformMetadata` 只有 `support_streaming_message` / `support_proactive_message` 两个布尔，其余差异靠平台名字符串落在核里（非适配器代码中 `get_platform_name() == / in / not in` 共 15 处），并且两个布尔默认 `True`，没有重写发送实现的适配器会被“当作支持”。
- chatluna：平台能力用“继承哪个 Client 子类”表达（`PlatformModelClient` / `PlatformEmbeddingsClient` / `PlatformRerankerClient` 及其组合，呈 2^n 形态），模型能力另有一套枚举；平台能力没有统一表。
- bearbot：平台知识集中在网关一个模块（长度上限、markdown 判定 `instanceof GatewayQQAdapter`、每平台一个 ingest 函数），没有通用能力位。

ACB 当前是 6 个粗粒度枚举 flag（`ReceiveEvents` / `Send` / `Catalog` / `Recall` / `Mention` / `RichMedia`），加上 `supports_at` / `supports_reply` / `events` / `event_tags` 几个数据字段。比 astrbot 的 2 个布尔好，但仍不是“可查表”：组合靠枚举，字段靠零散布尔。设计草案见 §3。

### 结论 3：扩展点数量不是目的，能在既有边界内改变行为才是

astrbot 有 18 类扩展点，但 pipeline 是**封闭**的：`stage_order.py` 硬编码 9 个阶段，`scheduler.rs` 用 `STAGES_ORDER.index(name)` 排序，未列入的 `Stage` 子类直接抛 `ValueError`。也就是说，插件可以往 9 个固定阶段里塞洋葱逻辑，却不能增加/替换阶段集合；扩展点数量没有换来“改 pipeline”的能力。

chatluna 的 32 个链式中间件则相反：顺序可声明、可插拔，但引入了 `before/after` 锚点、同层并发、环检测和启动失败风险，接口复杂度高。

ACB 应该走第三条路：

- 保留少量**正交决策点**（当前 13 个：Wake / Access / Receive / Route / Send / Clock / Delivery / TurnError / Observer / Tool / PlatformAdapter / Manager / Loader），每个决策点给足上下文数据。Observer 与 Tool 已落地（commit `adb89dc`）。
- 行为变化优先用**数据 + 通用动作**表达：`RouteAction::{Send, Task, Wake, Interrupt}`、`SendHook` 改写元素、`DeliveryComposer` 选批次、`WakeVerdict.delay_ms` 延迟开轮。
- 只有当“新的观察面”或“新的动作形状”现有接口确实表达不了时，才加 hook；**不为每条外部功能加一个入口**。
- 验收标准是“插件能不能在边界内改变行为”，不是“hook 数量是否追平”。

---

## 1. 判定图例与外部编号

外部功能编号规则：按 `feature-inventory.md` 各小节表格从上到下编号。

- 触发/唤醒 `A1–A11`；消息处理 `B1–B16`；定时任务 `C1–C9`；人设提示 `D1–D7`；记忆上下文 `E1–E12`；RAG `F1–F7`；工具调用 `G1–G11`；多平台 `H1–H7`；权限 `I1–I9`；群管风控 `J1–J10`；限流去重 `K1–K8`；富媒体 `L1–L18`；可观测 `M1–M12`；插件分发 `N1–N14`。合计 151。

判定标签：

| 标签 | 含义 |
|---|---|
| **已有** | core 现在就能做，不需要改代码；插件配置即可 |
| **插件可实现** | 现有扩展点 + 上下文数据足够，写插件即可 |
| **需要新 hook** | 现有钩子表达不了，需要新增只读 observer 或通用动作 |
| **必须进 core** | 改的是账本、调度或安全边界，插件不能碰 |
| **明确不做** | 不属于 ACB 边界，或与 core 不变量冲突 |

优先级：

- **高**：影响安全/正确性/核心扩展能力；不做会阻塞第三方插件或多平台接入。
- **中**：常见需求；现有钩子可组合实现，但体验或完整性有缺口。
- **低**：边缘、运维或模型侧；不影响 ACB 边界。

每行“四项口径备注”只记该行最关键的 1–2 项，不重复表头定义。

---

## 2. 能力差距矩阵

### 2.1 触发 / 唤醒（11 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| @ 机器人 / 引用触发 / 频道绑定触发方式（A1、A6） | **已有** | `WakePolicy.at_mention/reply/name_mention/events/event_tags/keywords/accumulate`；`WakeContext.at_self/reply_to_self/has_body`；订阅按房间配置。适配器用 `supports_at/supports_reply` 声明可信度 | 中 | 已有覆盖 mention/reply/name/event；`at_all` 与“all”触发方式未数据化，见 H/L 行 |
| 私聊直接唤醒（可选要求前缀）（A3） | **插件可实现** | `WakeProvider` 可读 `WakeContext.room` / `platform` / `tags`，按房间类型决定；core 没有“私聊恒醒”开关 | 中 | 平台无关：按房间数据判断，不按平台名分支 |
| 前缀触发并剥离前缀（A2） | **需要新 hook** | `WakeProvider` 只能决定醒不醒，不能改正文；`DeliveryComposer` 只能选编号。纯判定可插件，剥离需要入库前改写或展示层变换 | 中 | 接口面：应做通用“正文变换/展示变换”，不要为 QQ 前缀开专用字段 |
| 插件 handler filter 命中即唤醒（A4） | **插件可实现** | `RouteProvider::decide` 返回 `Handle{store,wake,actions}`；插件自己匹配命令/正则/参数 | 高 | 这是命令注册的替代路径；比在 core 建 handler 注册表更少接口面 |
| 会话等待中的后续消息放行 / 空 @ 等待 60s（A5、A9） | **插件可实现** | `WakeVerdict::wake_after(delay_ms)` 已支持延迟开轮；等待状态需插件自持，跨重启需 `PluginState`（缺口） | 中 | 稳态成本：等待器只在命中时空转；可观测性要求记录等待原因 |
| `!steer` 把消息插入进行中的回合（A7） | **明确不做** | 与轮次边界、同一会话串行/票不变量冲突（`crates/kernel/src/plugin.rs` 的 Route 只在入库前生效，不能往已开轮塞内容） | 低 | 可提供的替代是 `RouteAction::Interrupt` + 下一轮，而不是 mid-turn 注入 |
| 主动回复（按概率）（A8） | **插件可实现** | `ClockProvider` 决定是否开时钟轮；`RouteAction::Send` 可主动发消息；概率与冷却由插件状态维护 | 低 | 稳态成本：概率触发不应每轮扫全量历史；审计要记录“为什么发” |
| 机器人套娃禁止 / 黑名单拦截（A10） | **已有** | `ReceiveProvider::decide -> Drop`；`WakePolicy.ignore_speakers`；适配器可标记 self 消息 | 高 | 安全类；Drop 必须留日志/计数，不能静默吞 |
| 观测未回复消息（旁路通知）（A11） | **需要新 hook** | `RouteProvider` 可被 Drop/Handle 截断，`WakeProvider` 按订阅者判定，都不是“每条记录都看到”的通用观察点；需 `RecordObserver` | 高 | 热路径同步只读；必须证明不阻塞 I/O、不做 O(n²) |

### 2.2 消息处理与路由（16 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 9 阶段固定流水线 / 洋葱模型阶段（B1、B2） | **明确不做** | ACB 没有固定 stage 表；用独立决策点组合。把 astrbot 的固定阶段搬进来会重演“阶段集合不可扩展” | 低 | 这是反面样本，不是待补功能 |
| 插件优先、LLM 兜底的路由（B3） | **已有** | `RouteProvider` 先于入库/唤醒；`Handle` 可直接 `Send` 或 `Task`；`Pass` 后走正常唤醒 | 高 | 路由拒绝要带 reason；保持“插件先、agent 后”的单一顺序 |
| 命令注册（别名/参数） / 命令组 / 正则匹配（B4、B5、B6） | **插件可实现** | 全在 `RouteProvider` 插件内：`RouteContext.text`、`is_admin`、`room`、`sender`；命令树/别名是插件数据 | 高 | 接口面：不要为命令建 core 注册表；插件自己声明，失败走 `Drop{reason}` |
| 消息类型过滤 / 插件按会话启用 / 会话 LLM 开关（B7、B8、B9） | **已有**（B7）；**插件可实现**（B8/B9） | `RouteContext.kind/room/sender`；B8 读订阅与配置；B9 用 `WakeProvider` Stay 或 Route Drop + store | 中 | 会话开关必须仍入库；不要做成“丢消息” |
| 32 个具名链式中间件（B10） | **插件可实现** | 多数可拆到现有决策点：回复许可→Receive/Route，冷却/额度→插件状态，延时/合并→`WakeVerdict.delay_ms` + `DeliveryComposer`，思考提示→`SendHook`/`Task`，停止→`RouteAction::Interrupt` | 中 | 不引入 32 个 core 中间件；每个中间件若需新观察面再单独立项 |
| 声明式顺序求解（before/after/环检测）（B11） | **明确不做** | 顺序求解属于插件装载/组合层，不是 core 调度；core 已有配置顺序与 provider 解析 | 低 | 可变更性：一旦把 DAG 语义写进 core，后续很难改 |
| 同层并发执行 + stop 取消同批（B12） | **明确不做** | 与“同一会话串行 + 票”不变量冲突；core 调度不能交给插件 | 低 | 需要并发时应在不同会话，不是同一轮内并发 stage |
| 回合队列：同发送者/同角色合并成批（B13） | **已有** | `DeliveryComposer` + `unread` 累积；同一会话串行；`DeliveryRequest` 给 composer 选批次 | 高 | 批次大小受 `DELIVERY_OLDEST` / `MAX_DELIVERY_BYTES` 约束 |
| 中断策略 queue / steer（B14） | **已有**（queue/interrupt）；**明确不做**（steer） | `RouteAction::Interrupt{scope: Room/Sessions/Connection}`；steer 违反轮次边界 | 中 | 中断要写审计；不要新增“往轮内注入”的动作 |
| 绑定管理命令（attach/detach/new/status…）（B15） | **插件可实现**（只读/路由）；**必须进 core**（改配置） | `RouteProvider` + `is_admin` 可做查询与提示；配置变更只能走控制台/`config_patch`，工具和插件不能改配置/水位 | 中 | 安全边界：管理命令不能成为第二套配置写入口 |
| 单会话串行（B16） | **必须进 core** | 已有不变量：同一会话同时只处理一个回合；票是调度唯一性来源 | 高 | 不可插件化 |

### 2.3 定时与后台任务（9 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| Cron 定时任务（cron/date、立即运行、下次时间）（C1） | **插件可实现** | `ClockProvider::is_due` 可由插件计算 cron；任务记录与“下次时间”需 `PluginState`；执行体需 `TaskHandler`（缺口） | 中 | 稳态成本：tick 不应扫全量任务；持久化交给插件状态或外部 |
| 给 LLM 的定时任务工具（C2） | **需要新 hook** | 需要 `ToolProvider` 暴露工具 + `TaskHandler` 跑后台任务；`TOOL_NAMES` 写死在 core | 高 | 接口面：一个通用 ToolProvider 覆盖，不要每类工具加 core 入口 |
| 会话自动归档 / 归档清除 / 缓存过期（C3、C4、C5） | **插件可实现** | `ClockProvider` + 插件自有存储/`PluginState`；ACB 会话与记录持久化已有 core 负责 | 低 | 不要让 core 为某个插件的缓存表加清理逻辑 |
| 平台连接重试（1s→60s 退避）（C6） | **已有** | 适配器拥有连接、重连与生命周期（ADR-0027）；core 只实例化与收事件 | 中 | 退避状态在适配器；core 不感知平台连接细节 |
| agent 观察期计时 / 定期快照体检（C7、C8） | **明确不做** | 属于进程 supervisor/运维面；`native-reload-and-supervisor.md` 已定：core 只排空 + 退出码 + 探针，拉起/回滚由 systemd/外部 supervisor | 中 | core 不内置第二进程、不自重启 |
| 会话等待超时回调（C9） | **插件可实现** | `WakeVerdict.delay_ms` + 插件状态；超时动作可 `RouteAction::Send` 或 `Task` | 低 | 超时通知要可审计 |

### 2.4 人设与提示（7 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| Persona 注入与错误文案（D1） | **已有** | core 配置 `personas`，会话绑定 `persona`；`Delivery.persona: Option<PersonaInfo>` 把人格正文原样交给 harness；错误文案由 i18n | 高 | 桥只给语义，怎么塞进模型上下文由 harness 决定 |
| 子代理路由提示词（D2） | **明确不做** | 属于模型/harness 的 agent 编排；ACB 不持有模型会话 | 低 | 不做 subagent 编排、handoff 工具 |
| 系统提示词 / 上下文模板（D3） | **明确不做**（模板编排）；**已有**（数据面） | `Delivery` 已给 persona、memories、records、omitted/more；模板渲染是 harness 职责 | 低 | 避免把某家模型的消息格式写进 core |
| 预设 CRUD / 克隆 / 列目录（D4） | **已有** | 人格是 core 配置，控制台可增删改；插件不能改核心配置 | 中 | 不给插件第二套人格存储；插件自定义预设用 `PluginState` |
| Authors Note / Lore Book（D5） | **需要新 hook** | 现只有 `notes` 单入口工具（op=list/set/del），notes 不会自动进入投递；需要只读的上下文注入/展示注入 hook（可按关键词触发） | 中 | 注入内容与事实记录分开；不要把 note 混进 `elements` 真值 |
| `AGENTS.md` 作为记忆与提示（D6） | **已有** | 记忆 worktree + `memory_*` 工具；`Delivery.memories` 给 harness 路径 | 高 | 文件内容由 harness/agent 读；桥只锁路径 |
| `CLASSIFIER.md` 管理员补充规则（D7） | **插件可实现** | 插件可按 `RouteContext.is_admin/sender` 做策略；规则文件由插件自己持有 | 低 | 不要让 core 认识具体规则文件 |

### 2.5 记忆与上下文（12 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 会话 CRUD / 会话级 ACL（E1、E8） | **已有**（会话 CRUD）；**插件可实现**（ACL） | 会话/订阅/人格是 core 配置；ACL 用 `RouteProvider` + `is_admin` + 房间/发送者数据 | 中 | ACL 不是 core 账本；拒绝要留 reason |
| 平台消息历史 / 历史持久化（E2、E5） | **已有**（持久化/历史）；**明确不做**（插件侧导出） | core store 追加、`get_history` / `fetch_records`、水位与保留策略；导出属于运维/控制台，不是插件 | 中 | 返回体随条数线性增长，已有 limit；导出不塞进每轮上下文 |
| 上下文压缩 / 无限上下文压缩 / `/bb compact`（E3、E7、E10） | **明确不做** | 模型上下文压缩是 harness 职责；桥的 `Delivery` 只做批次选择与 `omitted/more` 提示 | 低 | 不在 core 做摘要；避免默认“省 token”改造 |
| 群聊上下文注入 / 观测缓冲 / `AGENTS.md` 重载（E4、E11、E12） | **已有** | `DeliveryComposer` 已按未读批次投递；store 本身就是有界观察缓冲（受保留策略约束）；记忆文件每轮由 harness 读取 | 中 | 不需要独立的“观察缓冲”子系统 |
| 上下文注入管线（可插的按序注入）（E6） | **需要新 hook** | 现有投递只有 persona/memories/records；插件无法追加自己的上下文片段（RAG、规则、Lore） | 高 | 接口面：一个通用注入 hook + 预算，不做每类注入一个入口 |
| 自动标题（E9） | **插件可实现** | `TaskHandler` + `PluginState`；标题写插件自有元数据，不改 core store | 低 | 每会话只生成一次，要有 claim 语义 |

### 2.6 检索 / RAG（7 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 知识库 CRUD + 解析/分块/混合检索 + 作为 LLM 工具（F1、F2） | **需要新 hook** | 检索子系统应由插件持有；暴露给 agent 需 `ToolProvider`；插件状态/索引需 `PluginState` | 中 | 不把 RAG 引擎搬进 core；工具描述是每轮常驻成本 |
| 向量库 / 嵌入 / 重排抽象与注册（F3） | **明确不做** | 属于模型侧/检索实现；插件可自带客户端，core 不建 provider 注册表 | 低 | 避免 core 认识厂商/SDK |
| 检索器与分数阈值（F4） | **插件可实现** | 插件内部实现，通过 `ToolProvider` 或注入 hook 使用 | 低 | 阈值策略不进 core |
| 长期记忆扩展 / RAG（F5、F7） | **已有**（F5）；**插件可实现**（F7） | 记忆 worktree + 文件/git 工具（`memory_*`）；RAG 按需插件，不为了齐平造 core 子系统 | 中 | 已有记忆是文件层，不是向量层；够用即可 |
| 多模态检索（图片描述）（F6） | **明确不做** | 模型/多模态侧能力；由插件或 harness 外部服务做 | 低 | 不在 core 兜底 |

### 2.7 工具调用（11 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| `@llm_tool` 注册 / `ToolSet` 抽象 / 通用 tool 插件（G1、G2、G9） | **需要新 hook** | `TOOL_NAMES` 写死在 `crates/kernel/src/tools.rs`；需 `ToolProvider`：`tools() -> Vec<ToolSpec>` + `call()`，core 只做校验/转发 | 高 | 接口面：一个通用工具面；工具描述每轮进上下文，必须可测量 |
| 内置工具集 / Agent 内工具（G3、G10） | **已有** | 当前 30+ 工具：send/ack/history/notes/platform/memory/feedback/task 等；`im_send`/`im_history` 有对应物 | 中 | 新增内置工具不应继续改 `TOOL_NAMES`，走 ToolProvider |
| 工具调用前后钩子（G4） | **需要新 hook** | 工具结果直接回 harness；需 `ToolOutputHook`（受硬预算约束）或只读观察 | 中 | 返回体改写会改变事实，必须标注；不要默认裁剪 |
| 子代理编排 / Agent 运行器注册（G5、G6） | **明确不做** | 模型/agent 运行时职责；ACB 不注册 agent 实现 | 低 | 与 D2 同一条边界 |
| 成员回合工具策略（G11） | **需要新 hook** | 现有 `AccessProvider` 只到 Room/Connection 粒度，没有“这个工具/参数能不能给这个成员用”；需 tool-access 决策或 ToolProvider 自带授权 | 中 | 安全类；拒绝要留审计，不能静默 |
| 工具注册/反注册 + 掩码 / MCP（G7、G8） | **需要新 hook**（ToolProvider）；**插件可实现**（MCP） | 注册/掩码由 core 控制暴露面；MCP 客户端在插件内，通过 ToolProvider 挂工具 | 高 | 掩码是安全边界，不能由插件自我声明 |

### 2.8 多平台适配（7 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 18 个平台适配器 / 适配器注册表（H1、H2） | **已有** | `AdapterFactory` + `AdapterManifest` + `PluginManager::register_adapter`；适配器是独立 crate，core 只认识 `kind` | 高 | 新增平台零改 core；实际平台覆盖靠 crate 数量，不靠 core |
| 4 个 IM 平台（telegram/qq/slack/console）（H5） | **插件可实现** | 每个平台写一个 adapter crate + 配置 + 注册；core 不硬编码平台集合 | 中 | 不学 bearbot 把平台写死在网关 |
| 15 个 LLM 平台适配器包（H3） | **明确不做** | 模型供应商适配属于模型侧；ACB 不做 LLM provider | 低 | 与 chatluna 不同定位 |
| 平台客户端注册/反注册（含能力类型）（H4） | **已有**（注册机制）；能力表待补（见 §3） | `register_adapter` 已有；`AdapterCapability` 是 6 个粗粒度枚举 flag，需扩成 §3 的 key-value 能力表 | 高 | 能力表是 manifest 数据，不在热路径 |
| 平台包装（状态/日志/超时）/ 连接状态面板（H6、H7） | **已有**（适配器职责 + 目录） | 连接生命周期在适配器；`describe_plugins` / `platform_methods` / 控制台展示 manifest 与目录 | 中 | core 不替适配器保管 socket |

### 2.9 权限与管理员（9 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 管理员名单 + 事件角色标记 / 权限过滤器 / 非管理员命令忽略（I1、I2、I8） | **已有**（I1）；**插件可实现**（I2、I8） | `Connection.admin_qqs`；`RouteContext.is_admin`；插件用 Route/Access 做过滤与拒绝 | 高 | 角色是数据；命令拒绝要留日志 |
| 插件级权限（`chatluna:admin`）定义与提供（I3） | **明确不做** | ACB 用 `PluginSource{Builtin,Trusted,Dynamic}` + `Capability` 授权，不建第三方权限树；插件不拥有“权限体系” | 低 | 避免第二套授权来源 |
| 管理命令组（I5） | **插件可实现** | `RouteProvider` + `is_admin`；命令是插件数据 | 中 | 与 B4/B5 同一套 |
| 共享管理员权限模型（按角色过滤工具）（I9） | **需要新 hook** | 需要 tool-access 决策（见 G11）；角色数据已有 | 中 | 安全边界；默认拒绝，显式放行 |
| 会话 ACL（I4） | **插件可实现** | `RouteProvider` / `AccessProvider` + 会话订阅 | 中 | 不是 core 账本 |
| 一次性管理员令牌 / 管理员增删列表命令（I6、I7） | **明确不做** | 控制台 token + `config_patch` 已覆盖管理面；插件不能改配置，core 不再造第二套 onboarding | 低 | 避免重复授权入口 |

### 2.10 群管理与风控（10 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 群/私聊白名单 / 黑名单用户 / 忽略自身与 @全体（J1、J5、J3） | **插件可实现** | `ReceiveProvider` Drop；`WakePolicy.ignore_speakers`；适配器可标 self；`@全体` 以 event tag 表达 | 高 | 安全类；Drop 必须可审计 |
| 内容安全（关键词 + 百度 AIP）/ 外部 censor（J2、J6） | **插件可实现** | `ReceiveProvider` + `SendHook` 调外部服务；具体厂商 SDK 留在插件 | 中 | core 不引入厂商 SDK；出站审查可阻断但要有原因 |
| Provider API 地址黑名单（J4） | **明确不做** | 模型供应商侧；ACB 不持有 provider 配置 | 低 | 与 J 类其他项不同，这条没有桥内落点 |
| 中止生成 / 回滚上一条（J7） | **已有**（中止）；**明确不做**（回滚） | `RouteAction::Interrupt` 可结束当前轮；会话回滚属于 harness/存储状态 | 中 | 中断不等于回滚；不要改账本 |
| 成员回合工具策略 / 管理员成员隔离 / 试运行标记（J8、J9、J10） | **需要新 hook**（J8）；**已有**（J9）；**插件可实现**（J10） | J8 需 tool-access；J9 由串行轮次 + 角色数据保证；J10 用 `PluginSource`/角色数据做插件策略 | 中 | J8 是安全边界；J10 不是 core 概念 |

### 2.11 限流与去重（8 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 会话限流（固定窗口） / 冷却 / 每日额度（K1、K2、K3） | **插件可实现** | `ReceiveProvider`（入站）、`SendHook`（出站） + 插件内存/`PluginState`；窗口与额度是插件策略 | 中 | 跨重启状态需 PluginState；拒绝要留计数 |
| 消息合并 / 延时（K4） | **已有** | `WakeVerdict.delay_ms` 延迟开轮；`DeliveryComposer` 合并批次 | 高 | 稳态成本：延迟窗口不应每次都扫历史 |
| 请求票去重 / 入站 ack 水位与乱序去重（K5、K6） | **必须进 core** | 已有不变量：票、ack、水位、未读推导、store 追加顺序 | 高 | 不可插件化 |
| 过期/编辑消息不触发 / 重复回复抑制（K7、K8） | **已有**（K7）；**插件可实现**（K8） | K7 由适配器标记 + `WakeProvider` Stay；K8 用 `SendHook::after_send` 记录已发文本 | 中 | K7 不要靠 core 猜平台；K8 记录有界 |

### 2.12 消息变换与富媒体（18 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 媒体路径映射 / 图片本地化 / 音频统一为 wav / 统一入站模型（L1、L2、L17） | **已有**（入站模型）；**插件可实现**（媒体处理） | 适配器拥有解码与本地化；`Element`/`Record` 是通用形状；core 只搬运 | 中 | 平台差异留在适配器，不进 core |
| 语音转文字（STT）（L3） | **插件可实现** | 适配器/TaskHandler 在入库前后处理；外部 STT 服务由插件调 | 中 | 若需入库前改写正文，则升级为“需要新 hook”；不要默认做 |
| 回复前缀 / 分段回复 / @回复与引用 / 空段清理（L4、L5、L9、L10） | **已有**（前缀/引用/空段）；**插件可实现**（分段多消息） | `SendHook::before_send` 可改元素；多消息分段需适配器或 `Task` 发多次；`Element` 已有 At/Reply | 中 | 分段会改变发送次数，必须可审计 |
| 语音合成（TTS）/ 文本转图片（t2i）/ 图片渲染插件（L6、L7、L14） | **插件可实现** | `SendHook` 替换元素 + 外部渲染/TTS 服务；renderer 是插件 | 低 | core 不内置渲染服务 |
| QQ 合并转发（L8） | **已有** | `platform_call` + 适配器方法目录（ADR-0020）；core 不认识 `Node` | 低 | 平台方法在适配器 |
| 消息渲染 Markdown→平台元素 / 平台 markdown（L11、L18） | **插件可实现** | 适配器/渲染插件负责；`SendHook` 可在发送前转换 | 中 | 不要按平台名在 core 分支 |
| 思考中提示发送/撤回（L12） | **插件可实现** | `Task` 发送 + `platform_call` 撤回；可选能力由能力表表达 | 中 | 撤回失败要显式报不支持 |
| 消息内容转换为模型可接收（L13） | **明确不做** | 模型输入格式由 harness/适配器决定；ACB 只投递记录 | 低 | 模型侧能力 |
| 平台长消息切分（L15） | **插件可实现** | 适配器或 `SendHook`；长度上限由能力表 `message.text_limit` 提供 | 中 | 数据化优于 `if platform == qq` |
| 附件落盘到工作区 inbox（L16） | **已有** | 文件中继（ADR-0025）与路径边界 | 中 | 安全边界：路径锁死在工作区 |

### 2.13 可观测与审计（12 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 事件级 trace / pipeline trace / 调用事件监听（M1、M2、M8） | **需要新 hook**（M1/M8）；**已有**（M2） | `wake_audit` 与轮次日志已有；每条记录/每次 hook 的观察需 `RecordObserver`，工具/发送事件可扩 after_send | 高 | 只读同步；必须证明不在热路径做阻塞 I/O |
| 每插件独立 logger（M3） | **插件可实现** | 插件自带日志；`PluginMetadata` 可带 id，core 日志格式可统一加前缀 | 中 | 不建 per-plugin 日志文件体系 |
| 日志轮转 / 环形缓冲 / 单行截断（M4、M11） | **明确不做** | 交给 journald/systemd 或外部 supervisor；core 不重造日志设施 | 低 | 与 native-reload 文档的 supervisor 边界一致 |
| 结构化诊断（M10） | **需要新 hook** | 插件可写 stderr，但没有统一诊断通道；可后续加只读 diagnostic 入口 | 低 | 先不做；不要为诊断加一堆 core 字段 |
| 插件 WebUI 页面（M5） | **明确不做** | 控制台是宿主界面；插件页面托管属分发/前端，不在本轮边界 | 低 | 与插件市场一起外置 |
| Token 用量/延迟指标 / 用量统计扩展（M6、M7） | **明确不做** | 模型侧指标；ACB 不调用模型，拿不到 token 用量 | 低 | 与 H3 同一条边界 |
| 出入站消息审计表（M9） | **已有** | core store 记录入站/出站；`wake_audit`；`SendHook::after_send` 回执 | 高 | 审计是 core 强项，不要搬到插件 |
| 上次应用/故障/维护态持久化（M12） | **明确不做** | 属 supervisor/部署状态；core 只提供 `/healthz` 与退出码证据 | 中 | 与 C7/C8 同一套外部 supervisor |

### 2.14 插件分发与热更新（14 条）

| 外部功能（编号） | ACB 现状 | 依据 | 优先级 | 四项口径备注 |
|---|---|---|---|---|
| 插件发现 / 元数据 / 配置 schema / 版本约束（N1、N2、N3、N4） | **已有** | `PluginLoader`、`PluginMetadata`（含 `options_schema`、`effects`、`docs_url`）、`PLUGIN_API_VERSION` / `ADAPTER_API_VERSION` | 高 | 版本不匹配要拒绝加载，错误要可定位 |
| 文件监听热更新 / 加载重载失败重载 / 启用停用（N5、N6、N8） | **已有**（重载/启停）；**插件可实现**（文件监听） | 轮次边界 reload（ADR-0029 D4）、`unload_provider`/`reload_provider`；文件监听可由外部工具调 `extensions_reload` | 高 | 热更新只发生在轮次边界；不能一轮中途换实现 |
| 安装/卸载/更新/依赖安装 / 插件市场 / npm 分发 / 生产 HMR（N7、N10、N11、N12） | **明确不做** | 供应链与安全边界；ACB 不做包管理器/市场/依赖安装；分发外置 | 低 | 不为了对齐 astrbot 把安装器塞进 core |
| 内置插件（N9） | **已有** | 内置 provider/适配器走同一套 manifest/能力/审计 | 中 | 内置与第三方同一契约 |
| AI 自写扩展 → 检查 → 应用 → 观察期 → 回滚（N13） | **已有**（部分）；**明确不做**（core 回滚） | `extensions_write`/`extensions_reload` 已让 agent 改扩展；观察期/回滚由外部 supervisor（native-reload 文档） | 高 | core 只写证据与退出码，不假装能回滚 native |
| 扩展能力协商（`apiVersion`）（N14） | **已有** | `ADAPTER_API_VERSION` / `PLUGIN_API_VERSION`；manifest 校验 | 高 | 版本号是合同，先窄后宽 |

### 覆盖核对

| 小节 | 覆盖编号 | 条数 |
|---|---|---|
| A 触发/唤醒 | A1–A11 | 11 |
| B 消息处理与路由 | B1–B16 | 16 |
| C 定时与后台任务 | C1–C9 | 9 |
| D 人设与提示 | D1–D7 | 7 |
| E 记忆与上下文 | E1–E12 | 12 |
| F 检索/RAG | F1–F7 | 7 |
| G 工具调用 | G1–G11 | 11 |
| H 多平台适配 | H1–H7 | 7 |
| I 权限与管理员 | I1–I9 | 9 |
| J 群管理与风控 | J1–J10 | 10 |
| K 限流与去重 | K1–K8 | 8 |
| L 消息变换与富媒体 | L1–L18 | 18 |
| M 可观测与审计 | M1–M12 | 12 |
| N 插件分发与热更新 | N1–N14 | 14 |
| **合计** |  | **151** |

判定分布（按行计，不按条数计；同一行内混合判定按主标签计）：已有 32 行、插件可实现 25 行、需要新 hook 14 行、必须进 core 2 行、明确不做 21 行。分布不是目标；关键是新增 hook 只落在“新的观察面/动作形状”上。

---

## 3. 能力表（capability table）字段设计草案

### 3.1 原则

1. **能力是数据，不是 trait 方法**：通用插件 trait 不带 `platform()` / `get_platform_name()`。
2. **平台名只出现在注册/manifest/上下文/记录/展示**，不作为 core 分支条件。
3. **禁止继承式差异**：不用“继承哪个 Client 子类”表达能力组合；用扁平 key-value 表查询。
4. **缺失即不支持**：没有默认 `true`；缺 key 时明确报 `unsupported_platform` / `not_implemented`，不静默降级。
5. **声明与实际分开**：manifest 声明（declared）、握手/账号协商后的实际（effective）、账号权限后的可用（account）三层都要能看到。
6. **core 只消费通用键**：平台私有的额外字段放 `extensions` 命名空间，core 原样展示、不解释。

### 3.2 表结构草案

```text
CapabilityTable {
  api_version: u32                 // 表结构版本，不是适配器版本
  adapter_id: string               // acb.adapter.onebot11
  kind: string                     // onebot11
  platform: string                 // qq（仅数据）
  declared:  Map<CapKey, CapValue> // manifest 静态声明
  negotiated: Map<CapKey, CapValue>// hello/握手后，适配器实际确认
  effective: Map<CapKey, CapValue> // 账号/权限过滤后的最终可用集
  extensions: Map<string, Value>    // 适配器私有，core 只透传展示
  provenance: "manifest" | "runtime" | "account"
}

CapValue = bool | i64 | string | string[] | CapabilitySchemaRef
CapKey   = 稳定字符串，不含平台名，例如 "message.text_limit"
```

### 3.3 首批字段

| CapKey | 类型 | 作用域 | core 通用消费点 | 缺失行为 |
|---|---|---|---|---|
| `message.text_limit` | i64（字节） | 连接/账号 | 发送计划与切分提示；适配器/插件据此分段 | 不猜；由适配器明确报参数超限 |
| `message.markdown` | bool | 适配器 | 目录展示；渲染选择 | 当作不支持 markdown |
| `message.markdown_dialect` | string | 适配器 | 展示/渲染提示 | 空 = 无方言 |
| `message.edit` / `recall` / `reaction` / `typing` | bool | 适配器/账号 | 方法目录与动作可用性 | 报 `unsupported_platform` |
| `message.thread` | bool | 适配器 | 房间/目标校验的可选分支（数据判断） | 不提供线程目标 |
| `message.forward` / `card` / `button` | bool | 适配器 | 目录与元素校验 | 不列相关方法 |
| `message.stream` | bool | 适配器/账号 | 流式发送计划 | 非流式 |
| `message.proactive` | bool | 适配器/账号 | 是否允许主动 Send/Clock 轮 | 只允许被动回复 |
| `message.at_all` | bool | 账号/群 | @全体能力展示与策略 | 不允许 @全体 |
| `message.quote` | bool | 适配器 | 引用回复可用性 | 不加引用元素 |
| `room.member_list` / `room.admin_ops` | bool | 账号/群 | 方法目录与管理动作可用性 | 不列相关方法 |
| `media.image` / `voice` / `video` / `file` | bool | 适配器 | 元素校验与方法目录 | 该类型元素报不支持 |
| `record.at_self_trusted` | bool | 适配器 | `WakePolicy` 是否可信 `at_self` | 调用方必须显式处理“不可信” |
| `record.reply_to_self_trusted` | bool | 适配器 | 同上 | 同上 |
| `event.tags` | string[] | 适配器 | 唤醒/投递语义标签 | 空 = 无标签 |
| `method.catalog` | bool | 适配器 | `platform_methods` / `platform_call` 是否暴露 | 不暴露目录 |
| `send.max_actions_per_min`（可选） | i64 | 连接 | 发送计划限速提示 | 不主动限速 |

> 注意：`message.text_limit` 是“数据”，core 读它做通用切分/提示，不读平台名；`extensions` 里的平台私有字段只用于 `describe_plugins`/控制台展示。

### 3.4 core 如何用（只允许这几种）

- **发送计划**：读 `message.text_limit` / `message.markdown` / `message.proactive`，做通用校验与提示。
- **方法目录**：读 `method.catalog` / `message.*` / `room.*` / `media.*`，决定目录里展示哪些方法、哪些动作明确报不支持。
- **唤醒/路由可信度**：读 `record.at_self_trusted` / `record.reply_to_self_trusted`；不可信时不要把 `at_self` 当事实。
- **目录/控制台展示**：读 `platform` / `kind` / `declared` / `negotiated` / `effective`，只展示，不分支。
- **错误**：缺能力返回五类错误（ADR-0020）：`unknown_method` / `unsupported_platform` / `not_implemented` / `forbidden` / `bad_params`。

### 3.5 禁止事项

- 禁止新增 `fn platform(&self) -> &str` 到 `WakeProvider` / `RouteProvider` / `ToolProvider` 等通用 trait。
- 禁止在 `engine.rs` / `outbound.rs` / `plugin.rs` 写 `if platform == "qq"` / `match platform`。
- 禁止用“继承某个 Client 子类”表达能力组合（chatluna 的 2^n 问题）。
- 禁止默认 `true`；缺能力必须显式报错。
- 禁止把平台私有字段塞进 core 语义路径；放 `extensions`，core 只透传。

---

## 4. 扩展点数量不是目的：astrbot 封闭 pipeline 对照

### 4.1 astrbot 的问题不在“hook 少”，在“阶段集合封闭”

- `astrbot/core/pipeline/stage_order.py:3-13` 硬编码 9 个阶段。
- `astrbot/core/pipeline/scheduler.py:25-27` 按 `STAGES_ORDER.index(name)` 排序；未列入的 `Stage` 在 `index()` 抛 `ValueError`。
- `astrbot/core/pipeline/bootstrap.py` 只注册固定阶段集合。

结果：插件可以用 `@register_stage` 往已知阶段塞洋葱逻辑，但不能新增/替换阶段；18 类扩展点没有换来“改 pipeline”的能力。再加上 core 内 15 处平台名分支，功能面很宽、边界却很硬。

### 4.2 39 条外部扩展点的形态归并

逐条位置见 `feature-inventory.md` §3；这里按形态归并，不逐条展开。

| 形态 | 外部条目（项目） | ACB 对应 | 判断 |
|---|---|---|---|
| 命令/过滤器唤醒 | astrbot `filter.command`/`command_group`/`regex`/`event_message_type`/`permission_type` | `RouteProvider` | 插件可实现；不建 core handler 注册表 |
| 阶段/管道扩展 | astrbot Pipeline `Stage` + `阶段集合` | `RouteProvider` / `SendHook` / `DeliveryComposer` 组合 | **明确不做**固定阶段表；机制用现有决策点 |
| 处理器/agent 替换 | astrbot `AdapterMessageEvent` handler、`@agent`；bearbot `pi.session_start` 接管连接 | `RouteProvider`（handler）；agent 运行器明确不做 | 插件可实现 / 明确不做 |
| 入站/出站中间件 | bearbot `InboundMiddleware`/`OutboundMiddleware`；chatluna `chain.middleware` | `ReceiveProvider` / `RouteProvider` / `SendHook` / `DeliveryComposer` | 已有/插件可实现；不引入通用 DAG |
| 生命周期通知 | astrbot `OnPluginLoaded/Unloaded/Error`、`OnAgentBegin/Done`；chatluna `chatluna/*`、`ctx.effect` | `PluginLoader` / `ManagerPlugin` / `RecordObserver`（缺口） | 插件生命周期已有；通用事件观察需新 hook |
| LLM 请求/响应钩子 | astrbot `OnLLMRequest`/`OnLLMResponse`/`OnUsingLLMTool`/`OnLLMToolRespond` | 无（模型侧）；工具前后可做 `ToolOutputHook` | 明确不做模型请求；工具输出后处理可选 |
| 适配器/客户端注册 | astrbot `register_platform_adapter`；chatluna `registerClient` | `AdapterFactory` + `register_adapter` | 已有 |
| 工具注册 | astrbot `@llm_tool`；chatluna `registerTool`；bearbot `pi.registerTool` | `ToolProvider`（缺口） | 需要新 hook |
| 命令面 | chatluna `ctx.command`；bearbot `/bb` | `RouteProvider` | 插件可实现 |
| 动作 API | bearbot `prompt`/`post`/`edit`/`delete`/`react`/`history` | `RouteAction::Send/Wake/Task/Interrupt`；`platform_call` | 已有/插件可实现；编辑/撤回走方法目录 |
| 结果装饰/发送后 | astrbot `OnDecoratingResult`、`OnAfterMessageSent`；chatluna 渲染/思考撤回 | `SendHook` / `Task` + `platform_call` | 已有/插件可实现 |
| 向量库/链注册 | chatluna `registerVectorStore`、`registerChatChain` | 不做 core provider 注册表；插件内部实现 | **明确不做** core 子系统 |
| 能力协商 | bearbot `apiVersion`；chatluna Client 子类 | `api_version` + §3 能力表 | 已有机制；能力表待补 |
| 故障/回滚编排 | bearbot `incident.Host`/`rollback.Host`/`Checker`/`Snapshot` | 外部 supervisor（native-reload 文档） | 明确不做 core |

### 4.3 ACB 应该怎么避免

1. **决策点少而正交**：11 个扩展点已经覆盖“醒不醒、收不收、路由到哪、发什么、发到哪、什么时候轮、投哪些、失败怎么办、平台 I/O、生命周期、装载”。新功能先问“能否落到其中一个”，落不到再谈新 hook。
2. **动作通用化**：需要副作用时优先扩 `RouteAction`（Send/Task/Wake/Interrupt）或 `SendHook`，不要为每个业务动作加一个 core 工具/入口。
3. **观察面统一**：需要“看到每条记录/每次工具输出/每次回执”时，加一个只读 observer，而不是每个功能一个专用回调。
4. **不建固定阶段表**：core 不维护 `STAGES_ORDER` 式枚举；顺序由配置/插件解析，core 只保证调度不变量。
5. **验收标准**：新增一条外部功能时，先回答“现有 hook + 数据能否改变行为”；答案是不能，且缺口属于“新观察面/新动作形状”，才加 hook。**不以 hook 数量齐平为目标。**

---

## 5. 明确不做

不为齐平而齐平。以下功能有外部实现，但 ACB 不做，理由是可验证的边界：

| 不做项 | 覆盖编号 | 理由 |
|---|---|---|
| 独立账号体系（登录、密码、协议、凭据、风控） | H1/H5 的账号侧 | 账号由外部适配器/OneBot 实现；core 不持有凭据。ADR-0020 也明确不列 `get_cookies`/`get_credentials` |
| 模型侧能力：LLM provider、模型路由、token 计量、用量报表 | H3、M6、M7 | ACB 是桥/宿主，不调用模型；这些属于 harness |
| prompt/persona 模板编排、上下文压缩、模型上下文管理 | D2、D3 模板、E3、E7、E10 | 桥只给 persona/memories/records 语义；怎么塞进模型由 harness |
| 子代理编排、agent 运行器注册、handoff 工具 | D2、G5、G6 | 模型/agent 运行时职责 |
| RAG 引擎、向量库/嵌入/重排 provider、图像描述检索 | F1 的引擎、F3、F6 | 检索实现属插件/外部服务；core 不建 provider 注册表 |
| 与轮次边界冲突：mid-turn steer、同轮并发 stage、插件自有调度/轮次生命周期 | A7、B12、J7 回滚 | 违反“轮次边界 + 串行/票”不变量 |
| 插件分发/安装/市场/依赖安装/生产 HMR/插件 WebUI | N7、N10、N11、N12、M5 | 供应链与安全边界；分发外置 |
| 进程 supervisor：自重启、观察期、定期快照、日志轮转/环形缓冲、部署回滚 | C7、C8、M4、M11、M12、N13 的回滚 | native-reload 文档已裁定：core 只排空 + 退出码 + 探针，拉起/回滚由 systemd/外部 supervisor |
| 厂商 SDK 进 core：百度内容安全、Provider 地址黑名单、TTS/t2i 渲染服务 | J2、J4、L6、L7、L14 | 具体厂商/服务留在插件；core 只提供通用 hook |
| 一次性管理员令牌/另一套管理员增删体系 | I6、I7 | 控制台 token + `config_patch` 已是唯一管理写入口；不造第二套授权 |
| 固定 9 阶段 pipeline / 阶段集合扩展 | B1、B2 | astrbot 反面样本；ACB 用正交决策点 |
| 把 4 个 IM 平台写死在 core/网关 | H5、H6 | 适配器 crate + 注册，不进 core |
| 为每条外部功能新增一个 core 工具/入口 | 全部 | 接口面增长不可接受；优先通用 hook + 数据 |

---

## 6. 四项口径复核与上下文成本

### 6.1 新增 hook 的成本

| 新 hook | 稳态成本 | 可观测性 | 接口面 | 可变更性 |
|---|---|---|---|---|
| `RecordObserver` | 每条记录一次同步只读；必须 O(1)、禁止阻塞 I/O | 观察者可上报计数/诊断；core 不静默吞 | 一个通用观察面，覆盖统计/审计/索引 | 签名只给 `RecordContext` + `&Record`，窄 |
| `ToolProvider` | 工具描述每轮进上下文；工具越多固定成本越高 | 工具调用/失败要进本轮记录 | 一个通用工具面，替代每类工具入口 | `ToolSpec` 是外部合同，先窄、要版本化 |
| `TaskHandler` | 后台任务 worker；任务记录是内存态 | 任务结果/失败要可查；drain 时明确丢失 | 一个通用动作，替代每类后台任务 | handler 名 + params schema 要稳定 |
| `PluginState` | 作用域 + TTL + 容量上限；可能落盘 | get/set/delete 要可审计 | 一个通用状态面 | 命名空间/键格式一旦发布难改，先窄 |
| 上下文注入 / 展示变换 | 每轮可能增加注入内容；需硬预算 | 注入来源/长度要可查 | 一个通用注入面，不做每类注入 | 必须区分“展示层”与 `elements` 真值 |
| tool-access 决策 | 每次工具调用一次判定 | 拒绝必须留 reason/审计 | 一个通用安全面 | 默认拒绝，显式放行 |

### 6.2 上下文成本

- 当前 `TOOL_NAMES` 约 30+ 个，工具描述是每轮常驻成本；`ToolProvider` 新增的工具会继续增加这部分。**不默认做 lazy tool exposure / 极简返回 / 短码**；先测量“每轮工具调用次数、单次返回字节、工具描述字节”，再决定是否优化。
- 工作区已有 `LIST_INDEX_BYTES`（4 KiB）自动把大列表降级为索引（`task_list` / `events` / `platform_methods` / `feedback_list`）。这不是本矩阵提出的默认优化，但评审时要按四项口径复核：可发现性是否下降、是否多一次 `*_get`/`fetch_records`、`indexed` 提示是否足够。
- `RecordObserver` 不在工具上下文里，但它在热路径；必须证明不阻塞、不做 O(n²)。
- 能力表是 manifest 数据，不在每轮热路径；成本可忽略。
- 上下文注入 hook 一旦允许插件塞大段内容，必须有硬预算与截断提示；否则最坏返回体会线性增长。

### 6.3 四项口径结论

- **稳态成本**：能力表零热路径成本；RecordObserver 是唯一需要严格证明的同步热路径；ToolProvider 的成本主要进上下文，必须测量。
- **可观测性**：所有拒绝/跳过/异常必须留日志或计数；`Drop` 不能静默。`WakeVerdict.reason`、`RouteDecision::Drop{reason}`、`SendHook::after_send`、`TurnErrorProvider` 已是可审计基础。
- **接口面**：优先“一个通用 hook + 数据字段”；能由插件自己长的能力（工具、任务 handler、状态、命令）不再给 core 加入口。
- **可变更性**：能力表 key、ToolSpec、PluginState 命名空间都是外部合同；先定窄接口，避免平台名/业务枚举进 schema。

---

## 7. 待用户拍板的高优先级项

1. ~~**`ToolProvider` / `RecordObserver` / `TaskHandler` 的首批范围**~~：已代行决策——先做 `ToolProvider` + `RecordObserver`（commit `adb89dc`），`TaskHandler` 随 T8；`PluginState` 不进 core。
2. **`TaskHandler` 的签名与任务记录持久化**：随 T8 做时再定 handler 名/参数 schema 与内存记录丢失的边界。
3. **能力表首批字段**：是否先补 `message.text_limit` / `message.edit` / `message.recall` / `message.reaction` / `message.thread` / `message.at_all` / `message.quote` / `message.proactive`，还是等真实适配器需求再补？一旦发布会被外部适配器依赖。
4. **能力表的“声明 vs 实际”是否都暴露**：`describe_plugins` / `platform_methods` 是否同时展示 declared / negotiated / effective，并要求缺失能力显式报错（不做默认 `true`）。
5. **上下文注入 hook 的形态**：只做展示层 `DeliveryTransform`（不改 `elements` 真值），还是允许受预算约束的 `ContextInjection`（插件提供 RAG/规则/Lore 片段）？这决定 D5/E6 的实现方式。
6. **tool-access 决策是否进 core**：G11/I9/J8 都指向“按角色/成员过滤工具”，但现有 `AccessProvider` 只到 Room/Connection。是扩 `AccessAction`，还是加独立 `ToolAccessProvider`？
7. **是否接受核心清账清单**：`config::validate_wake_events`（`qq.*` 前缀校验）、`engine::platform_of`（展示）两处是否按“数据/展示”保留，并禁止新增平台名分支？`sample_string` 已迁走（`2a722af`）。
8. **插件分发边界**：N7/N10/N11/N12 明确不做，是否也排除“插件 WebUI 页面”（M5）与“生产 HMR”，只保留 `extensions_*` 工具 + 控制台？
9. **回滚边界**：N13 的“观察期/回滚”是否完全交给外部 supervisor（native-reload 文档），core 只写 `current.json` 证据、不写 `last-good`？
10. **是否把 `message.text_limit` 等能力值作为 core 的通用发送计划输入**：若接受，core 会读数据但不按平台分支；若不接受，则切分完全留给适配器/插件，core 只报参数错误。

