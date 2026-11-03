# 扩展点图谱、能力边界与上下文成本清单

状态：盘点 + 提案（2026-11-04）。**只出文档，不改代码。** 用于和后续「外部项目功能面
清单」合并成差距矩阵。

定位说明：这份文档回答四件事——今天插件能做什么、还缺哪些 hook、什么必须留在 core、
新增能力对 agent 上下文意味着什么。

评估口径（2026-11-04 用户裁定，后续设计与评审统一使用）：

1. **稳态运行成本**：每轮固定开销、随数据规模线性增长的返回体、额外往返次数。
2. **可观测性 / 可审计性**：日志能否对账、错误能否定位、是否丢失细节。
3. **接口面增长速度**：每加一个功能，是插件自己长，还是 core 又多一个入口/工具/字段。
4. **可变更性**：今天定下的 schema/协议会不会锁死后续改法；外部依赖一旦形成就难改。

**开发难度不作为否决理由**：多写几百行、迁移调用点、一次性重构、学习成本都不计入；
只要方向对，直接做。上下文经济属于第 1/2 项的权衡，不是独立最高目标。

## 1. 现状清单：扩展点与钩子

签名按 Rust trait 简化；「拦截 / 旁路」指能否改变主流程，「改写」指能否改变内容或目标。

| # | 扩展点 | 签名（简化） | 触发时机 | 拦截 / 旁路 | 改写内容 | 改写目标 |
|---|---|---|---|---|---|---|
| 1 | `ReceiveProvider` | `decide(&ReceivePolicy, &ReceiveRequest) -> Record \| Drop` | 记录入库前（路由未命中时） | **拦截**（Drop 后不写流水、不唤醒） | 否 | 否 |
| 2 | `RouteProvider` | `decide(&RoutePolicy, &RouteContext) -> Pass \| Drop{reason} \| Handle{store,wake,actions}` | 入库前、receive 之前；按连接顺序调用，首个非 Pass 生效 | **拦截 + 旁路**：Drop 或 Handle 可吃掉命令 | 可产生新 `Send` 元素；不能直接改原记录 | 可发到别的房间（仍受 access 校验） |
| 3 | `WakeProvider` | `decide / decide_with_reason(&WakePolicy, &WakeContext) -> Wake \| Stay + reason + delay_ms` | 记录入库后，对每个订阅者逐个判定 | 决定是否开轮、可延迟 | 否 | 否 |
| 4 | `AccessProvider` | `check(&AccessPolicy, &AccessRequest) -> Allow \| Deny` | 工具访问房间/连接前 | **拦截** | 否 | 否 |
| 5 | `DeliveryComposer` | `compose(&DeliveryPolicy, &DeliveryRequest) -> DeliveryPlan{keep, omitted}` | 开轮时，投递批次组装 | 决定投哪些编号 | 否（只选择/跳过） | 否 |
| 6 | `SendHook` | `before_send(ctx, &mut Vec<Element>) -> Result<(), PluginError>`；`after_send(ctx, &Sent)` | 发送前 / 发送成功后 | **拦截**（before 返回 Err 导致整次发送失败） | **可改写正文元素** | 否（room 已定） |
| 7 | `TurnErrorProvider` | `decide(&TurnErrorPolicy, &TurnErrorRequest) -> Vec<TurnErrorAction>` | 轮次失败收尾时 | 决定发哪些失败通知 | 只能选 Log / Send | Send 可指定房间 |
| 8 | `ClockProvider` | `is_due(&ClockPolicy, &ClockContext) -> bool` | 周期 tick / 空闲检查 | 决定是否开时钟轮 | 否 | 否 |
| 9 | `PlatformAdapterFactory` | `manifest() -> AdapterManifest`；`create(&Connection, EventQueue) -> AdapterInstance` | 启动时按配置 `kind` 实例化 | 拥有平台 I/O | 编解码平台内容 | 平台方法目录 |
| 10 | `ManagerPlugin` | `manifest / descriptors / instantiate / children / reload / shutdown / health / details / root` | 装载、轮次边界 reload、退出 | 管理子树生命周期 | 由子树 provider 决定 | 否 |
| 11 | `PluginLoader` | `manifest / install / unload / can_unload / inactive / reload` | 启动装载、热卸载/重载 | **拦截**（能否卸载/重载） | 否 | 否 |
| 12 | 声明式扩展目录 | `manifest.toml` + 规则；`Action = Pass \| Drop \| Send{room,elements,store,wake}` | 入库前路由 | 同 Route | 可产生 Send | 可指定房间 |
| 13 | 任务 handler | `task::run(&handler, &params)`（当前只认 `exec`） | 路由 `Task` 动作之后，route worker 执行 | 无插件扩展点 | 由 handler 自己 | 否 |
| 14 | 插件生命周期 | `PluginInfo{source,health,lifecycle,capabilities,...}`；控制台 `POST /api/plugins/*` | 目录展示 / 轮次边界执行 | 控制卸载/重载 | 否 | 否 |
| 15 | `RecordObserver` | `observe(&Record)` | 记录 append 之后 | 只读旁路 | 否 | 否 |
| 16 | `ToolProvider` | `tools() -> Vec<ToolSpec>`；`call(&ToolCallContext, tool, args) -> Value` | `plugin_tools` / `plugin_tool_call` 调用时 | 只读旁路（插件自己的 I/O 由插件负责） | 由工具自己 | 否 |

