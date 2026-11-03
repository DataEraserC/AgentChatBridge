# 执行进度日志（append-only）

用途：抗意外中断、审计与恢复入口。每完成一个可独立验证单元追加一条；不要改写历史条目。

## 2026-11-04 01:19 CST — A：递归卸载 manager 子树

- 任务编号：A
- 做了什么：
  - `crates/kernel/src/plugin.rs`
  - `uninstall_manager` 改为：先 `children()` 递归收集子树，再按“子先父后”逆序
    `shutdown` 并清理 descriptors / wake / access / delivery / send / turn_error /
    clock / receive / route / adapter_providers。
  - 新增 `collect_manager_namespaces`，用 `HashSet` 防运行期 `children()` 环路。
  - 新增测试 `uninstalling_parent_removes_child_providers_and_routes`：
    子 manager 同时注册 wake + route，卸载 parent 后两者都不可解析、descriptor 不残留、
    `manager_health()` 为空。
- 证据：
  - `cargo test -p acb-kernel --lib` → 159 passed。
  - `cargo test --workspace` → 全部通过（41 个 test binary/suite 汇总，0 failed）。
- commit：`830f21f fix(plugin): recursively uninstall manager subtrees`
- 偏离说明：没有先引入 effect 栈；用最小递归收集满足 A。可扩展性偏好只部分满足
  （新增扩展点仍需改 `retain` 列表），已记录为 B/C 之外的后续 effect 栈方向。
- 下一步入口：B 文档；读 `engine.rs::plugin_references`、`config.rs` 结构定义。
- 未决问题：是否批准 effect 栈内部化（见 Cordis 评估）。

## 2026-11-04 01:21 CST — B：`plugin_references` 声明式收集设计

- 任务编号：B
- 做了什么：新增 `docs/plan/declarative-provider-references.md`。
  - 当前手写扫描位置：`crates/kernel/src/engine.rs:484-527`。
  - 最小方案：`Config::provider_references()` + `ProviderReference { id, location }`，
    各配置结构内部收集；engine 只过滤。
  - 兼容性：配置/JSON/C ABI 不变；风险是新增 provider-bearing 字段仍需手工加一行。
  - 备选 derive 宏方案的成本与暂不采用理由已记录。
- 证据：文档评审；未改代码。
- commit：`5701f05 docs(plan): design declarative provider references`
- 偏离说明：不一次性改成 derive 宏，换取小步、可评审；可扩展性偏好部分满足。
- 下一步入口：C 文档；读 `docs/adr/0030-unload-safe-native-plugin.md` D3。
- 未决问题：是否批准最小方案落地。

## 2026-11-04 01:22 CST — C：ADR-0030 D3 决策选项

- 任务编号：C
- 做了什么：新增 `docs/plan/native-reload-rollback-options.md`。
  - 方案 A：候选 `.so` 预加载探针（临时路径/独立 namespace、双库、探针 create/destroy）。
  - 方案 B：明确同路径 native reload 不可逆；失败保持 unloaded + `lastError`。
  - 对比实现成本、平台风险、RSS 峰值、测试成本、对 AI native 迭代的影响。
  - 推荐当前采用 B、A 作为后续可选增强；未改 ADR 原文。
- 证据：文档评审；未改代码。
- commit：`dcd214f docs(plan): record native reload rollback options`
- 偏离说明：没有为了“满足 ADR 原文”直接选高风险双库方案；以可验证、低风险为优先。
- 下一步入口：D（T7）；先读 `docs/plan/dynamic-extensions-and-fixes.md` T7、
  `crates/kernel/src/route_worker.rs`、`crates/kernel/src/task.rs`、`engine.rs` 工具分派。
- 未决问题：用户需在 A/B 方案中拍板；T7 是否接受 T7a/T7b/T7c 拆分。

## 2026-11-04 01:31 CST — D/T7a：任务记录与查询

- 任务编号：D / T7a
- 做了什么：
  - `crates/kernel/src/engine.rs`
    - 新增 `TaskStatus` / `TaskRecord` / `Bridge::tasks` / `task_seq`；
    - `create_task_record`、`mark_task_running`、`finish_task`；
    - `task_list` / `task_get`，按会话隔离，任务结束后仍可查询；
    - route `task` 动作创建记录并写入 `PreparedTask.task_id`。
  - `crates/kernel/src/route_worker.rs`
    - 任务开始/结束写回 Bridge；成功/失败/发送失败/候选不支持都落记录；
    - 参数摘要只保留前 200 字符，避免把长命令/环境原样塞回模型。
  - `crates/kernel/src/tools.rs`、`harnesses/shared/tools.mjs`
    - 增加 `task_list` / `task_get` schema。
  - `crates/kernel/src/lib.rs` 导出 `TaskRecord` / `TaskStatus`。
- 证据：
  - `cargo test -p acb-kernel --test task_flow` → 2/2。
  - `cargo test --workspace` → 全部通过（42 个 test 汇总，0 failed）。
  - `npm test` dsh 34/34、opencode 42/42；`node --check harnesses/shared/tools.mjs` 通过。
- commit：`0b39dbd feat(task): record background task status and results`
- 偏离说明：
  - 任务记录先放进程内、上限 200，不落库；换取小步可验证，持久化/重启保留留给 T7 后续。
  - T7 拆成 T7a（记录/查询）、T7b（合成事件/wake_on/自触发防护）、T7c（feedback reply），
    先交付可独立验证的查询面；合成事件与唤醒尚未做。
- 下一步入口：T7b；读 `engine.rs` 的 `ingest` / `enqueue`、`task_worker` 完成路径、
  `RouteContext` 与 `TurnReason`，设计 `kind=task` 合成事件与 `wake_on`。
- 未决问题：`task` 记录是否需要在 SQLite 持久化；`wake_on` 默认值；直接唤醒 API 的限流策略。

## 2026-11-04 01:38 CST — B 落地：provider 引用集中收集

- 任务编号：B（代行决策后实施）
- 代行决策：采纳 `Config::provider_references()` 最小方案。理由：不改配置/JSON/C ABI；
  把手写扫描从 engine 收敛到 config 结构旁；增量风险可控。可回退：revert 本提交即可，
  没有 schema/兼容性迁移。
- 做了什么：
  - `crates/kernel/src/config.rs`
    - 新增 `ProviderReference { id, location }`；
    - `Config::provider_references()` 集中收集 receive / route / access / delivery /
      outbound / turn_error / wake / clock，按 id+location 排序去重。
  - `crates/kernel/src/engine.rs`
    - `plugin_references` 改为过滤 `config.provider_references()`，不再手写遍历。
  - 测试：
    - `config::tests::provider_references_cover_all_current_fields`：8 个位置全覆盖。
    - `plugin_lifecycle::access_reference_also_blocks_unload_and_reload`：
      access 引用会拒绝 unload/reload；未引用路径仍由既有测试覆盖。
- 证据：
  - 目标测试：config 1/1、plugin_lifecycle 9/9。
  - `cargo test --workspace` → 42 suites 全通过，0 failed。
- commit：`c8908aa refactor(config): centralize provider reference collection`
- 偏离说明：没有引入 derive 宏；接受“新增 provider-bearing 字段仍需手工加一行”的残余风险，
  换取零新依赖、零配置变更。可扩展性偏好部分满足。
- 下一步入口：C 的注释/文档标注；之后 T7b。

## 2026-11-04 01:39 CST — C 代行决策：native reload 按方案 B 运行

- 任务编号：C（代行决策）
- 代行决策：暂不改 ADR-0030 原文；同路径 native reload 按方案 B 的现实语义运行：
  失败保持 `unloaded` + `lastError`，不声称回滚到上一份实现。理由：用户明确要求暂不改
  ADR；方案 B 与 T4c 现状一致，零代码/零兼容性风险。可否回退：只涉及注释/文档，
  删除注释即可回退；ADR 未动。