补充：`describe_plugins`、`extensions_list/read/write/reload`、`task_list/get`、
`feedback_*` 是**工具面**，不是扩展点；它们让 agent 观察/操作扩展与状态。

## 2. 今天就能用插件做的事（不需要改 core）

| 能力 | 用哪个钩子/载体 | 边界 |
|---|---|---|
| 消息改写（发送前） | `SendHook::before_send` | 可改正文元素；改不了目标房间 |
| 发送后审计/统计 | `SendHook::after_send` | 只能看回执，不能撤回 |
| 入站拒绝（机器人/噪声/黑名单） | `ReceiveProvider` | 只能整条 Drop，不能改内容 |
| 去重/限流（入站、发送） | `ReceiveProvider` + module 内存状态；发送用 `SendHook` 拒绝 | 跨重启状态需插件自己持久化 |
| 命令与后台任务 | `RouteProvider` + `RouteAction::Task{exec}` | 任务 handler 目前只有 `exec` |
| 定时任务/例行检查 | `ClockProvider` + `is_due` | 只能决定「开不开轮」，不能注入内容 |
| 唤醒策略（平台特有语义） | `WakeProvider`，读 `platform/event/tags` | 不能绕过轮次边界；直接唤醒需高信任 `RouteAction::Wake` |
| 投递裁剪（按规则跳记录） | `DeliveryComposer` | 只能选编号，不能改内容 |
| 失败通知 | `TurnErrorProvider` | 只能 Log / Send |
| 人设/提示词注入 | **做不完整**：delivery 只能选已有记录；人设由 core 配 | 缺口，见 §3 |
| 记忆 | core 的 `memory_*` 工具 | 插件无法注册自己的记忆工具 |
| 多平台转发 | `RouteProvider` 匹配 + `RouteAction::Send` 到别的房间 | 受 access 校验；不能跨会话发送 |
| 平台能力扩展 | `PlatformAdapterFactory` | 必须写适配器 crate |
| 工具面扩展 | `ToolProvider` + `plugin_tools` / `plugin_tool_call` | 插件工具按索引发现，schema 按需查 |
| 全量记录观察（统计/审计） | `RecordObserver`，append 后只读调用 | 不能改记录、不能开轮 |

## 3. 需要新 hook 才能做的事（缺口与建议签名）