- 做了什么：
  - `crates/adapters/dynamic/src/lib.rs` `reload` 上方加注释，标明 ADR-0030 D3 的
    “回滚”在当前同路径 reload 下不成立，并指向决策文档。
  - `docs/plan/native-reload-rollback-options.md` 增加“代行决策记录”与建议调整。
- 证据：`cargo test --workspace` 在 B 落地后已全通过；本步只改注释/文档，未改行为。
- commit：`6700fce docs(dynamic): mark native reload rollback as unimplemented`
- 逐条对照：没有假装满足回滚；已知不满足的引用位置是 ADR-0030 D3 与
  `adapters/dynamic/src/lib.rs` 的 `reload` 注释。
- 下一步入口：T7b；读 `task_flow.rs`、`route_worker.rs`、`engine.rs` 的 route/wake 路径。

## 2026-11-04 01:47 CST — T7b-1：任务合成事件与 wake_on

- 任务编号：D / T7b-1
- 代行决策：
  - `wake_on` 只作为合成事件的发射过滤器（never / error / always），不直接影响 wake 策略；
  - 缺省 `always`：任务完成默认发事件，但是否开轮仍由现有 wake/clock 策略决定；
  - 任务记录继续内存 + 200 条上限，暂不上 SQLite。
  - 理由：用户明确指定 wake_on 语义；默认 always 让任务完成可观察，但不绕过策略；
    内存记录换取 T7b 阶段的可迭代性，持久化等真实跨重启需求。
  - 可否回退：把 `create_task_record` 的缺省 wake_on 改成 `never`、或删除合成事件调用即可；
    不改配置格式/JSON wire/C ABI。
- 做了什么：
  - `crates/kernel/src/engine.rs`
    - `WakeOn` 枚举；`TaskRecord` 新增 `wake_on`、`event_id`、`event_woke`、`event_error`；
    - `ingest` 拆出 `ingest_inner(draft, skip_route)`；合成事件跳过 route/receive，只走
      store + wake 策略；
    - `finish_task` 按 wake_on 发射 `kind=task` 合成事件，写回事件 id / 唤醒会话 / 错误。
  - `crates/kernel/tests/task_flow.rs`
    - `task_records_success_failure_and_can_be_queried_after_turn_end` 增加：
      always 成功/失败都发事件；never 成功不发；error 成功不发、失败发；
      合成事件误入 route 会再触发任务，用 total 计数抓自触发循环；
    - `task_event_wakes_through_existing_policy`：`event_tags=["task.failed"]` 时，
      现有 wake 策略确实开轮，`event_woke=["main"]`。
  - `harnesses/shared/tools.mjs` 描述补 wake_on / 事件字段。
- 证据：
  - `cargo test -p acb-kernel --test task_flow` → 3/3。
  - `cargo test --workspace` → 42 suites 全通过，0 failed。
  - dsh/opencode `npm test` 通过；`node --check harnesses/shared/tools.mjs` 通过。
- commit：`928715b feat(task): emit synthetic task events filtered by wake_on`
- 偏离说明：默认 `always` 会让每个任务在流水里多一条合成事件；换取默认可观察性。
  如果用户觉得噪音大，可回退为 `never` 或只让 `error` 发。
- 下一步入口：T7b-2 直接唤醒 API（高信任模块、审计、频率限制）；读 `PluginManager` 的
  wake/clock 注册与 `Bridge::enqueue`。
- 未决问题：直接唤醒 API 的信任标记、限流窗口；T7c feedback reply 的事件复用。
- 新增待办（优先级低于 T7b）：`docs/plan/native-reload-and-supervisor.md` 设计文档，
  讨论 core 排空+退出、外部 supervisor 重启、版本/插件回滚边界。仅出设计，不改代码/
  systemd/服务。完成后在本日志补记录。

## 2026-11-04 01:59 CST — T7b-2：高信任模块直接唤醒