| 缺口 | 今天为什么不行 | 建议 hook / 签名 | 优先级 |
|---|---|---|---|
| ~~观察**每一条**入站记录~~（已落地） | — | `RecordObserver`：`observe(&Record)`，append 之后同步只读；测试见 `observer_flow.rs` | 已完成 |
| ~~插件提供工具~~（已落地） | — | `ToolProvider`：`tools() -> Vec<ToolSpec>` + `call(&ToolCallContext, tool, args)`；通过 `plugin_tools` 索引发现，`plugin_tool_call` 调用 | 已完成 |
| ~~任务 handler 可扩展~~（已落地） | `task::run` 曾只认 `exec`（ADR-0029 D5） | `TaskHandler`：`handle(&self, task_id: &str, payload: &Value) -> Result<TaskOutcome, String>`；只给任务 id + payload，不暴露 Store/票/水位；任务记录仍是内存 200 条上限，不落库 | 已完成 |
| 跨轮次/跨重启状态 | 插件只能自己找地方存 | `PluginState`：`get/set/delete(namespace, key, ttl)`，由 core 提供作用域和上限 | 中 |
| 发送目标改写 | `before_send` 只给元素，room 已定；随便改会绕过 access | `SendTargetHook`：`fn rewrite_target(&self, ctx) -> Option<RoomId>`，core 收到后**重新跑 access** | 中 |
| 按会话速率限制（发送/入站） | 可用 send/receive hook 做，但没有统一状态 | 先用 `PluginState` + 现有 hook；暂不加新 hook | 低 |
| 群成员变更等平台事件的高层语义 | 适配器已能写 `kind=event` + tags；策略可读 | 无需新 hook；若要做成员索引，用 `RecordObserver` | 低 |
| 工具输出后处理（摘要/裁剪） | 工具结果直接回 harness | `ToolOutputHook`：`fn transform(&self, ctx, output: &mut Value)`，受硬预算约束 | 中 |
| 投递内容的展示变换 | composer 只能选编号 | `DeliveryTransform`：只允许改 `note`/summary 等展示层，不得改 `elements` 真值 | 低 |
| 消息编辑/撤回/表情等高层动作 | 只能走 `platform_call` + 方法目录 | 先用 adapter 能力 flags + `platform_call`；需要统一体验时加 `Action` 扩展 | 低 |

设计约束：新 hook 默认**同步、只读、不能写 store/水位/票**；需要 I/O 的动作仍走
`pending_route_actions` / action worker 或独立线程，避免把平台慢响应带进 core 锁内。

## 4. 必须留在 core 的不变量

| 不变量 | 为什么不能插件化 |
|---|---|
| 轮次边界（开轮/收工/热重载时点） | 决定插件快照何时生效；插件自己管会在一轮中途换实现 |
| 同一会话串行 + 票 | 串行键和票据是调度唯一性来源；插件无法跨会话协调 |
| ack / 水位 / 未读推导 | 唯一写入者必须存在，否则投递保证失效 |
| 引用判定（`quote_ref` / `reply_to_self`） | 需要平台编号反查与流水一致性 |
| 访问边界（订阅、跨会话、破坏性方法） | 安全闸门不能在插件里自我声明 |
| store 追加顺序与编号 | 水位和分页的前提 |
| 任务/投递计数与上限 | 预算与可观察性由 core 统一 |
| 插件生命周期与安全点 | unload/reload 的时序不能由被卸载方决定 |
| 配置解析与启动校验 | 启动失败必须早于任何插件副作用 |

原则：**插件可以改变策略和表达，不能改变账本和调度。**

## 5. 判据：新需求出现时怎么决定

> **改策略走插件，改不变量进 core；要新副作用/新数据形状，先加 hook，不把平台或业务
> 流程塞进 core。**

具体顺序：
1. 只影响「什么时候醒/收/发/失败怎么办/选哪批」→ 现有策略扩展点或新策略插件。
2. 需要新的**动作**（发、跑任务、改目标、编辑）→ 先看现有 `RouteAction` / `SendHook`
   能否表达；不能则加**通用动作**，动作执行仍由 core 做安全校验。
3. 需要新的**观察面**（每条记录、每个回执、每次工具输出）→ 加只读 observer hook。
4. 需要改**账本/调度/安全边界** → 进 core，不接受插件化。
5. 平台专有语义 → 适配器或策略插件；平台身份不进入通用 trait。

每一条都按四项口径复核：
- **稳态成本**：新 hook 是否让每轮多一次同步遍历/多一次往返？只读 observer 必须证明
  它不在热路径上做 O(n²) 或阻塞 I/O。
- **可观测性**：hook 的拒绝/跳过/异常必须留日志或计数；不能让插件静默吞掉输入。
- **接口面**：优先「一个通用 hook + 数据字段」而不是「每类需求一个专用入口」；
  能由插件自己长的能力（工具、任务 handler、状态）不要再给 core 加入口。
- **可变更性**：签名里不放平台名/业务枚举；用 `Value` + 版本化 schema 时要想清楚
  外部插件一旦依赖就难改，宁可先定窄接口。

## 6. Context budget：成本清单 + 可选优化

口径（按用户 2026-11-04 两次修正）：

- **排序**：正确性与不变量（投递保证、轮次边界、串行/票、引用判定、生命周期安全）
  高于上下文经济；上下文经济属于四项口径里的第 1/2 项，是**强约束**但不是最高目标。
- 不做默认全上的「省 token」改造。任何优化必须先给测量口径与前后对比：
  每轮工具调用次数、单次返回字节数、每轮进入上下文的工具描述字节数。
- 本节是**成本清单 + 可选优化**，不是硬指标；开发难度不计入评审。

### 6.1 工具/接口返回体

字节数为 JSON 序列化的大致范围（UTF-8，中文按 3 字节估）；「典型」按常见用法，
「最坏」按当前代码上限或注明「无上限」。

| 工具/接口 | 典型 | 最坏 | 随规模线性增长？ | 现有上限/分页 |
|---|---|---|---|---|
| `send_message` / `send_message_to` | < 200 B | < 500 B | 否 | — |
| `ack_messages` | < 100 B | < 1 KB | 否（ids 数线性） | 无 ids 上限 |
| `get_history` | 20 条 ≈ 4–16 KB | 200 条 ≈ 40–160 KB+ | **是**（条数 × 单条大小） | `limit` ≤ 200；单条内容无上限 |
| `fetch_records` | 少量 id ≈ KB 级 | **无上限** | **是**（ids 数 × 单条；且扫全房间流水） | 无 ids 上限 |
| `wait_message` | 0–几条 ≈ 几 KB | 受 limit/超时约束 | 是 | 默认/上限见参数 |
| `list_rooms` | < 1 KB | 订阅数 × ~100 B | **是**（房间数） | 无分页 |
| `get_notes` | 几 KB | notes 数 × 单条 | **是**（笔记数） | 无分页 |
| `set_note` / `del_note` | < 300 B | < 500 B | 否 | — |
| `get_platform_events` | 20 条 ≈ 几 KB | 200 条 | **是** | `limit` ≤ 200 |
| `platform_methods` | 摘要 ≈ 几 KB | 全量方法 × 连接数，可达几十 KB | **是**（连接 × 方法） | `connection` 过滤；无条数上限 |
| `platform_call` | 平台相关，通常 KB 级 | 500 条 × 单条 | **是** | `limit` ≤ 500，有 offset |
| `describe_plugins` | session 范围 < 2 KB | global 全插件目录，插件数 × 元数据 | **是**（插件数） | `extension` 过滤；无分页 |
| `extensions_list` | 模块数 × 元数据，几 KB | **无上限**（模块多/描述长） | **是**（模块数） | 无分页、无摘要模式 |
| `extensions_read` | 单文件 | ≤ 64 KiB | 否 | 64 KiB 硬上限 |
| `extensions_write` / `extensions_reload` | < 500 B | < 1 KB | 否 | — |
| `task_list` | 20 条 ≈ 4–8 KB | 100 条 ≈ 20–40 KB | 是，但有界 | `limit` ≤ 100；内存保留 200 |
| `task_get` | < 2 KB | < 4 KB | 否 | 单条 |
| `report_feedback` | < 300 B | < 1 KB | 否 | — |
| `feedback_list` | 20 条 ≈ 10–20 KB | **100 条 × 正文 4 KB + 回复** ≈ 400 KB+ | **是**（条数 × 正文/回复） | `limit` ≤ 100；正文/回复无列表级截断 |
| `feedback_get` | < 8 KB | 正文 4 KB + 回复 × 2 KB，**回复数无上限** | **是**（回复数） | 正文/单条回复有上限，条数无上限 |
| `feedback_reply` | < 500 B | < 1 KB | 否 | — |
| `memory_read` | 文件大小 | **无上限** | **是**（文件大小） | 无上限/无分页 |
| `memory_search` | 100 命中 ≈ 几 KB | 500 命中 | 是，但有界 | `limit` ≤ 500 |
| `memory_git_log` | 10 条 ≈ 几 KB | `limit` 无上限 | **是**（提交数） | 默认 10，无上限 |
| `memory_git_diff` / `blame` | 变更规模 | **无上限** | **是** | 无上限 |
| `memory_write` / `git_add/commit/push/branch` | < 1 KB | 几 KB | 否 | — |