- 任务编号：D / T7b-2
- 代行决策：
  - 通道用 `RouteAction::Wake`，复用现有路由动作管线；不改 C ABI / JSON wire / 配置格式；
  - 信任闸门 = 载体来源 builtin / trusted / dynamic + descriptor 声明 `Capability::WakeRequest`；
  - 限流固定「每来源 6 次 / 60 秒」、审计保留 100 条，先不进配置；
  - 审计暴露 `Bridge::wake_audit()` 与 `GET /api/plugins` 的 `wakeAudit` 字段。
  - 理由：高信任模块目前能产生副作用的唯一稳定通道就是路由动作；换成新扩展点或
    回调 ABI 会同时改 native 插件合同，收益低于成本。固定常量先保证可观察、可回退，
    等真实调优需求再配置化。
  - 可否回退：删除 `RouteAction::Wake` 与 `Capability::WakeRequest` 两个变体、对应
    引擎分支与测试即可；不触碰既有 provider / 配置 / 动态库 ABI。
- 做了什么：
  - `crates/kernel/src/plugin.rs`
    - 新增 `Capability::WakeRequest`（`wake_request`）；
    - 新增 `RouteAction::Wake { session, room, reason }`。
  - `crates/kernel/src/engine.rs`
    - 路由决策记录来源 provider id；`apply_direct_wake` 做信任、订阅、频率检查；
    - `WakeAuditEntry` / `WakeRequestOutcome` 与 100 条审计环；
    - `Bridge::wake_audit()` 最新在前的 JSON 视图；
    - 限流：每来源 6 次 / 60 秒（`WAKE_REQUEST_LIMIT` / `WAKE_REQUEST_WINDOW_MS`）。
  - `crates/bridge/src/console.rs`：`/api/plugins` 响应附带 `wakeAudit`。
  - `crates/kernel/tests/direct_wake.rs`（新）：直接唤醒绕过 wake 策略、未声明能力
    被拒、错误房间/会话被拒、第 7 次限流、省略 session 时唤醒全部订阅者。
- 证据：
  - `cargo test -p acb-kernel --test direct_wake` → 5/5；
  - `cargo test -p acb-kernel --test task_flow` → 3/3；
  - `cargo test --workspace` → 全通过（exit 0）；
  - `cargo fmt --all -- --check` 无差异。
- commit：`44816e1 feat(wake): add audited direct wake for high-trust providers`
- 偏离说明：Lua / 声明式模块即使构造同样的 `Wake` 动作也会被信任检查拒绝；不为 Lua
  增加解析入口，避免把直通能力漏给 T1 沙箱。代价是这两种载体暂时只能走合成事件。
- 未决问题：审计是否持久化、限流是否按会话细分、控制台是否要做审计 UI。
- 新增待办（已完成 T7b 后处理）：
  - `docs/plan/native-reload-and-supervisor.md`：排空+退出、外部 supervisor、版本/
    插件回滚边界。只出设计。
  - `docs/plan/platform-boundary-proposal.md`：平台实现禁止进 core、平台无关能力
    抽象必须留在 core；附 ADR 措辞建议与复核位置。只出文档，等待用户确认。
  - `docs/plan/extension-point-catalog.md`：现有扩展点/钩子清单、今天能做的插件能力、
    缺口 hook、必须留在 core 的不变量与判断规则。只出文档。

## 2026-11-04 02:32 CST — T7c：反馈查询、回复与通知闭环

- 任务编号：D / T7c
- 代行决策：
  - `feedback_list` / `feedback_get` 只读当前会话写过的反馈；跨会话回复需要管理员身份
    （按轮次来源发送者命中 `admin_qqs`），普通会话只能回复自己的反馈；
  - 回复把 open 推进到 acked；其余状态仍由控制台显式流转；
  - 回复发 `kind=feedback` 合成事件（tags `feedback.reply`），并直接唤醒原会话，
    不依赖 wake 策略是否命中——「有人回了你」不能被普通策略静默吞掉；
  - 回复存进 `feedback.replies`（SQLite schema 10 → 11，新增 JSON 列）。
  - 理由：反馈通道原来只进不出；不给查询和回复，agent 只能反复重报。跨会话权限沿用
    现有的管理员判定，不新增 ACL 概念。
  - 可否回退：回退到 schema 10 需要删 `replies` 列/迁移并移除三个工具；现有反馈数据
    在旧程序里仍可读（`replies` 列不被旧程序查询）。
- 做了什么：
  - `crates/kernel/src/storage.rs`：`FeedbackReply`、`Feedback.replies`、
    `feedback_by_id` / `reply_feedback`；MemoryStore 实现。
  - `crates/kernel/src/storage/sqlite.rs`：schema 11、`replies` 列与迁移、
    SQLite 实现；`docs/07-storage.md` 同步。
  - `crates/kernel/src/engine.rs`：`feedback_list` / `feedback_get` / `feedback_reply`；
    回复事件 + 原会话强制唤醒；权限与会话隔离。
  - `crates/kernel/src/tools.rs` / `harnesses/shared/tools.mjs`：三个工具与参数。
  - `crates/bridge/src/console.rs` / `console.html`：`POST /api/feedback/:id/reply`、
    反馈页回复按钮与回复展示。
  - `crates/kernel/tests/feedback_flow.rs`（新）：回复入库/事件/唤醒、会话隔离、
    非管理员跨会话拒绝、管理员跨会话回复。
- 证据：
  - `cargo test -p acb-kernel --test feedback_flow` → 3/3；
  - `cargo test --workspace` → 全通过（exit 0）；
  - dsh `npm test` 34/34；opencode `npm test` 42/42；`node --check harnesses/shared/tools.mjs` 通过。
- commit：`59e30e0 feat(feedback): add reply lifecycle and wake-back loop`
- 偏离说明：回复事件额外做了一次「强制唤醒原会话」，严格说绕过了 wake 策略；这是
  反馈闭环的必要语义（否则回复可能永远不被看见），代价是原会话会多开一轮空投递。
- 未决问题：是否需要限制回复频率/回复长度可配置；反馈审计是否要单独持久化。

## 2026-11-04 02:45 CST — 设计文档：native 重载与 supervisor

- 任务编号：T7 之后的设计议题（用户新增，只出文档）
- 做了什么：
  - 新增 `docs/plan/native-reload-and-supervisor.md`：
    - 边界：core 只能排空 + 按退出码退出 + 暴露健康/排空探针；真正拉起必须外部
      supervisor（systemd）；进程内 self-exec 在进程已不可信时不可靠，用具体失败场景
      论证（堆损坏、残留线程/锁、fd/信号污染、fork+exec、PID 不变、启动期坏配置）。
    - supervisor：systemd `Restart` / `RestartForceExitStatus=75` /
      `RestartPreventExitStatus=78` / `StartLimitIntervalSec` / `TimeoutStopSec` /
      `ExecStart` 包装器 / `ExecStartPost` 健康门禁；core 侧信号、探针、退出码、
      drain 配置项命名（`[runtime.drain]`）。
    - 回滚：版本回滚（`current` / `last-good` / `attempt` + 健康门禁切回 store path）；
      插件回滚（`current.json` / `last-good.json` 清单 + 启动失败回退）。
    - 与 ADR-0029/0030：drain 期间拒绝新轮次但不丢队列；任务记录内存态会丢；
      ADR-0030 D3 的 native 回滚仍不成立，supervisor 重启是现实保底。
    - 代价：MVP vs 完整版（独立常驻 supervisor）对比；落地顺序。
- 证据：文档静态审阅；对照 `deploy/systemd/acb-bridge.service`、
  `crates/bridge/src/main.rs`（无信号处理、退出码只有 0/1）、ADR-0029/0030。
- commit：`439f980 docs(plan): design native reload supervisor and record agent context budget rule`
- 下一步入口：等用户拍板文档末尾四个选项（重启作为唯一可信边界、drain 强制退出丢
  内存任务、探针是否免 token、版本回滚用包装器还是独立 supervisor）。