另：给 harness 的**工具描述本身**是常驻上下文。当前 `TOOL_NAMES` 约 30+ 个，schema
描述是每轮固定成本；`tools.mjs` 之间的描述长度差异会直接进入首轮上下文。

### 6.2 事件/合成事件

| 事件 | 典型大小 | 最坏 | 说明 |
|---|---|---|---|
| `kind=task` 合成事件 | 几百 B | 结果 payload 可能到 KB | `result` 只做摘要？当前直接放 `Value` |
| `kind=feedback` 回复事件 | 几百 B | 回复 ≤ 2 KB | 回复正文进 elements + payload |
| 普通平台事件 | 平台载荷决定 | 无上限（适配器原样携带） | `payload` 不摊开，投递只给摘要 |

### 6.3 可选优化（每项都标注代价，不默认执行）

| 优化 | 省什么 | 代价 | 测量方式 |
|---|---|---|---|
| `feedback_list` 默认不返回 `body`/`suggestion`，只给标题、状态、回复数 | 最坏从 400 KB+ 降到 ~10 KB | agent 要多一次 `feedback_get`；可发现性下降 | 同场景调用次数 + 返回字节 |
| `extensions_list` 默认只给 id/extension/summary，详情按需 | 模块多时线性下降 | 多一次查询；调试时要翻两层 | 模块数 × 返回字节 |
| `fetch_records` 限制 ids 数量并提示分批 | 防止一次拉全房间 | agent 需分批；错误信息要教它怎么分 | 单次 ids 数、返回字节 |
| `memory_read` 加 head/tail/offset 或 64 KiB 截断提示 | 防大文件塞爆 | 多一次调用；截断会丢上下文 | 文件大小 vs 调用次数 |
| `memory_git_log` 加上限 | 防无限提交历史 | 翻历史要分页 | limit 与返回字节 |
| `platform_methods` 默认摘要、详情要求 `connection` | 连接/方法多时下降 | 多一次调用 | 连接数 × 方法数 |
| `get_history` 默认列已有精简，但单条文本仍无上限 | 可加单条字节预算 | 截断会改变事实，需明确标注 | 单条最大字节 |
| 工具按需注册（lazy tool exposure） | 省工具描述常驻成本 | 可发现性明显下降，agent 可能不知道能力存在 | 每轮工具描述字节 + 漏用率 |

**明确不建议默认做的**：给所有返回体加极简模式、把工具名/参数压成短码、默认裁剪
内容。它们会牺牲可调试性和表达力，除非测量显示成本确实是瓶颈。

### 6.5 外部成本报告基线（2026-11-04）

外部评审（`acb-context-cost-critique.md`）给出的实测基线，作为后续优化的对照：

| 项 | 基线 | 说明 |
|---|---|---|
| 工具数量 | 36 | `tools.rs::TOOL_NAMES` / `tools.mjs` |
| 工具定义序列化 | 30,855 bytes | `{name, description, schema}` 求和 |
| 工具定义估算 | **9,058 tokens/请求** | 描述 4,352 + schema 4,198；CJK 按 1 token/字近似 |
| 4 个请求的轮次 | ~36k tokens | 工具定义随每次请求重付 |
| 单工具平均 | 252 tokens | 长尾：`platform_call` 1,273、`platform_methods` 748、`report_feedback` 649 |
| 每条记录 `columns` 映射 | 20 条约 890 tokens | 与消息长度无关的固定浪费，已删除 |
| `platform_methods` 全目录 | 52 方法 ≈ 7.3 KB ≈ 2.9k tokens | 默认一次全给 |
| 共享说明复制 | `ROOM_SCHEMA`/`COLUMNS_DOC`/`ELEMENTS_DOC` 合计 ≈ 1,548 tokens | 同一段文字抄进多个 schema |