- 新增用户设计偏好（2026-11-04，写入后续所有设计与实现）：
  1. **主要使用者是 agent**：新增工具/输出/事件先问上下文消耗——返回体大小、字段冗余、
     分页/截断、稳定短 id、批量查询、错误信息可行动性。
  2. `extension-point-catalog.md` 必须增加 context budget 一节：每个工具/接口的典型与
     最坏返回大小，标出随数据规模线性增长的输出并给上限/分页。
  3. 功能优先由插件/扩展实现，但不让功能面扩张变成上下文膨胀；新增能力同时给
     「最小调用面」（默认按需查询细节，不平铺全量）。
  4. 本机内存有限：`cargo test --workspace`、`nix build` 等重任务串行执行，不同时起
     多个；若进程被杀，先记录 `journalctl` 的 OOM 条目再继续。

## 2026-11-04 03:05 CST — 设计提案：平台边界

- 任务编号：设计议题（用户新增，只出文档）
- 做了什么：新增 `docs/plan/platform-boundary-proposal.md`：
  - 把「平台实现/策略进 core 禁止」与「平台无关能力抽象必须留在 core」拆成两条；
  - 给出可执行判据：新增平台能否只写适配器 + 注册、不改 core 业务文件；
  - 反例：通用插件 trait 里加 `platform()`；正例：`PlatformAdapter` + 能力 flags；
  - 复核现有代码位置（`AdapterManifest.platform`、context 的 `platform` 数据、
    `validate_wake_events`、`Element::Face{platform}`、`sample_string`、`WireManifest`），
    逐条给出允许/可疑/禁止判断；
  - 给出愿景/ADR-0027 的修订建议文本（不改原文）与能力 flags 建议。
- 证据：对照 `docs/01-vision.md`、`docs/adr/0027-platform-adapter-registration.md`、
  `crates/kernel/src/adapter.rs`、`crates/adapter-api/src/lib.rs`、`crates/kernel/src/outbound.rs`。
- commit：`a13667d docs(plan): add platform boundary and extension catalog with four-axis criteria`
- 未决问题：是否采纳 ADR 修订方向；能力 flags 首批范围；`sample_string` 是否移到
  `MethodSpec.examples`。
- 下一步入口：等用户确认后再改 ADR/代码。

## 2026-11-04 03:05 CST — 设计文档：扩展点图谱与上下文成本

- 任务编号：设计议题（用户新增，只出文档）
- 做了什么：新增 `docs/plan/extension-point-catalog.md`：
  - 现状清单：14 类扩展点/钩子的签名、触发时机、能否拦截、能否改写内容/目标；
  - 今天插件能做的事与边界（含「工具扩展/任务 handler/跨轮状态」缺口）；
  - 缺口 hook 建议（`RecordObserver` / `ToolProvider` / `TaskHandler` / `PluginState` /
    `SendTargetHook` / `ToolOutputHook`）与优先级；
  - 必须留在 core 的不变量（轮次边界、串行/票、ack/水位、引用判定、访问边界、
    存储顺序、生命周期安全）；
  - 判据：「改策略走插件，改不变量进 core；要新副作用/新数据形状，先加 hook」；
  - context budget：逐工具典型/最坏返回体、线性增长项、现有上限、可选优化与代价、
    测量口径。
- 证据：对照 `crates/kernel/src/plugin.rs` 的 trait/上下文、`adapter.rs` 的 manifest、
  `tools.rs` 的参数与上限、`engine.rs` 的 delivery/tool 实现。
- commit：`a13667d docs(plan): add platform boundary and extension catalog with four-axis criteria`
- 未决问题：`RecordObserver` / `ToolProvider` / `TaskHandler` 首批范围；`PluginState`
  是否进 core；context budget 中哪些优化先测量。
- 下一步入口：与「外部项目功能面清单」合并成差距矩阵。

## 2026-11-04 03:05 CST — 评估口径更新（用户裁定）

- 后续所有设计与评审统一只看四项：
  1. 稳态运行成本（每轮固定开销、线性返回体、额外往返）；
  2. 可观测性/可审计性（日志对账、错误定位、细节丢失）；
  3. 接口面增长速度（插件自己长，还是 core 多入口/工具/字段）；
  4. 可变更性（schema/协议是否锁死后续改法）。
- **开发难度不作为否决理由**：多写代码、迁移调用点、一次性重构、学习成本不计入。
- 已写入 `extension-point-catalog.md` 顶部与 §5，`platform-boundary-proposal.md` §5，
  `native-reload-and-supervisor.md` §5。
- 上下文预算定位：属于第 1/2 项的权衡，不是独立最高目标；短 id 用「可读前缀 + 短 id」
  折中，不放弃唯一性/可对账性。

## 2026-11-04 03:40 CST — Supervisor MVP：排空、退出码、健康探针

- 任务编号：supervisor（用户授权代行决策）
- 代行决策：
  - 进程重启是 native 生命周期的唯一可信边界；core 只做排空后退出，拉起交 systemd；
  - drain 默认 30s（常量，配置化留后），超时强制退出并记录剩余内存任务清单；
  - `/healthz` 只允许 loopback，且要求控制台 token；core 另写 0600 运行时 token 文件
    供 systemd curl，单元文件不出现 token 明文；
  - 回滚 MVP 手动：Nix generation 保留上一个 store path + 插件 LKG 清单留后；
  - 退出码：0 = 运维停止，75 = core 请求重启，78 = 配置/启动失败（不自动重启）。
  - 可否回退：删除信号处理与排空分支即可恢复原行为；systemd 单元可退回
    `Restart=always`。不改外部协议/配置格式。
- 做了什么：
  - `crates/kernel/src/engine.rs`：`draining` 状态、`begin_drain` / `is_draining` /
    `drain_snapshot` / `drain_leftovers`；排空时继续入库、跳过 route/wake 解析与开轮。
  - `crates/kernel/src/route_worker.rs`：`busy` 计数，排空等待后台发送/任务收尾。
  - `crates/bridge/src/main.rs`：SIGTERM/SIGHUP handler（只写原子量）、30s drain 循环、
    退出码 0/75/78、写 0600 health token 文件。
  - `crates/bridge/src/console.rs`：`GET /healthz`（都走 token 鉴权 + 强制 loopback）。
  - `deploy/systemd/acb-bridge.service`：`Restart=on-failure`、
    `RestartForceExitStatus=75`、`RestartPreventExitStatus=78`、`StartLimit`、
    `TimeoutStopSec=45`、`ExecReload=kill -HUP`、`ExecStartPost` 本机 curl 探针。
  - `crates/kernel/tests/drain_flow.rs`（新）：排空跳过 route/wake、记录仍落库、
    active turn 计入快照、leftovers 可报告。
- 证据：
  - `cargo test -p acb-kernel --test drain_flow` → 3/3；
  - `cargo test --workspace` → 全通过（exit 0）；`cargo fmt` 无差异；
  - 实机：restart 后 `curl /healthz` 返回 `state=ready`、`schema_version=11`；
  - 实机：`systemctl --user reload` 触发 SIGHUP → 日志「收到信号 1，开始排空」
    → 进程退出 75 → systemd 自动重启（Invocation 变化，health 再次 ready）。
  - 实机发现：NixOS 无 `/bin/kill`，ExecReload 必须用
    `/run/current-system/sw/bin/kill`；已修正。
- commit：`dee2630 feat(runtime): drain before exit and expose loopback health probe`
  （ExecReload 路径修正另见下一次 docs/unit 提交）。
- 偏离说明：`/healthz` 复用控制台 HTTP 端口（不强开 3101），用 loopback 限制 +
  token 鉴权保证只给本机；代价是控制台若绑 0.0.0.0，探针路径也走同一端口。
- 未决问题：自动回滚/插件 LKG 提升逻辑是否要做；drain 参数是否配置化。
- 下一步入口：平台能力 flags（仅现有适配器真实能力）。

## 环境状态

- `acb-bridge.service` 曾因上轮中断变为 inactive；2026-11-04 01:19 已重新启动，
  仍使用 `~/.config/systemd/user/acb-bridge.service.d/worktree.conf` 指向 worktree debug 构建。
- 未提交文件：`code-review-prompt.md`（按用户要求不提交）。
- `docs/09-cordis-mechanism-evaluation.md` 已补提交，作为 Cordis 评估的可评审产物。