Pack 拆分（用于分层设计，不默认启用）：

| Pack | 工具数 | tokens |
|---|---:|---:|
| core（消息/读取/平台/反馈） | 11 | 5,541 |
| notes | 3 | 548 |
| memory | 12 | 1,206 |
| extensions | 4 | 608 |
| feedback 查询/回复 | 3 | 389 |
| task | 2 | 361 |
| describe_plugins | 1 | 405 |
| 全部 | 36 | 9,058 |

### 6.6 已实施与实验设计

**已实施（纯冗余/阈值触发，无语义变化）**

1. **删除每条记录的 `columns` 映射**：列定义只保留响应顶层 `columns` 数组。20 条记录
   固定省约 890 tokens；`get_history` / `fetch_records` 调用方仍能从顶层知道取了哪些列。
2. **目录/列表阈值索引**：`LIST_INDEX_BYTES = 4096`。超过阈值时自动降级：
   - `platform_methods` 默认清单 → name + 短 summary + risk（tags 省略、summary 截到 80 字）；
   - `feedback_list` → 去掉 body/suggestion/replies，保留标题、状态、回复数；
   - `task_list` → 去掉 result/error，保留 id/status/handler；
   - `get_platform_events` → 去掉 payload/elements，保留 id/at/sender/event/tags/summary。
   响应顶层加 `indexed: true` 与 `detail_hint`，详情走 `feedback_get` / `task_get` /
   `fetch_records` 或带 `group/tag/query` 的 `platform_methods`。阈值以下保持全文，
   不默认拆成两步。阈值取值依据：一次额外调用的成本（~9k 工具定义 + 历史）通常高于
   4 KiB 全文，只有超过 4 KiB 才值得换一次调用。

**待实验（先设计 + 基准，不默认启用）**

- **分层工具包**：按会话能力注册 core / notes / memory / extensions / feedback / task /
  describe_plugins 等 pack。基线 9,058 tokens；最小 core pack 约 5,541 tokens，
  不含 memory 的会话可省约 1,206，普通成员可省 extensions 约 608 + feedback 约 389。
  实现必须是**可开关的实验路径**，默认行为不变。
- **描述瘦身 / 共享 schema**：`ROOM_SCHEMA` / `COLUMNS_DOC` / `ELEMENTS_DOC` 合计
  ~1,548 tokens。需要先给测量口径（每请求工具描述字节、每轮调用次数、漏用率），
  再决定是否合并；未测量前不改默认工具集。
- **可发现性代价**：按需工具包会让 agent 不知道未注册能力存在。缓解方式是保留一个
  `capabilities`/`help` 入口或错误里给「该能力属于哪个 pack」；这会增加一次调用，
  必须先测量再上。

### 6.7 稳定短 id 与错误信息

- 反馈 id 是数字，稳定且短；任务 id 形如 `task-<票前缀>-<序号>`，前缀用于跨重启不撞号，
  比纯序号长但可读、可审计。按四项口径属于**可观测性**：可读前缀 + 短 id 的折中即可，
  不必为了省几个字节放弃唯一性/可对账性。
- 错误信息目前多数自带下一步（如 `[bad_params] kind 只能是 ...`、
  `[send_expired] ... Do not retry now`）。新增工具必须沿用这个标准，避免
  「报错后再查一次才知道怎么办」。

## 7. 与外部功能清单合并时的输出格式

建议差距矩阵每行包含：能力、外部项目做法、ACB 现有钩子、缺口类型
（插件即可 / 新 hook / 必须进 core）、上下文成本、优先级。这样功能面和上下文成本
不会各算各的。

## 8. 需要用户拍板

1. 新 hook 的首批范围：`RecordObserver` / `ToolProvider` / `TaskHandler` 是否三个一起做，
   还是先做 `TaskHandler`（ADR-0029 D5 已有承诺）。
2. `PluginState` 是否进 core（会引入作用域、TTL、容量上限与持久化选择）。
3. 6.3 中哪些优化先做测量、哪些直接不做。
