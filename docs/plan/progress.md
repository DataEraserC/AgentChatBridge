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

## 2026-11-04 04:10 CST — 上下文成本：删冗余 columns + 阈值索引 + 三个输出测量

- 任务编号：context budget（用户代行决策）
- 代行决策：
  - 立即删除每条记录的 `columns` 布尔映射，列定义只留响应顶层；
  - 目录/列表超过 4 KiB 才索引化，不默认拆成两步；阈值以下保持全文；
  - 分层工具包先设计+测量，不默认启用；未测量前不改默认工具集；
  - 不改错误信息可行动性，不引入新的两级查询默认路径。
- 做了什么：
  - `crates/kernel/src/engine.rs`：`record_columns` 去掉每记录 `columns`；新增
    `LIST_INDEX_BYTES = 4096` 与 `maybe_index_list`；
    `platform_methods` / `feedback_list` / `task_list` / `get_platform_events`
    超阈值时返回索引 + `detail_hint`；
  - `crates/kernel/tests/turn_flow.rs`：列定义只顶层出现，记录不再带 `columns`；
  - `crates/kernel/tests/feedback_flow.rs` / `drain_flow.rs`：大列表自动索引、
    详情接口仍返回全文；
  - `crates/kernel/tests/context_budget.rs`（新）：可重复测量插件目录/task_list；
  - `crates/extensions/tests/authoring.rs`：可重复测量扩展目录；
  - `docs/plan/extension-point-catalog.md`：并入外部成本报告基线、pack 拆分、
    已实施/待实验与阈值依据。
- 证据/前后数据（JSON 字节；前两项为本轮 `--nocapture` 实测，columns 来自外部报告）：
  - 每记录 `columns`：外部报告实测 20 条短消息 1,686 B → 796 B（省 890 B，≈890 tokens）；
  - `plugin_catalog`：10 个内置 3,709 B（典型）；60 个插件 26,089 B（最坏，线性增长）；
  - `task_list`：5 条 1,739 B（全文，1 次调用）；20 条触发索引 3,781 B（1 次调用，
    详情需再 1 次 `task_get`）；
  - `extensions_catalog`：1 个模块 550 B；50 个模块 14,742 B（线性增长，1 次调用，无分页）；
  - `cargo test -p acb-kernel --test context_budget` 2/2；
    `cargo test -p acb-extensions --test authoring measure_extension_catalog_sizes` 1/1。
- commit：`aacb555 perf(tools): drop per-record columns map and index large lists`
  与 `b9297e3 test(tools): measure context cost of catalogs and task lists`
- 偏离说明：阈值索引会让超过 4 KiB 的列表少给细节并多一次详情调用；这是用一次
  额外调用换体积，依据是外部报告「一次调用 ≈ 9k 工具定义 + 历史」。未对
  `describe_plugins(global)` 做索引（不在本次四个接口清单里），只记录测量值。
- 未决问题：分层工具包是否做实验开关；描述瘦身/共享 schema 是否值得；阈值 4 KiB
  是否需要按模型窗口调整。
- 下一步入口：RecordObserver 与 ToolProvider。

## 2026-11-04 04:35 CST — 扩展点：RecordObserver 与 ToolProvider

- 任务编号：扩展点首批（用户代行决策）
- 代行决策：
  - 只做 `RecordObserver` 与 `ToolProvider`；`TaskHandler` 随 T8，`PluginState` 不进 core；
  - 观察者 append 后同步只读调用，不能改记录/开轮/水位；
  - 插件工具用 `plugin_tools` 索引 + `plugin_tool_call` 通用入口，schema 按 name 按需查，
    不给每个插件工具单独注册成 core 工具；
  - 工具名不能和内置工具重名；工具上下文只给会话/房间/来源/是否在轮内。
  - 可否回退：删除两个扩展点与两个工具即可；不影响现有配置/协议。
- 做了什么：
  - `crates/kernel/src/plugin.rs`：`RecordObserver`、`ToolSpec`、`ToolCallContext`、
    `ToolProvider`；`ExtensionPoint::Observer/Tool`、`Capability::ObservedRecord/ToolCall`；
    `register_observer` / `observe`、`register_tools` / `plugin_tool_index` /
    `tool_provider_for`；manager instantiate 支持 Observer/Tool；卸载清理。
  - `crates/kernel/src/engine.rs`：append 后调用观察者；`plugin_tools` /
    `plugin_tool_call` 工具分发。
  - `crates/kernel/src/tools.rs` / `harnesses/shared/tools.mjs`：两个新工具。
  - `crates/kernel/tests/observer_flow.rs`（新）：每条入库记录观察一次、不改变调度；
  - `crates/kernel/tests/plugin_tools_flow.rs`（新）：发现/schema/调用/上下文/未知工具/
    内置重名拒绝。
- 证据：
  - `cargo test -p acb-kernel --test observer_flow` → 2/2；
  - `cargo test -p acb-kernel --test plugin_tools_flow` → 2/2；
  - `cargo check --workspace` 通过。
- commit：`adb89dc feat(plugin): add record observers and provider tools`
- 偏离说明：插件工具采用「索引 + 通用调用」两步发现，新增 2 个常驻工具定义；
  这比给每个插件工具单独注册省接口面，但 agent 需要先查索引。下一步可用测量数据
  评估是否值得把频繁使用的插件工具提升为常驻（暂不做）。
- 未决问题：ToolProvider 的权限是否只给 high-trust 载体；插件工具是否需要单独的
  调用审计/限流；`plugin_tools` 索引是否需要阈值索引（当前 limit 50）。
- 下一步入口：TaskHandler（T8 一起）。

## 2026-11-04 04:50 CST — 能力差距矩阵（外部功能清单合并）

- 任务编号：capability gap matrix（用户新增，只出文档）
- 做了什么：新增 `docs/plan/capability-gap-matrix.md`：
  - 覆盖外部 151 条功能（A1–N14），按 14 类归组，每条给 ACB 现状/依据/优先级；
  - 三个重点结论：平台适配器外置可行、能力必须是可查表数据、扩展点数量不是目的；
  - 明确不做清单（独立账号、模型侧、RAG 引擎、插件分发/市场、生产 HMR 等）；
  - 能力表字段草案、astrbot 封闭 pipeline 对照、四项口径复核与 10 项待拍板。
- 证据：对照 `feature-inventory.md`、`extension-point-catalog.md`、platform/supervisor
  文档，并核实 `plugin.rs` / `adapter.rs` / `tools.rs` / `outbound.rs`；文档 462 行。
- commit：`28cc75e docs(plan): merge external feature inventory into capability gap matrix`
- 文档状态修正：`sample_string` 已迁走（`2a722af`）；`RecordObserver`/`ToolProvider`
  已落地（`adb89dc`）；`TaskHandler` 留到 T8，`PluginState` 不进 core。
- 待用户拍板高优先级：`TaskHandler` 签名与持久化、能力表首批字段、上下文注入形态、
  tool-access 是否进 core、核心清账清单、插件分发边界、回滚边界。
- 下一步入口：等用户拍板后，按矩阵优先级逐项落地；不为了齐平而加 core 入口。

## 2026-11-04 03:09 CST — T8：TaskHandler（插件后台任务）

- 任务编号：T8 / TaskHandler（用户代行决策实施）
- 代行决策：
  - 签名先按最小面：`handle(task_id, payload) -> Result<TaskOutcome>`，不暴露
    Store、票、水位或 Bridge；任务记录仍是 core 的内存态，插件要持久化就自己存。
  - 任务记录继续内存 200 条上限（`TASK_KEEP`），**不落 SQLite**。理由：现在没有
    跨重启恢复需求；一旦落库就要引入 schema 迁移、清理策略与旧数据兼容。
    **何时该做**：出现「重启后任务要续跑/查询」或控制台要跨实例看任务状态时，
    单独设计任务表与恢复语义，不在当前扩展点里顺手加。
  - `TaskHandler` 只给插件一个执行动作，不改 core 调度：仍由 route worker 串行执行，
    handler 在锁外跑，返回 `TaskOutcome::Send` 后走既有发送管线。
  - 可否回退：删除 `task.rs` 的 trait、`plugin.rs` 的 Task 扩展点与
    `route_worker.rs` 的分支即可；`text_limit` 是独立提交，互不牵连。
- 做了什么：
  - `crates/kernel/src/task.rs`：公开 `TaskOutcome`，新增 `TaskHandler` trait；
  - `crates/kernel/src/plugin.rs`：`ExtensionPoint::Task`、`Capability::TaskHandle`、
    `ProviderOptions::Task`、`ProviderInstance::Task`；`register_task_handler` /
    `task_handler`；manager 实例化与卸载清理；
  - `crates/kernel/src/engine.rs`：`Bridge::task_handler` 按名字取执行器；
  - `crates/kernel/src/route_worker.rs`：插件 handler 优先，取不到回退内置 `exec`；
  - `crates/kernel/tests/task_flow.rs`：`EchoTask` + `/task plugin` 路由测试；
  - `crates/kernel/src/plugin.rs` 单元测试：卸载 namespace 后 handler 不残留。
- 证据：
  - `cargo test -p acb-kernel --test task_flow` → 4/4；
  - `cargo test -p acb-kernel --lib` → 162/162；
  - `cargo test --workspace` 全绿。
- commit：`f8c11c4 feat(plugin): add task handler extension point`
- 实施中发现并修正的隐患：`task_handlers` 原先按 handler 名字前缀判归属，名字可以
  比 provider id 短（如 `echo`），卸载后会残留。已改成存 `(owner ProviderId, handler)`，
  按注册者 namespace 清理，并加单元测试。**不是默认决策冲突，是接口实现修正。**
- 未决问题：插件 handler 是否需要调用审计/超时（当前只有内置 `exec` 有超时）；
  manager 实例化的 task provider 每次调用都重新 instantiate，是否需要缓存。
- 下一步入口：T8 收尾后，按代行决策做上下文注入的声明式形态。

## 2026-11-04 03:09 CST — 能力表：text_limit 数据化

- 任务编号：能力表补充（用户代行决策实施）
- 代行决策：
  - 补 `AdapterManifest.text_limit` / `WireManifest.text_limit` /
    `WireAdapterInfo.text_limit`，由适配器声明单条文本建议上限；
    `None` = 未声明，不等于无限。
  - `edit` 保持不声明，直到真有适配器实现；`reply` / `forward` 这类先不做。
    理由：能力表只写「真实存在、可验证」的能力，避免 astrbot 那种默认 True 的谎报。
  - 可否回退：删除字段与三处映射/展示即可；没有消费方依赖它做硬性拒绝。
- 做了什么：
  - `crates/kernel/src/adapter.rs`、`crates/adapter-api/src/lib.rs`、`wire.rs`：
    字段 + wire 转换 + 构建器/测试字面量；
  - OneBot11、process 适配器声明 `text_limit: None`（不谎报一个数字）；
  - process 子进程 hello 可声明 `text_limit`，经 `runtime_manifest` 进 core；
  - `describe_plugins` / 连接摘要输出新增 `text_limit`（`null` = 未声明）；
  - 测试：wire 转换 + process conformance 断言上限进 manifest。
- 证据：`cargo test --workspace` 全绿；`cargo fmt --all -- --check` 通过。
- commit：`b3c5c86 feat(adapter): declare text limit as adapter capability`
- 事实说明：core 目前**没有**硬编码的平台文本长度上限可替换；本项是把未来切分/
  提示所需的数据前移，不新增按平台名的分支，也没有让 core 读它做截断。
- 未决问题：谁消费 `text_limit`（适配器切分、`SendHook`、还是 core 发送计划）；
  一旦引入消费方，需要补「超限如何报错/分段」的审计。

## 2026-11-04 03:09 CST — 上下文注入 / tool-access / 自动回滚（决策登记，待实施）

- 任务编号：代行决策登记（用户不在）
- 决策 3a：上下文注入只做「插件贡献 prompt 片段」的声明式形态。
  - 约束：片段有长度上限、有优先级、可开关、可审计（来源可追到 provider id）；
    片段与事实记录分离，不写进 `elements` 真值；**不允许**插件任意改写整段对话历史。
  - 理由：注入会进入模型上下文，必须可解释、可回退、可关；整段改写等于把桥的
    上下文账本交给插件，破坏可观测性与轮次语义。
  - 可否回退：片段是附加数据，关掉开关即恢复到现有投递内容。
  - 状态：未实施。实施前先定协议字段（投递里的片段列表）与审计落点，再动 core。
- 决策 3b：tool-access 权限判定仍留 core；扩展点只做「允许/拒绝 + 原因」，
  不允许插件自行放行。
  - 理由：tool-access 是访问边界，不是策略偏好；放行权一旦外包，审计和默认拒绝
    都无法保证。
  - 可否回退：只读判定扩展点，删除后回到 core 默认判定。
  - 状态：未实施；先在 extension-point-catalog 里补形状，再决定是否值得做。
- 决策 4：supervisor 自动回滚维持手动。
  - 前置条件：自动回滚需要**可信健康判据**（能区分「启动完成」与「启动成功但功能
    坏了」），当前只有 `/healthz` + 进程存活，不足以自动判坏。
  - ADR-0030 D3 措辞等用户确认，不改原文。
  - 可否回退：手动回滚路径（Nix generation + 插件 LKG 列表）已存在，自动回滚可后加。

## 2026-11-04 05:10 CST — 上下文注入片段协议设计（只出文档，待确认）

- 任务编号：代行决策 3a 补协议设计（用户要求：文档出来后等确认再实施）
- 做了什么：新增 `docs/plan/context-injection-protocol.md`，覆盖：
  - `Delivery.prompts: Vec<PromptFragment{source,text,priority,truncated}>`，可选、默认空；
  - `PromptProvider` 只给 session/room/platform/reason，不交记录列表、Store、票、水位；
  - 会话级 provider 引用作为开关；空列表 = 关闭；
  - 预算：单段 2000 字符、整轮 6000 字符、最多 8 段；优先级降序 + source 字典序；
  - 超限：裁剪标 `truncated`，整段放不下就跳过并留日志；首段超整轮上限才裁剪；
  - 失败：provider `Err` 丢该来源；同步调用、不设 core 超时；provider 契约要求快/只读；
  - 审计：不写 store（避免污染水位/历史），只写 journald 结构化日志，`source` 随投递可见；
  - harness：人格之后、记录之前，独立 `[插件上下文]` 块 + 来源标签，不进入 `records`；
  - 兼容：加法字段，不改 `PROTOCOL_VERSION`；旧 harness 忽略。
- 证据：文档 197 行；未改任何代码/协议/harness（符合「只出文档」）。
- commit：`6d05dc1 docs(plan): design declarative prompt fragment protocol`
- 阻塞原因：**实施等待用户确认**（开关形态、上限数值、审计环、harness 渲染位置）。
- 可回退：纯文档，删除即无影响。

## 2026-11-04 05:10 CST — 下一项：ToolAccess 只读否决点

- 任务编号：tool-access（代行决策 3b 落地；计划中 §7 第 6 项）
- **完成时用户能做什么**：
  - 安装一个 `ToolAccessProvider` 插件后，可以按工具名、会话、房间、平台、来源发送者、
    是否管理员、参数拒绝任意 core 工具调用；
  - 被拒绝时 agent 收到 `[tool_denied] <provider>: <reason>`，能知道原因而不是笼统失败；
  - 插件说 Allow 不能越过 core 的已知工具/轮次/访问边界；core 仍先判，插件只能加否决；
  - 卸载插件后否决点随 namespace 一起消失，不需要重启桥。
- **验收标准**：
  1. 单个工具被拒绝，其它工具照常；拒绝前不产生发送/平台 I/O 等副作用；
  2. provider 返回错误时按失败关闭（拒绝），并留 provider id 与原因；
  3. `Allow` 不能覆盖 core 的越权拒绝（未订阅房间仍被 core 拒绝）；
  4. 卸载 namespace 后 `check_tool_access` 不再命中；
  5. `cargo test --workspace`、dsh/opencode harness 测试全绿。
- 做了什么：
  - `plugin.rs`：`ToolAccessProvider` / `ToolAccessContext` / `ToolAccessVerdict`；
    `ExtensionPoint::ToolAccess`、`Capability::ToolAccessDecision`、
    `ProviderOptions::ToolAccess` / `ProviderInstance::ToolAccess`；
    `register_tool_access` + `check_tool_access`（provider id 排序、首个 Deny 生效、
    Err/实例化失败按失败关闭）；卸载清理。
  - `engine.rs`：`check_tool_access` 组装上下文（platform、is_admin、live）；
    `tool_call` 与 `begin_tool_call` 在匹配分支/副作用之前各调用一次。
  - `lib.rs`：导出三个新类型；i18n 中英六条。
  - `crates/kernel/tests/tool_access_flow.rs`（新）：拒绝单工具 + 上下文断言、
    Allow 不能覆盖 core、provider 出错失败关闭。
  - `plugin.rs` 单元测试：卸载 namespace 后否决点不残留。
- 证据：
  - `cargo test -p acb-kernel --test tool_access_flow` → 3/3；
  - `cargo test -p acb-kernel --lib` → 163/163；
  - `cargo test --workspace` 全绿；
  - `npm test`：dsh 34/34、opencode 42/42。
- commit：`4aac32c feat(plugin): add deny-only tool access provider`
- 边界与未验证：
  - `begin_tool_call` 的守卫与 `tool_call` 共用同一函数，但未新增 TCP 级 endpoint 用例；
    两条入口的调用点已代码审查，provider 契约声明「同一次调用可能被问两次」。
  - `plugin_tool_call` 只按外层工具名判定；插件工具的内层名字暂不能单独拒绝（已记入后续）。
  - provider 是全局安装即生效，没有会话级配置开关；要关就卸载插件或让它返回 Allow。
- 可回退：删除 ToolAccess 扩展点、两个调用点与测试即可；core 判定完全不依赖它。

## 计划剩余项的阻塞原因（按用户要求跳过）

| 项 | 阻塞原因 |
|---|---|
| 上下文注入实施 | 协议设计待用户确认（`6d05dc1`） |
| supervisor 自动回滚 / ADR-0030 D3 措辞 | 待用户拍板；自动回滚还缺可信健康判据 |
| T9 memory 远端/多机拓扑 | 需要先定 ADR（权威源、凭据、冲突策略），且涉及 agenix；用户拍板前不动 |
| T10 扩展工厂 | 依赖插件分发边界（N7/N10/N11/N12）拍板，当前明确不做安装器/市场 |

## 2026-11-04 06:10 CST — 上下文注入实施（已批准）

- 任务编号：代行决策 3a 实施（用户批准默认值）
- 做了什么：
  - `Delivery.prompts: Vec<PromptFragment{source,text,priority,truncated}>`；
    空列表序列化时省略，旧 harness 行为不变；
  - `PromptProvider` / `PromptContext` / `PromptDraft`；上下文只含 session/room/platform/
    reason/wake_reason/now，类型层面拿不到 Store、票、水位；
  - 会话配置 `prompts = ["provider.id"]`；加入 `provider_references`，被引用禁止热卸载；
  - core 在独立线程里调用每个 provider，50ms 超时；超时/panic/Err 只跳过该来源；
  - 预算：单段 2000、整轮 6000、最多 8 段；超限裁剪或整段丢弃，全部写 journald 日志；
  - harness（dsh 复用 opencode 的 `renderTurn`）新增 `renderPrompts`：人格之后、记录之前，
    独立 `[插件上下文]` 块，带来源标签，不进入 `records`。
- 证据：
  - `cargo test -p acb-kernel --test prompt_flow` → 4/4（正常注入、裁剪/总预算、坏来源隔离、
    超时跳过）；
  - `cargo test -p acb-kernel --lib`、`cargo test --workspace` 全绿；
  - proto 单测：空 `prompts` 不出现在帧里，旧形状可解码；
  - opencode 44/44（含旧 harness 无字段输出不变）；dsh 34/34。
- commit：`712bbff feat(plugin): add declarative prompt fragment injection`
- 偏差说明：设计稿原写“MVP 不设 core 超时”，按用户批准改为 50ms 超时；文档已同步。
- 未决：超时线程不 join（可能短暂空转）；审计目前只有 journald，没有控制台审计环。

## 2026-11-04 06:10 CST — 剩余项选择：三个小方向

- 背景：todolist 剩下的都是阻塞项（上下文注入已做完；T9/T10/自动回滚等用户）。
  按用户指示不空转，选以下三个高价值小项；每项独立提交。
- 选定项与理由：
  1. **core 内存与延迟基准**：Observer/PromptProvider 已进热路径，但没有可复跑基线；
     先用数字固定“空插件、100/500 个观察者”的 RSS 与单轮 p50/p95，后续优化才有对照。
  2. **插件作者最小路径验证**：对外目标是插件作者；文档目前只覆盖 dynamic wake 与声明式
     route，ToolProvider 没有最小路径。先走一遍并记录卡点，再改文档，避免凭空设计。
  3. **SQLite 损坏恢复验证**：存储是唯一事实来源；先验证“文件损坏时明确报错、不 panic、
     不静默建新库”，成本小、能防止最坏的静默数据丢失。
- **完成时用户能做什么 / 验收标准**：
  1. 基准：`cargo run --release -p acb-kernel --example core_bench` 输出冷启动 RSS、
     不同观察者数量下的 RSS/注册耗时、单轮 p50/p95/max；数字写进 progress.md。
  2. 作者路径：按文档步骤能在不读源码的情况下写出声明式 route 模块；ToolProvider 的
     最小路径（当前必须 in-process Rust + Manager）与卡点写清楚，文档可直接照抄。
  3. 存储恢复：损坏的 SQLite 文件启动时报明确错误；不会悄悄新建空库；已有好库仍能读。
- 纪律：重任务串行；每个小项一提交一推送。

## 2026-11-04 06:30 CST — core 内存与延迟基准（结果）

- 任务编号：剩余项 1（基准）
- 做了什么：新增 `crates/kernel/examples/core_bench.rs`；父进程为 0/100/500 个空观察者
  各起一个干净子进程，输出 JSON 行；口径为 `/proc/self/status` 的 VmRSS/VmHWM 与
  ingest→poll_turn→end_turn 的墙钟耗时。
- 复跑命令：`cargo run --release -p acb-kernel --example core_bench`
- 基线（本机 release，2026-11-04）：
  - 0 观察者：进程启动 RSS 3,216 KiB；建桥后 4,812 KiB；HWM 5,236 KiB；
    建桥 455 µs；单轮 p50 42 µs / p95 58 µs / max 62 µs。
  - 100 观察者：启动 3,304 KiB；建桥后 4,980 KiB；HWM 5,356 KiB；
    建桥 453 µs；p50 44 µs / p95 62 µs / max 67 µs。
  - 500 观察者：启动 3,184 KiB；建桥后 5,396 KiB；HWM 5,824 KiB；
    建桥 743 µs；p50 77 µs / p95 92 µs / max 95 µs。
- 解读：
  - 空观察者本身几乎不改变单轮延迟；500 个时 p50 从 42 µs 升到 77 µs，主要是每条记录
    append 后把 500 个 provider id 排序一次（`observe` 的既有实现）。
  - 内存随插件数增长约 1.2 KiB/观察者（500 个约 +584 KiB），没有阶跃式泄漏。
  - 这是“插件壳”成本，不含真实插件内部逻辑/I/O；后续若做观察者批量/索引优化，
    用这组数字做对照。
- 证据/产物：`cargo run --release -p acb-kernel --example core_bench` 输出上述 JSON；
  `cargo check --workspace --all-targets` 通过。
- 可回退：纯 example + 文档，不影响生产代码。

## 2026-11-04 06:45 CST — 插件作者最小路径验证（结果）

- 任务编号：剩余项 2（作者路径）
- 做了什么：
  - 新增 `examples/tool-provider/`：可独立构建运行的最小 `ToolProvider`
    （实现 `tools()/call()` → `register_tools` → `Bridge::with_plugins` →
    `plugin_tools` 发现 → `plugin_tool_call` 调用 → 收轮）；
  - `docs/08-plugin-development.md` 新增「工具提供者（in-process）」小节：四步、
    边界（不能和内置工具重名、context 不含 Store/票/水位、core 不设工具超时、
    ToolAccess 只能否决）；
  - `examples/README.md` 登记新示例。
- 实测命令：`cargo run --manifest-path examples/tool-provider/Cargo.toml`
  → 输出 `plugin_tools`（1 个工具）与 `plugin_tool_call`（回显参数），退出码 0。
- 卡点（已据此改文档）：
  1. 文档此前完全没有 ToolProvider 路径；作者只能从测试里找 `Bridge::with_plugins`
     与 `tool_call` 的组合。
  2. **当前 ToolProvider 不能热装**：动态库 ABI / `ACB_DYNAMIC_DIR` 没有 tool 载体，
     必须和宿主一起编译，或由进程内 Manager 提供。这是后续 ABI 缺口，不是文档能解决的。
  3. 工具名重名在注册时拒绝；schema 是 JSON Schema object，agent 需要先 `plugin_tools`
     再 `plugin_tool_call`。
- 证据：示例运行成功；`cargo check --workspace --all-targets` 不受影响（示例独立 workspace）。
- 可回退：删除示例与文档小节即可。

## 2026-11-04 07:00 CST — 失败恢复验证（SQLite 损坏 / provider panic）

- 任务编号：剩余项 3（存储与插件失败恢复）
- 做了什么/验证了什么：
  1. `crates/kernel/tests/sqlite_store.rs` 新增损坏库用例：写入非 SQLite 字节后
     `SqliteStore::open` 返回 `StorageError::Backend`，不 panic；原文件字节保持不变，
     没有被悄悄清空/替换成新库。好库重开可读由既有 `restart_*` 用例覆盖（24/24）。
  2. `crates/kernel/tests/prompt_flow.rs` 新增 provider panic 用例：provider 在投递线程里
     panic，core 通过 channel 断开识别并跳过该来源，整轮投递不受影响（5/5）。
  3. 盘点结论：
     - PromptProvider 的 panic 已被隔离（独立线程 + channel）。
     - RecordObserver / RouteProvider / ToolProvider 的 panic **没有** catch_unwind；
       它们在调用线程里 unwind，最坏会结束进程，由外部 supervisor 拉起。本轮不改，
       因为把 panic 当成正常错误会掩盖插件 bug，且需要单独的安全点/审计设计。
     - kill -9 的恢复语义已由 supervisor MVP（退出码 75/78 + systemd Restart）覆盖；
       本轮没有在临时实例上重复 live kill -9。
- 证据：
  - `cargo test -p acb-kernel --test sqlite_store` → 24/24；
  - `cargo test -p acb-kernel --test prompt_flow` → 5/5；
  - `cargo test --workspace` 全绿。
- 可回退：两个测试用例独立，删除即可；不改生产代码。

## 2026-11-04 08:10 CST — ToolProvider 热装（轮次边界）

- 任务编号：ToolProvider 热装（用户要求：与现有 pending_unload/refs/lastError 一致）
- **完成时用户能做什么**：
  - 用 `ToolLoader` 注册的 ToolProvider 会出现在 `plugin_tools` 索引里并可调用；
  - `queue_unload` / `queue_reload` 只入队，下一个轮次边界生效：索引里的工具随之
    消失/出现，不需要重启桥；
  - 在途轮次继续用旧快照；卸载期间其它内置工具照常可用；
  - 工具名和内置或其它 provider 冲突时安装立即失败，错误里带工具名并要求换名；
  - 卸载后 `plugin_catalog` 显示 `unloaded`，`lastError` 与现有状态机一致。
- 做了什么：
  - `plugin.rs`：新增 `ToolLoader`（`PluginLoader` 实现：install/unload/can_unload/
    inactive/reload；批量安装失败会回滚已装项，不留半套）；
  - `PluginManager::remove_tool`：只摘直接注册的 ToolProvider；manager-owned 的
    工具必须走 manager 卸载；
  - `PluginError::ToolNameConflict`：内置/跨 provider 重名时返回可行动错误；
  - `lib.rs` 导出 `ToolLoader`；i18n 中英三条；
  - `crates/kernel/tests/tool_hot_install.rs`（新）：热装后立即可列可用、卸载后索引消失、
    在途轮次旧快照、重载恢复、重名拒绝（内置 + 跨 provider）；
  - `examples/tool-provider` 改为 `ToolLoader` + 边界卸载/重载演示；
  - `docs/08-plugin-development.md` / `examples/README.md` 同步（从「需重启」改为
    「边界内生效，但代码仍需和宿主编译」）。
- 证据：
  - `cargo test -p acb-kernel --test tool_hot_install` → 2/2；
  - `cargo test --workspace` 全绿；
  - `cargo run --manifest-path examples/tool-provider/Cargo.toml`：
    initial total=1 → pending_unload → unloaded（total=0、调用报 unknown）→
    pending_reload → total=1 且调用成功。
- commit：`3f138fd feat(plugin): hot unload and reload in-process tool providers`
- 边界：`ToolLoader` 解决的是**注册/卸载的边界语义**，不是从磁盘加载新代码；
  动态库 ABI 仍没有 tool 载体。启动时的首次安装仍是立即的（没有在途轮次），
  之后的卸载/重载走排队安全点。
- 可回退：删除 `ToolLoader` / `remove_tool` / `ToolNameConflict` 与测试即可；
  现有 `register_tools` 直接路径不受影响。

## 2026-11-04 08:30 CST — 回调 panic 策略（只出结论）

- 任务编号：回调 panic 评估（用户要求：只出结论，不改代码）
- 产物：`docs/plan/callback-panic-policy.md`。
- 结论：
  - **必须 catch**：`RecordObserver`（记录已落库，panic 不能挡住唤醒/毒化锁）、
    `RouteProvider`（catch → Pass + 日志，路由不是安全闸门，丢消息比漏命令更糟）、
    `ToolProvider`（catch → `[tool_panic]` 错误，轮次继续）。
  - **已经隔离**：`PromptProvider`（线程 + channel 断开）。
  - **不能 catch，应该退出**：动态库 / FFI 回调；panic 跨 `extern "C"` 是未定义行为，
    core 侧 catch 只是掩盖问题。插件应在自己的 FFI 入口 catch 并返回 wire error。
  - **core 自身 panic**：退出，暴露 bug。
- catch 后必须断言：锁未中毒、无半完成的 core 写入（记录/唤醒/`pending_*`）、
  其它 provider 不受影响、下一次调用可用、不能用“成功”掩盖失败。
- 与既有测试的关系：`prompt_flow` 已固定 Prompt panic 隔离；Observer/Route/Tool 目前
  仍直接 panic，尚未实现 catch（本文件只定策略，改动留待用户确认实施顺序）。
- commit：`a16d80e docs(plan): evaluate callback panic handling policy`
- 可回退：纯文档。

## 2026-11-04 09:20 CST — 回调 panic catch 实施

- 任务编号：按 `callback-panic-policy.md` 实施 catch（用户要求）
- 做了什么：
  - `plugin.rs`：`RecordObserver` 的每次 `observe` 包 `catch_unwind`；panic 只跳过该
    观察者并记 `kernel.plugin.observer_panicked`，记录入库/唤醒/其它观察者继续；
  - `engine.rs`：`RouteProvider::decide` panic 按 `Pass` 处理并记日志；core 在收到完整
    decision 前不写 pending，因此没有半成品要回滚；
  - `engine.rs`：`ToolProvider::call` panic 返回 `[tool_panic] <provider>: <tool>` 的
    `ToolError::Internal`；轮次、票、工具索引继续可用；
  - `panic_text` 统一提取 `&str` / `String` payload，避免只留“某处 panic”。
- 测试：
  - `callback_panic.rs` 3/3：Observer（入库+唤醒+其它观察者+再次调用）、Route
    （Pass 后下一条路由继续+入库+`pending_route_actions=0`）、Tool（`[tool_panic]`、
    同轮其它工具、再次调用、下一轮索引）；
  - `panic_stress.rs` 1/1：连续 200 次 Observer+Route+Tool panic；断言
    `queued_turns=0`、`pending_route_actions=0`、provider `lastError=null`、
    生命周期不变、压力后仍可服务；
  - 实测 RSS：baseline 11,012 KiB → 12,456 KiB，增长 1,444 KiB（含 200 条新入库记录），
    低于测试上限 16 MiB；没有结构性残留。
- 证据：`cargo test --workspace` 全绿。
- commit：`10e897f fix(plugin): catch callback panics without poisoning bridge state`
- 边界/未做：
  - 只 catch **回调本身**；`manager.instantiate` 的 panic 仍会冒泡（它属于插件装载路径，
    需要单独的安全点设计）。
  - 动态库 / FFI 回调仍不能由 core catch；按文档由插件在 FFI 入口自己 catch，
    否则进程退出由 supervisor 拉起。
  - 压力测试的内存断言是宽松上限；增长里包含正常入库数据，不能单独当作“无泄漏”证据，
    结构断言（pending/队列/lastError）才是硬检查。
- 可回退：移除三处 `catch_unwind` 与日志键即可；测试与文档可保留。

## 2026-11-04 10:10 CST — manager.instantiate panic 兜底

- 任务编号：manager.instantiate panic（用户要求优先）
- 做了什么：
  - `PluginManager` 新增 `instantiate_safe`；14 个 manager 实例化入口（wake/access/
    delivery/send/turn_error/clock/receive/route/tool/tool_access/prompt/task/observer/
    platform adapter）全部改走它；
  - panic 或普通 `Err` 只把该 provider 记为 `instantiate_errors`，成功时清除；
  - `catalog` 对该 provider 输出 `Health::Unhealthy(错误)`；`Bridge::plugin_catalog`
    的 `lastError` 合并该错误；日志用 `kernel.plugin.instantiate_panicked`；
  - 实例化失败不会新增注册条目：失败发生在解析阶段，core 不写 provider 表；
  - 被 `config.toml` 引用的坏 provider 由 `validate_plugins` 启动检查返回
    `PluginError::Provider`，错误里带 provider id + panic 原因；bridge 启动路径已调用它。
- 测试：`instantiate_panic.rs` 2/2：
  - 同一 manager 里坏 wake/bad_tool/route 与好 tool 并存：坏的全部 panic 被隔离，
    `plugin_tools` 只出现 `good_tool`，好工具可调用，内置 `list_rooms` 可用，
    重复列索引不卡死；
  - 坏 provider `lastError` 含 panic 原因，好 provider `lastError=null`；
  - descriptor 数量不变；被 config 引用的 panic.wake 在 `validate_config` 返回可行动
    错误；再次解析仍报错；route panic 也只影响自己。
- 证据：`cargo test --workspace` 全绿。
- commit：`4955971 fix(plugin): contain manager instantiate panics per provider`
- 边界：manager 自己的 `descriptors()`/`health()` panic 仍未 catch；本项只覆盖
  `instantiate`。动态库 FFI 仍按前一份策略处理。
- 可回退：改回直接 `manager.instantiate` 调用并删除错误表即可。

## 2026-11-04 10:10 CST — 早间决策速览（只出文档）

- 任务编号：4 项待决汇总（用户要求短文档）
- 产物：`docs/plan/decision-brief.md`（约 1.5 页）。
- 四项与推荐：
  1. memory 远端：推荐裸仓库权威 + 每机 clone + 每会话 worktree；
  2. 扩展工厂/分发：推荐暂不做安装器/市场，保留 `extensions_*` + 控制台；
  3. 自动回滚/ADR-0030 D3：推荐维持手动，D3 原文不动，等可信健康判据；
  4. 动态库 tool ABI：推荐短期 in-process + ToolLoader，中期 B（`.so`）或 C（外置进程）
     选一个，优先 C。
- 每项都写了「问题 / 选项 / 各自代价 / 推荐理由 / 不决策会怎样」，细节只给链接。
- commit：`4d80848 docs(plan): add manager panic status and morning decision brief`
- 可回退：纯文档。

## 2026-11-04 04:31 CST — core 代码质量审计（clippy + 风险清单 + manager 只读接口兜底）

- 任务编号：core 代码质量审计（用户指定三项）
- 做了什么：
  1. `cargo clippy --workspace --all-targets -- -D warnings` 清零：
     - 真实问题：needless borrow/closure、redundant deref、`map().flatten()`、
       `sort_by` → `sort_by_key(Reverse)`、`.err().expect()` → `expect_err`、
       未用 import、doc 续行缩进、`not_unsafe_ptr_arg_deref`（`UnloadSafeLibrary::invoke/
       destroy` 改为 `unsafe fn` + `# Safety`）；
     - 刻意保留的 lint 用局部 `#[allow]` + 一行理由：`large_enum_variant`（`Frame`、
       `ToolCallStart`、onebot11 `Parsed`）、`too_many_arguments`（`enqueue`、测试夹具）、
       中文测试名含 URL（`relay.rs` / `platform_flow.rs` / `turn_flow.rs`）。
  2. 新增 `docs/plan/core-risk-inventory.md`：unsafe 清单、生产/测试 unwrap/expect 分布、
     锁中毒与持锁 I/O、无界集合、已知未做项；每条给行号、影响、动作、P0/P1/P2。
     结论：无 P0，P1 共 6 项（入站队列、路由队列、动态库调用线程、relay 持锁复制、
     process stdin 写入、relay 磁盘清理）。
  3. `descriptors()` / `health()` panic 兜底：
     - `plugin.rs` 新增 `call_manager` / `manager_health`（`catch_unwind`）；
     - `collect_manager` 的 descriptors panic → `PluginError::Manager`，不留半棵子树；
     - `catalog` / `manager_health()` 的 health panic → `Health::Unhealthy`；
     - 新增测试 `panicking_descriptors_becomes_install_error`、
       `panicking_health_is_reported_not_propagated`。
- 证据：
  - `cargo clippy --workspace --all-targets -- -D warnings` → exit 0；
  - `cargo test --workspace` → 54 个 suite 全部 `ok`（0 failed）；
  - 新增两个 panic 测试单独跑 2/2。
- commit：`f0e5aee chore(clippy): clear workspace lints under -D warnings`、
  `e1850d4 fix(plugin): contain manager descriptors and health panics`
- 偏离/未做：
  - P1 只评估不批量修：队列上限、锁外 relay、process 写入超时都涉及行为/配置决策，
    按「只修能明确证明的问题」留给后续逐项提交。
  - `manifest()` / `children()` / `details()` / `root()` 仍未 catch，列入清单 P2。
  - `unsafe impl Send for AdapterHandle` 与动态库 ABI 逐条契约未补注释，列入清单 P2。
- 可回退：clippy 提交可逐文件回退；panic 兜底移除 helper、i18n 两条键与测试即可；
  `core-risk-inventory.md` 为纯新增文档。

## 2026-11-04 04:58 CST — 风险清单 A 类修复（relay 锁外 I/O、定期清理、process 写超时）

- 任务编号：core-risk-inventory 跟进 A1–A3
- 做了什么：
  1. **A1 relay 锁外 I/O**（`c912a63`）
     - `PreparedSend` / `PreparedPlatformCall` 新增 `relay()`；锁内只登记 relay 与原始
       params/elements，endpoint 与 route worker 放下全局锁后先 `relay()` 再 `execute()`。
     - `FileRelay::publish` 改为「`create_dir` 抢占唯一 token 目录 + 同目录临时文件 +
       rename」，并发发布不会共享目录或暴露半写文件。
     - 新增并发测试：8 线程同时发布 256 KiB 文件，校验每个 URL 可见时内容完整、token
       目录唯一、无 `.part` 残留。
  2. **A2 relay 定期清理**（`ba39c88`）
     - 启动扫一次 + 后台线程每小时扫一次（`RELAY_SWEEP_EVERY = 3600s`）。保留期 24h，
       因此过期目录最多多留 1h；单次只 read_dir 一层。
     - 单个目录删除失败只记 `kernel.relay.sweep_failed` 日志，不中断循环。
     - 测试用 `sweep_expired_older_than(Duration::ZERO)` 避免真实等待 24h。
  3. **A3 process 写队列**（`f0ea047`）
     - `ChildStdin` 交给独立 writer 线程；调用方只把编码好的行放进有界队列
       （64 行），不持锁等 I/O。
     - 队列满超过 `WRITE_TIMEOUT = 5s`：记 `adapter.process.write_timeout`，置
       `write_failed`/`alive=false`，supervise 杀子进程并按既有退避重启。
     - 测试：`sh -c 'sleep 30'` 不读 stdin，容量 0 队列使第二次写入必然超时；断言
       `write_failed`、`alive=false`、`wait_for_exit` 杀掉子进程。
  4. **B 类设计文档（只出设计，未改代码）**
     - `docs/plan/queue-backpressure.md`：入站 4096 / 路由 1024 候选，reject vs block
       vs drop_oldest 的语义与内存上界、观测字段、`[queues]` 配置草案、对「不丢消息」
       不变量的改写建议。
     - `docs/plan/dynamic-call-timeout.md`：线程泄漏成因、D（超时标死句柄）/ A（单
       worker）/ B（进程隔离）/ C（线程池）对比，推荐先 D 再评估 A，B 作为不可信插件
       配套。
     - `docs/plan/core-risk-inventory.md` 已更新三项「已修」与两项「待拍板」状态。
- 证据：
  - 每次提交前 `cargo clippy --workspace --all-targets -- -D warnings` → exit 0；
  - 每次提交前 `cargo test --workspace` → 54 个 suite 全 `ok`；
  - A1 并发测试、A2 sweep 测试、A3 写超时测试均单独通过。
- 偏离/未做：
  - A 类没有引入新的队列抽象；relay 只做「prepare 登记 + execute 前 relay」的最小拆分。
  - B 类按用户要求只出文档，队列上限与动态库 `failed` 状态未改代码，等拍板。
- 可回退：A1 回退 `PreparedSend/PreparedPlatformCall` 的 relay 字段与调用点；A2 回退
  后台线程与 sweep 返回值；A3 回退 writer 线程改回直接写 stdin；文档纯新增。

## 2026-11-04 08:55 CST — 动态库调用超时方案 D（句柄标死）

- 任务编号：`dynamic-call-timeout.md` 方案 D（用户代行决策：队列上限暂不动）
- 做了什么：
  - `[[plugins]].timeout_ms` 可配置：缺省/0 → 5 秒（保守默认），钳到 100ms–60s；
    只对动态库平台适配器生效。
  - `DylibOutbound` 超时后把该连接句柄写入锁外 `OnceLock` 失效标记；后续 `send/call`
    在拿锁前直接返回明确错误，不再 spawn 线程排队。
  - `DylibAdapterFactory` 按连接记录失效原因；`AdapterFactory::health()` 默认健康，
    动态工厂返回 `Unhealthy`，`PluginManager::catalog` 对平台适配器读取该状态。
  - 重新 `create` 同连接适配器实例（新 handle）时清掉失效记录，catalog 恢复
    `Healthy`，无需重启进程。
  - 日志：`adapter.dynamic.handle_dead`；错误文案改为「句柄已标死」。
  - 测试（`dynamic-fixture/tests/loader.rs`）：`demo.slow` 阻塞 1200ms、超时 300ms；
    断言超时触发、后续调用 <50ms 失败、catalog `Unhealthy`、内置 provider 仍健康、
    新实例恢复且调用成功。
- 证据：
  - `cargo clippy --workspace --all-targets -- -D warnings` → exit 0；
  - `cargo test --workspace` → 54 个 suite 全 `ok`；
  - 动态适配器测试 1/1、配置解析测试 1/1。
- commit：`10791f8 fix(dynamic): mark adapter handles dead after call timeout`
- 残留（未解决，已写入 core-risk-inventory）：
  - 超时那次调用线程仍卡在插件 FFI，无法强制中断；线程与库引用泄漏到进程退出。
  - 适配器载体仍不支持热 reload；“恢复”指重新创建连接实例/新 handle，不是同一个
    dead handle 复活。
  - 动态策略 provider（wake/access/...）仍是同步调用、无超时；本次只覆盖适配器
    发送/调用路径。
- 可回退：移除 `timeout_ms` 字段、`DeadCell/DeadRegistry`、`AdapterFactory::health`
  与 catalog 分支即可；测试与文档可保留。

## 2026-11-04 08:55 CST — decision-brief 用户异议修订（纯文档）

- 任务编号：用户四条异议（memory 远端、扩展工厂、自动回滚、tool/回滚 B+C）
- 修订：
  1. memory 远端：B 改为「裸仓库权威，本地路径/ssh/另一台机器均可」，不依赖
     GitHub/Gitea；代价改为 ssh/文件系统权限与跨机可达性。
  2. 扩展工厂：明确 A → B → C 递进；B 验收线只有「能装、能验（摘要固定）、能卸」，
     评分/搜索/自动更新划到 C；并标明旧 B 草案缺少「验/卸」。
  3. 自动回滚：LKG 定义为 bridge 运行时状态 `plugins-lkg.toml`（版本/摘要/上次健康），
     回滚 = 改配置引用 + reload，不依赖 Nix、不需要 root、不 rebuild；Nix 只钉二进制
     与基础配置，不拥有该文件。写明 Nix / 非 Nix 各自的干净维护方式。
  4. 回滚 B/C 改为层次：C（插件 LKG，core，跨平台无 root）先做，B 的 Nix generation
     作为 Nix 侧适配器后加；真正阻塞是可信健康判据，不是顺序。tool 载体 B/C 另注
     与回滚 B/C 不是同一组选项。
- 结论变化：扩展工厂从「A 现在、B 以后再看」变为明确三步验收；自动回滚从「A 手动、
  B 需要健康判据」变为「C 先做 core 能力、B 作为 Nix 适配器后加」；阻塞点统一为
  **可信健康判据**。
- 仍待用户拍板：LKG 的默认健康判据与写入时机；B（本地扩展工厂）启动时间；
  tool 首期载体是否确认 C（外置进程）。
- 证据：纯文档；`cargo test --workspace` 与 clippy 不受影响（D 提交前已全绿）。
- 可回退：纯文档，按条目回退即可。

## 2026-11-04 — A：插件 LKG 与自动回滚（已实现）

- 做了什么：
  - 新增 `crates/kernel/src/lkg.rs`：`plugins-lkg.toml`（id / version / digest / path /
    promoted_at），每个插件保留最近 3 条，临时文件 + rename 原子写，坏文件降级为空历史。
  - `PluginConfig` 增加可选 `version`；摘要用 64 位 FNV-1a 内容指纹（非签名）。
  - `PluginLoader` 增加 `artifact` / `set_artifact_path`；`DynamicLoader` 登记所有已识别
    provider 的产物引用，回滚时改路径后 reload。
  - `Bridge` 增加 LKG 状态机：启动/reload 成功只进 `candidate`；只有无在途轮次且
    自加载以来无失败、无边界错误、provider 仍可解析时才提升为 `lkg`；
    10 分钟内 3 次失败（解析失败、panic、reload 失败、catalog Unhealthy 转换）
    触发回滚到上一条 LKG，只改运行时路径 + reload，不 rebuild、不重启、不需要 root。
  - `plugin_catalog()` 暴露 `lkgState` / `lkgReason` / `lkgVersion` / `lkgDigest` /
    `lkgPromotedAt`；技术日志统一英文 `plugin-lkg: ...`。
  - 生产 `main` 使用 `Bridge::with_plugins_lkg`，状态文件与 `storage.path` 同目录。
- 偏离：Spec 中的「有在途轮次不提升」由代码结构保证（`maybe_promote_lkg` 首行检查
  `active.is_empty()`）；集成测试改为覆盖安全点顺序，未能从外部构造「轮次中途注册候选」。
  LKG 仅登记 Dynamic + UNLOAD_SAFE v2 可回滚插件；适配器不写文件。
- 残留：策略 provider 的同步调用仍没有通用超时；catalog Unhealthy 是动态适配器超时的
  观测入口。重启后按用户配置重新加载，LKG 只恢复历史，不自动改磁盘配置。
- 证据：`cargo clippy -p acb-kernel -p acb-adapter-dynamic -p acb-bridge --all-targets -D warnings`
  通过；`cargo test --workspace` 全绿。
- 可回退：移除 LKG 模块与 Bridge 接线即可回到提交前行为；状态文件可单独删除。

## 2026-11-04 — B：外置进程工具载体 MVP（已实现）

- 做了什么：
  - 新增 `crates/plugins/process-tools`：stdio 行 JSON-RPC（initialize / list_tools /
    call_tool / shutdown）、每调用超时、调用队列上限 64、崩溃后退避重启并标 Unhealthy。
  - `ToolProvider` 增加默认 `health()`；`PluginManager::catalog` 对直连工具 provider
    使用该健康状态。
  - `Config` 新增 `[[tool_plugins]]`（id / command / args / timeout_ms），解析期校验
    id、保留 namespace、重复 id、空 command、超时范围 100..60000。
  - `crates/bridge/src/main.rs` 在配置非空时安装 `ProcessToolLoader`；第一方工具不变。
  - 新增 `examples/process-tool-echo/`（Node echo 工具 + 四步 README）与
    `docs/plan/process-tool-carrier.md`。
- 测试：协议编解码、工具列表解析、超时标 Unhealthy + 快速失败、crash-once 退避恢复、
  同名工具拒绝、shutdown 终止子进程。
- 偏离：`ErrorCode` 没有 `Unavailable`，超时用 `Timeout`、进程不可用/崩溃用 `Internal`。
  进程载体 provider 暂不支持热卸载/重载。
- 残留：被杀子进程的外部副作用不由 core 清理；权限模型/签名/市场明确不做。
- 证据：`cargo clippy -p acb-plugin-process-tools -p acb-kernel -p acb-bridge --all-targets -D warnings`
  通过；`cargo test --workspace` 全绿。
- 可回退：移除 `[[tool_plugins]]` 接线与 crate 即可；示例与文档可单独删。

## 2026-11-04 — C：入站/路由队列上限（已实现）

- 做了什么：
  - `Config` 新增 `[queues]`：`inbound` 默认 4096、`route` 默认 1024；0 在解析期拒绝。
  - `EventQueue` 从无界 `channel` 改为有界 `sync_channel` + `try_send`；`submit` 返回
    `Full` / `Closed` 明确错误，拒绝计数并打英文日志；新增 depth / highWatermark /
    rejected / 80% 单次 warning 的 `QueueStats`。
  - `RouteWorker` 同样有界；满时拒绝该动作并计数，不阻塞 drain 线程、不 drop 已排队项；
    `busy` 只统计成功入队+执行中的动作。
  - `/api/status` 增加 `queues.inbound` / `queues.route`（深度、容量、水位、拒绝数、
    warning 水位、route busy）。
  - core 不因拒绝自动向房间发提示；只写日志与计数。
- 测试：配置默认/显式/0 拒绝；入站满拒绝与计数、80% warning 回落、顺序保持；
  路由满拒绝且 busy 不膨胀；status 队列字段。
- 偏离：路由队列的 warning 水位按「已排队深度」计算，`busy` 单独展示；不做
  drop_oldest、不做阻塞写、不做跨进程/持久化队列。
- 证据：`cargo clippy --workspace --all-targets -D warnings` 通过；
  `cargo test --workspace` 全绿。
- 可回退：把 `[queues]` 配置与 `channel_with` / `spawn_with_limit` 换回默认构造即可；
  统计字段可单独移除。

## 2026-11-04 — D：dsh 连接失败放大修复（已实现）

- 做了什么：
  - `dsh-client.mjs`：`httpRequest` 对 transport 错误（ECONNREFUSED / ECONNRESET /
    ETIMEDOUT / EPIPE）做有界重试，最多 3 次尝试（首次 + 1s/2s 两次退避）；
    重试耗尽后统一抛 `DshError("unreachable")`，文案带目标 url 与 errno。
  - 只对可安全重放的方法开启重试：`session/list`、`session/cancel`、
    `session/selectModel`、`workspace/rename`；`session/prompt`、`session/create`、
    `workspace/create` 与未知方法保持单次。
  - `driver.mjs#pinModel`：只有 provider 明确返回 `session/model-unavailable` 才映射成
    `dsh.model_unavailable`；transport/auth/protocol/rpc 原样上抛。
  - 新增 i18n `dsh.unreachable`（en / zh-CN）。
  - 文档：`06-harness-adapters.md` 增加重试与错误分类；`troubleshooting.md` 增加
    `dsh.unreachable` 排查行。
- 测试：ECONNREFUSED 最终错误类型与 url/errno；ECONNRESET 计数 3 次；超时计数 3 次；
  `session/prompt` 只 1 次；`session/selectModel` 3 次；driver 侧 transport 失败不被
  报成模型不存在。
- 偏离：重试只覆盖 transport 层，业务 RPC 错误与 4xx/5xx 不重试；WebSocket `follow`
  不在本次重试范围。
- 证据：`harnesses/dsh` 的 `npm test` 40 项全过；`node --check` 两个源文件通过；
  `cargo test --workspace` 全绿。
- 可回退：把 `retryDelaysMs` 设为 `[]` 并恢复 `pinModel` 的统一包装即可。

## 2026-11-04 — LKG 阈值可配置 + 语义书面化

- 做了什么：
  - 新增 `[lkg]` 配置：`max_failures`（默认 3）、`window_secs`（默认 600）；
    0 在解析期拒绝；`Bridge::with_plugins_lkg` 把配置传给 `PluginLkg`。
  - 默认值下行为与之前完全一致；新增可配置阈值/窗口的单元测试。
  - 在 `docs/plan/native-reload-and-supervisor.md` 补「运行时 LKG」一节，写清：
    什么算失败、什么不算、计数何时清零、回滚后候选是否保留、是否有冷却。
- 语义核对结论：实现是「窗口内累计达到 max_failures」，不是严格连续 N 次；
  成功调用不清零，只在候选登记/成功提升/成功回滚时清零。原本文案里的“连续”
  与实现不完全一致；本次只改文档，不改行为。
- 证据：`cargo test -p acb-kernel` 全绿；clippy / fmt 待提交前统一跑。
- 可回退：删掉 `[lkg]` 配置接线即可回到常量默认值。

## 2026-11-04 — connectivity 第①步：core 依赖表 + harness 存在性

- 做了什么：
  - 新增 `crates/kernel/src/deps.rs`：独立锁的 `DependencyRegistry`，三态
    `unknown/up/down`，字段 `since / lastOkAt / lastAttemptAt / neverConnected /
    lastError / lastErrorClass / attempts`；最近 200 条迁移环。
  - endpoint 握手成功后更新 harness `up`，连接结束更新 `down`（class=closed）；
    只在状态变化时写一条 `[deps] transition=...` 日志。
  - `/healthz` 增加 `dependencies` 计数汇总；新增 `/api/dependencies?since=<seq>`
    返回完整依赖表与迁移历史。
  - 主程序启动时按 `[[harnesses]]` 登记初始 `unknown`。
- 偏离/边界：
  - 文档里的 30s 启动宽限期定时器未做；初始就是 `unknown`，等事件驱动。
  - 配置热重载后不裁剪依赖表；第①步只登记启动时配置。
  - “未配置”不是运行时状态：handshake 已拒绝未知 harness，不进入表。
- 证据：`cargo test --workspace`、clippy、fmt 全绿；deps 单元测试覆盖
  unknown→up→down→up 与 since 查询。
- 可回退：删除 deps 模块与两个接口接线即可，不影响 turn 通道。

## 2026-11-04 — connectivity 第②步：适配器上报平台连接状态

- 做了什么：
  - `AdapterHost` 增加 `status: AdapterStatus`；适配器只报
    `connected / disconnected / not_configured`，时间戳、`since`、`lastOkAt`、
    `neverConnected`、`attempts` 由 core 的依赖表统一维护。
  - `DependencyRegistry::from_config` 登记 `[[connections]]`，初始 `unknown` +
    `class=not_configured`，不报 down。
  - OneBot：连接成功报 `connected`，连接失败/断开报 `disconnected`（class
    `unreachable`/`closed`），重连成功再报 `connected`。
  - process：启动成功、重启成功报 `connected`；子进程退出报 `disconnected`；
    重启失败报 `disconnected(unreachable)`。
  - `AdapterFactory::health` 默认从 `Healthy` 改为 `Health::Unknown("connection state
    is reported in /api/dependencies")`；新增 `Health::Unknown` JSON 映射。
  - 不做任何通知，只更新状态表与 `/api/dependencies`。
- 测试：deps 单元测试覆盖平台 `not_configured → up → down → up`；workspace 全绿。
- 偏离：`not_configured` 以初始 `unknown` 状态体现，而不是单独的第四种 state；
  文档里 30s 宽限期尚未实现。
- 可回退：去掉 AdapterStatus 上报调用即可，依赖表不影响 turn 通道。

## 2026-11-04 — fix(deps): count attempts on every connection attempt

- 问题：`attempts` 只在连接成功时累加，导致「从未连上但 attempts>0」永远不会出现，
  而这正是排障时最需要的一条。
- 改动：新增 `harness_attempt` / `platform_attempt` / `AdapterStatus::attempt()`；
  适配器在每次 `connect` 前、接入点在每个 `Hello` 握手前记一次尝试。
  `connected` 不再顺带累加 attempts，也不再写 `lastAttemptAt`。
  `neverConnected` 独立保留，只看 `lastOkAt` 是否为空。
- 测试：新增「只尝试三次、从未连上」用例：`state=unknown`、`neverConnected=true`、
  `attempts=3`、无迁移记录；既有连/断/重连用例改为显式尝试再连。
- 证据：`cargo test --workspace`、clippy、fmt 全绿。
- 可回退：去掉 attempt 调用点即可恢复旧的只在成功时计数。

## 2026-11-04 — fix(queue): count depth before sending

- 问题：深度计数在 `try_send` 成功之后才 +1，接收端可能已经先取走并 -1；
  计数器被减到 0 以下后下一次入队会溢出（debug 下直接 panic）。
  `route_send_goes_through_the_action_worker` 稳定复现。
- 改动：入队前先计数，发送失败/断开时回滚；`dequeued` 用 `fetch_update`
  检查下界，绝不让深度变成 `usize::MAX`。
- 证据：该用例与 event_queue 单测全绿；workspace 全量随后统一跑。
- 可回退：只影响统计口径，不改队列本身。

## 2026-11-04 — fix(deps): prune dependencies on config reload

- 问题：依赖表只在启动时登记，配置热重载后旧条目一直留着，`/api/dependencies`
  会展示已不在配置里的连接，越积越多。
- 改动：新增 `DependencyRegistry::retain_config(&Config)`，启动与轮次边界换配置
  共用：新增条目以 `unknown` 登记（平台带 `not_configured`），仍在配置里的条目
  原样保留，被移除的条目**直接删除**。
- 删除语义：不改成 `down`、不留幽灵条目、不写迁移环；只打一行
  `[deps] removed kind=... id=...`。迁移环只描述运行时状态变化。
- 接线：`Bridge` 持有注入的依赖表；`apply_staged_config` 换配置时调用
  `retain_config`；生产 main 用 `with_dependencies(deps.clone())` 注入同一份表。
- 测试：deps 单测覆盖「保留旧状态 + 新增 unknown + 不追加迁移」与「删除不写迁移」；
  `hot_reload_flow` 增加端到端用例：暂存改名配置后 `poll_turn` 生效，旧 id 消失、
  新 id 出现且为 `not_configured`。
- 证据：`cargo test --workspace`、clippy、fmt 全绿。
- 可回退：去掉 `retain_config` 调用即可恢复「只增不减」；表字段不变。

## 2026-11-04 — connectivity 第③步：turn 失败去重/抑制

- 做了什么：
  - `Bridge::end_turn` 在非 stop 的失败上用原始 `error` 文本算 64 位 FNV-1a 指纹，
    按 `(session, room, fingerprint)` 去重；10 分钟窗口内同一故障只发首条 in-band。
  - 同一房间 10 分钟内最多 3 条 in-band 失败通知；超限只打一行抑制日志。
  - 一轮成功（`error` 为空）清掉该 `(session, room)` 的全部历史失败，下一次故障
    重新通知；`/stop` 仍不参与这个通道。
  - 只有 `notify_turn_error` 真正排进发送队列的通知才计入去重/限流；provider 只
    返回 `log`、或组装失败时不占额度。
  - 去重表有界（256 条），并随 10 分钟窗口清理旧条目。
- 偏离/边界：
  - 不做组件级抑制：这一步拿不到结构化 `component`，不按错误文本猜；等第④步。
  - 指纹是文本哈希，不同措辞的同一根因仍算不同故障；这是第③步的已知边界。
  - 限流值 3 条/10 分钟先写常量，不做配置项。
- 测试：`turn_flow` 新增三条——相同故障三次只发一条；成功一轮后同一故障重新发；
    同一房间不同故障第 4 条被限流。
- 证据：workspace 全量、clippy、fmt 待提交前统一跑。
- 可回退：删掉 `turn_failures` 表与 `end_turn` 检查即可恢复现有行为。

## 2026-11-04 — connectivity 第④步：harness 结构化失败分类

- 协议：`TurnEnd` 增加可选 `failure { component, id?, class }`；`PROTOCOL_VERSION`
  升到 7，`version_compatible` 同时接受 6，旧 harness 不带字段时走原路径。
  `docs/04-protocol.md` 同步补字段形状。
- core：`Bridge::end_turn_with_failure`（`end_turn` 保持原签名）消费分类：
  - `component=model`：写 `DependencyKind::Model`，id 用 `failure.id`，否则
    `session.model`，再否则 `model:<harness>/default`。
  - `component=backend`：写对应 harness 条目，class 用 harness 给的分类。
  - 一轮成功：该会话的模型与 harness 标回 `up`，并对齐 `lastOkAt`。
  - 有结构化分类时 in-band 去重键用 `(session, component, id, class)`；
    没有时仍用原始错误文本指纹。
  - 依赖表按新配置登记/裁剪 model 条目（仍以当前配置为界）。
- harness：`acb-wire.mjs` 协议版本 7，`endTurn` 可带 `failure`。
  dsh 区分 `model_unconfigured`（selectModel 对照 catalog 失败）与
  `model_unavailable`（turn/end 的 model-unavailable code）；dsh 自身不可达归
  `backend/unreachable`。opencode 只在错误码/文本有明确模型、鉴权、限流线索时
  填分类，`outcome=failed` 这类模糊失败不猜。
- 测试：proto 覆盖 v6 无 failure 解码与 v6/v7 兼容；turn_flow 覆盖模型 down→up
  与按结构化分类去重；dsh/opencode JS 测试覆盖 turn_end 带 failure 的形状。
- 证据：workspace 全量、clippy、fmt 全绿；两个 harness 的 `node --test` 全绿。
- 可回退：去掉 `failure` 字段与 `end_turn_with_failure` 接线即可回到第③步路径。

## 2026-11-04 — connectivity 第⑤步：带外通知层

- 落地位置：
  - `crates/plugins/process-tools` 新增 `ProcessNotifier`：kernel `NoticeSink`
    的进程实现，复用本 crate 的 stdio 行 JSON-RPC 载体；有界队列 128，
    满了丢事件并计数，绝不在主流程等网络。
  - `crates/notifier-process` 的 `acb-notifier` 子进程负责 ntfy 兼容 HTTP、
    去重、恢复配对与有界重试；HTTP 客户端（`ureq`）只在这个 crate，core /
    bridge 的依赖图里没有。
  - kernel 新增 `NoticeEvent` / `NoticeSink`：`DependencyRegistry` 在
    `unknown→up`、`→down`、`up→down` 等状态边沿发出事件；engine 在 turn
    失败通知被去重/限流时发 `TurnSuppressed`。不读 harness 内部状态。
- 配置：
  - `[notifications] enabled / url / topic / token / min_interval_secs`；
    `[notifications.in_band] window_secs / room_limit / max_entries` 接管第③步
    三个硬编码常量，省略时默认值与旧行为一致。
  - `url` / `topic` 任一为空或 `enabled = false`：整层不加载——不 spawn、不建
    线程、不打启动日志。`ProcessNotifier::spawn` 也会拒绝不完整配置。
- 推送行为：
  - `POST {url}/{topic}`；`Title` / `Priority` / `Tags` 走 header，body 纯文本
    含 component、transition、since、last_ok、class、lastError、attempts。
  - 同一依赖边沿在 `min_interval_secs` 内只推一次；`up` 只有该组件 `down`
    成功推送过才配对，且带 `down_since` / `duration` / `down_class` /
    `down_lastError`；连续抖动由边沿去重吸收。
  - HTTP 失败首次后最多重试 3 次，退避 250/500/1000ms；仍失败只记日志与计数并丢弃，
    不影响桥和其他事件。
- 测试：`crates/notifier-process/tests/push.rs` 用本地 stub HTTP server 覆盖
  ntfy 请求形状、去重、恢复配对、未配对恢复、3 次重试、turn 抑制去重；
  process-tools 宿主测试用假子进程确认事件按行 JSON 发送；kernel 测试覆盖
  依赖边沿进 sink。
- 部署注意：需要把 `acb-notifier` 与 `acb-bridge` 一起安装（同目录）；
  测试/特殊部署可用 `ACB_NOTIFIER_BIN` 覆盖路径。
- 证据：`cargo test --workspace`、clippy、fmt 待提交前统一跑。
- 可回退：删掉 `[notifications]` 配置即整层不加载；代码可整体删除而不影响
  依赖表与 turn 通道。

## 2026-11-04 — 策略 provider 同步调用通用超时

- 修复前现状（最坏情况都是永久阻塞）：
  - `resolve_*` 本身是 Root 的 map/descriptor 查找；只有委托树外 manager 的
    `instantiate` 会执行插件代码，没有任何超时。
  - `wake.decide_with_reason`、`receive.decide`、`route.decide`、`access.check`、
    `delivery.compose`、`clock.is_due`、`turn_error.decide`、`send.before/after`、
    `observer.observe`、`tool_access.check` 全部在调用线程同步执行；只有 route 与
    observer 有 `catch_unwind`，没有超时。卡住时会分别阻塞 ingest、tool_call、
    open_turn、end_turn、clock tick 或发送，且没有明确错误。
  - 已有例外：prompt provider 是独立线程 + 50ms 超时；process 工具插件是
    100–60000ms 每调用超时 + 500ms 宽限，超时会杀子进程重启。
- 实现：
  - `[provider_calls] timeout_ms`，默认 5000ms；`0` 关闭超时（行为回到旧语义），
    上限 60000ms。
  - `PluginManager::call_timed`：每个外部 provider 一个常驻 worker 线程；
    调用方用 `busy` 拒绝并发。超时返回 `PluginError::ProviderTimeout`，
    panic 返回 `PluginError::Provider`，worker 停掉返回
    `ProviderExecutorStopped`，都带 provider id 与 call 名。
  - 覆盖点：外部 provider 的 wake / receive / route / access / delivery /
    clock / turn_error / send before / send after / observer / tool_access，
    以及树外 manager 的 `instantiate`。
  - 快路径：`acb.*` 内置 provider 直接同步调用，不做输入快照、不跨线程、
    不付超时代价；只有 `namespace != acb` 的外部 provider 走 `call_timed`。
  - 超时后的 fallback：
    - wake → Stay；route → Pass；clock → 不 due；delivery → 默认 composer 计划；
    - receive → 返回错误、不入库；access / send.before → 返回明确 ToolError；
    - turn_error → 默认 provider 的动作；send.after / observer → 跳过并记日志；
    - tool_access → 拒绝（安全闸门 fail closed）。
- 线程语义：超时那次 worker 线程仍可能卡在 provider 里，无法强制中断；但
  每个 provider 最多一个这样的线程，后续调用因 `busy` 立即失败，不会按调用
  次数新增线程；worker 恢复后同一 provider 的调用继续可用。
- LKG 判定：**超时算一次失败**。引擎侧的解析/实例化、策略决策、发送钩子和
  tool_access 超时都会经过 `note_plugin_failure`，与解析失败、panic 共用
  `[lkg]` 阈值；理由是超时是外部 provider 运行期故障，如果忽略，一个永久
  卡死的插件永远不会触发回滚。内置 provider 不走超时，因此不受影响。
  `observer.observe` 是只读旁路：超时只记日志、跳过该观察者，不计数（它不参与
  任何决策，且 PluginManager 不持有 Bridge 的 LKG 状态）。分 class 阈值
  （如 panic 1 次回滚）仍是后续议题，本次未改。
- 测试：
  - `plugin.rs`：永久阻塞调用 → 100ms 超时；随后同一 provider 快速返回
    busy；另一个 provider 正常；释放阻塞后同一 provider 恢复可用。
  - `turn_flow.rs`：外部 blocking wake provider 让 `ingest` 在 100ms 返回，
    消息仍入库、不唤醒；第二次入库因 busy <50ms 返回。
  - 配置测试覆盖默认值、0 关闭、超上限拒绝。
- 证据：`cargo test --workspace`、clippy、fmt 全绿。

## 2026-11-04 — 外部 provider executor 生命周期（stuck / dead / 重置）

- 背景：`call_timed` 的超时只让调用方放弃，worker 仍卡在 provider 里。旧实现里
  `unload_provider` / `reload_provider` 不清 `timed_executors`，LKG 回滚后同一 id
  仍会撞上一个 busy/stuck 的旧 worker；「回滚 → 再卡 → 再回滚」会无限漏线程。
- 生命周期：
  - `ExecutorSlot { current, stuck_generations, dead }`；`MAX_STUCK_GENERATIONS = 2`。
  - 首次超时：把当前 worker 标 `stuck`，后续调用立即返回 `ProviderStuck`，不再
    让调用方白等一个完整超时。worker 若之后返回，会自行清掉 `stuck`，同一代
    可以恢复使用。
  - 卸载 / 重载 / 整 namespace 卸载 / 重新注册同一 id：丢掉 `current`，下一次
    调用起新一代 worker。旧卡死线程无法取消，只能作为一次性泄漏留着；每代
    最多一个。
  - 连续两代卡死（第一次超时后经卸载/重载，下一代再次超时）→ provider 标
    `dead`：`current = None`，后续调用立即返回 `ProviderDead`，日志明确写
    `restart required`。dead 不会因为再次卸载/重载而复活，只有重启进程清空
    内存状态才能恢复。
- 错误区分：`ProviderBusy`（并发/队列满）、`ProviderStuck`（已判卡死）、
  `ProviderDead`（代数超限）、`ProviderExecutorStopped`（worker 线程已退出）
  四个变体各自独立，都有 i18n 文案，不再统一塞进 `ProviderBusy`。
- LKG：`PluginError::counts_toward_lkg()` 只让真实故障计数——
  `ProviderTimeout` / `ProviderExecutorStopped` / `Provider` 算；
  `ProviderBusy` / `ProviderStuck` / `ProviderDead` 不算。引擎侧的
  `note_provider_error` / `record_provider_call_failure` 统一走这个判定：
  第一次超时计一次失败，之后 stuck/dead 的快速失败不会把同一故障重复累加。
- 测试：
  - 卡死 → 超时 → `ProviderStuck` 快速失败 → 重置后新一代 worker 正常工作；
  - 卸载 namespace 后重新注册同一 id → 新一代 worker 正常工作；
  - 连续两代卡死 → `ProviderDead`，重置不复活；
  - `counts_toward_lkg` 区分 timeout/busy/stuck/dead/stopped/provider；
  - 原超时测试仍覆盖：恢复后同一 worker 可继续调用、其他 provider 不受影响。
- 证据：`cargo test -p acb-kernel --lib plugin::tests` 39 项通过；workspace
  clippy / test / fmt 全绿。

## 2026-11-04 — connectivity 第①步收口：harness 宽限期、多连接与未认证握手

- 背景：第①步此前把 harness 记为 `unknown` 后完全事件驱动，设计里的 30s 启动宽限
  未实现；同一 harness 多连接时，任一连接关闭都会误判 `down`。
- 决策：
  - `unknown` 固定为「配置已登记但尚无观测」。harness 在启动宽限期（30s）后仍无连接，
    迁到 `down(class=not_started)`；宽限期只由 core 对 harness 执行。platform 是否启动
    只有适配器知道，core 不凭「没观测到」判 platform down，避免未上报载体被误判。
  - 同一 harness 的连接按 open count 管理：第一条连接把状态置 `up`，最后一条关闭才
    `down`；`attempts` 在每次认证通过的握手上 +1。
  - 未认证 Hello 不再写依赖表；握手失败不能创建或污染条目。
  - model 不在启动时预登记，首次 `model_down` / `model_up` 才 upsert，避免 `/healthz`
    的 unknown 计数被未使用模型撑大；`retain_config` 仍按配置裁剪旧 model 条目。
- 接线：`crates/kernel/src/deps.rs`、`crates/kernel/src/endpoint.rs`、
  `crates/bridge/src/main.rs`（时钟线程每秒 `deps.tick`）。
- 测试：deps 单测新增 unknown 宽限、多连接、未观测 model、model 裁剪；
  `endpoint_flow` 新增未认证握手不建条目、成功握手 `attempts=1`。
- 证据：`cargo test --workspace` 62 个 suite 全 ok；clippy `-D warnings`、fmt 全绿。
- 可回退：从 main 去掉 `deps.tick`；把 open/close 改回单一 connected/disconnected。

## 2026-11-04 — connectivity 第②步收口：平台初始状态与逐连接视图

- 背景：platform 初始 `class=not_configured` 把「配置已存在但尚未连接」误写成
  「未配置」；`/healthz` 只有计数汇总，`/api/plugins` 仍只有工厂级 health。
- 决策：
  - platform 初始保持 `unknown`，`lastError=null`、`attempts=0`；「未启动」由
    `state=unknown + neverConnected=true + attempts=0` 表达，「连不通」由
    `state=down + class=unreachable + attempts>0` 表达，「曾经连上后断开」由
    `lastOkAt` 非空表达。
  - `AdapterStatus` 删除 `not_configured`：不可用语义由适配器上报的 attempt/up/down
    承担，core 不替适配器猜。
  - `/healthz` 的 `dependencies` 改为逐依赖对象
    `{status, up, down, unknown, seq, items[]}`；`/api/dependencies` 保留完整表 +
    迁移环；`/api/plugins` 增加 `connections`（逐平台连接状态）。
- 接线：`crates/kernel/src/deps.rs` 的 `summary` / `platform_items`、
  `crates/kernel/src/adapter.rs`、`crates/bridge/src/console.rs`。
- 测试：deps 单测覆盖 `summary` 的 items/status、平台从未连上到失败、成功/断开/
  重连；`/api/plugins` 的 connections 复用同一份条目。
- 证据：`cargo test --workspace` 62 个 suite 全 ok；clippy `-D warnings`、fmt 全绿。
- 可回退：删除 `summary` 的 `items`/`status`、`platform_items` 与 console 两个字段即可。

## 2026-11-04 — connectivity 第③步收口：组件级抑制与 abandon 不算成功

- 背景：第③步只按文本指纹做 10 分钟窗口去重，窗口过后同一依赖故障仍会再刷；
  连接断开时 `Conn::abandon` 用 `end_turn(None)` 收尾，被当成成功轮次，会把依赖
  刷回 `up` 并清掉失败历史。
- 决策：
  - 结构化 `failure` 在写依赖表之前先查旧状态；依赖已经 `down` 时，后续 turn 失败
    记为 `component_down` 抑制，只发带外事件，不再发 in-band。首失败仍正常通知一次。
  - `Conn::abandon` 改走 `Bridge::abandon_turn`：只作废票、写 `ended` 宽限记录、
    释放会话；不推水位、不发通知、不清失败去重历史、不把依赖置 up。
  - `/stop` 且没有错误的轮次同样不按成功处理。
- 接线：`crates/kernel/src/engine.rs`、`crates/kernel/src/endpoint.rs`、
  `crates/i18n/locales/{zh-CN,en}.json`。
- 测试：`turn_flow` 覆盖结构化失败第二次走 `component_down`、abandon 不产生 up
  边沿；既有文本指纹去重/房间限流/成功重置测试保持通过。
- 证据：`cargo test --workspace` 62 个 suite 全 ok；clippy `-D warnings`、fmt 全绿。
- 可回退：删除 `ComponentDown` 分支与 `abandon_turn`，恢复 `end_turn(None)` 即可；
  这会把「断线不算成功」的行为退回旧语义。
- commit：`026230a feat(deps): expose per-dependency health and tighten failure edges`
  （第①–③步收口合计）。

## 2026-11-04 — 控制台依赖健康面板

- 做了什么：概览页新增「依赖健康」卡片，逐条展示 `id` / `kind` / `state` /
  `since` / `lastOkAt` / `attempts` / `lastError` + `lastErrorClass` /
  `configured` / `expected`；时间用本地时间，空值显示「从未连上」。
- 决策：不改后端 API，直接复用 `/api/dependencies`；`up` / `down` / `unknown`
  使用三套独立 pill 样式（正常绿、不可用红、未观测灰）；刷新复用
  `loadStatus()` 与既有标签切换；接口失败时保留上次成功快照并标注过期时间与错误。
- 测试：`console_html_defines_distinct_dependency_state_rendering` 静态契约测试
  锁定三态分支与三种样式；Node 语法检查 `console.html` 内嵌脚本；
  `cargo test --workspace` 62 suite 全绿；clippy `-D warnings`、fmt 全绿。
- 未做：仓库没有前端测试框架，未加浏览器级渲染测试；未改 supervisor / WASM /
  插件加载区域。

## 2026-11-04 — 动态库平台载体上报 AdapterStatus

- 现状核实：OneBot（内置）与 process 外置适配器已经通过 `AdapterStatus` 上报真实
  连接状态；Lua 只承载策略扩展点，不提供平台适配器，因此没有平台依赖条目；
  真正一直 `unknown` 的是 dynamic `.so` 平台适配器。
- 契约：`AdapterStatus` 增加 `loaded()`（加载/初始化成功 -> `up`，并记
  `lastOkAt`）与 `failed(class, detail)`（明确失败 -> `down`，沿用现有错误分类，
  如 `unavailable`/`closed`/`internal`）。未调用这两个入口的载体保持 `unknown`，
  语义是「未提供状态」，不是「坏了」。
- 接入：dynamic 适配器在 `launch` 时记录 `attempt` 并置 `up`；`DylibOutbound`
  标记句柄失效时立即 `failed("unavailable", reason)`；正常 shutdown 报
  `closed`。不探测远端、不做心跳，不改 process adapter 语义与协议版本。
- 测试：deps 单测覆盖未上报 `unknown`、`loaded()` 后 `up`、`failed()` 后
  `down + lastErrorClass`；dynamic fixture 端到端覆盖加载成功 `up` 与句柄超时
  `down`（`lastErrorClass=unavailable`）。`cargo test --workspace` 63 个 suite
  全 ok；clippy `-D warnings`、fmt 全绿。
- 未做：dynamic 在启动期 dlopen/manifest/实例化失败时桥仍按原语义退出，在线健康
  页不可见；没有网络探测或心跳。

## 2026-11-04 — Supervisor 内置策略引擎（纯计算层）

- 新增独立 crate `crates/supervisor-policy`（`acb-supervisor-policy`），只依赖 serde；
  不接入 supervisor、不做进程监管、不做策略错误计数。
- 公开 API：`decide(&PolicyConfig, &Snapshot) -> Decision`；`DecisionKind`
  = `fail/hold/rollback`，附 `reason_code`、`reason`、`hold` 的 `delay_ms`。
- 输入快照包含 `observed_at_ms`、`startup_at_ms`/`grace_elapsed`、连续失败、
  最近失败记录（时间+类别）、连续不健康探测/起点、LKG、连续 hold 次数；时间全部
  来自快照，模块内无 I/O、无时钟、无随机。
- 规则：panic 类一次即动、启动宽限、健康持续不健康、连续失败、窗口累计、默认
  hold；无 LKG 时 rollback 降级为 fail；hold 退避按
  1s/2s/4s/8s/30s 阶梯取 `consecutive_holds`。
- 配置键：`startup_grace_secs`、`consecutive_failures`、`window_failures`、
  `window_secs`、`panic_classes`、`panic_rollback`、`provider_parse_uses_window`、
  `unhealthy_secs`、`unhealthy_probes`、`restart_backoff_secs`；全部可缺省。
- 测试：`crates/supervisor-policy/tests/decision_table.rs` 5 项，表驱动覆盖 11 个
  规则场景 + 退避阶梯 + 确定性 + 默认值；`cargo test --workspace` 65 个 suite
  全 ok；clippy `-D warnings`、fmt 全绿。
- 明确没做：systemd 单元、supervisor 接入、外部进程/WASM 策略载体、策略错误连续
  3 次升级（调用方职责，代码内有注释）。

- 2026-11-04 — 决策索引：supervisor 平台无关、Nix profile 回滚、测试矩阵、回滚判据、反馈/工单、记忆管家、扩展工厂 → [supervision-packaging-issues.md](supervision-packaging-issues.md)

## 2026-11-05 — platform_call 单入口（层级 op + 封闭 tag）

- 任务编号：adapter-snowluma 第 9 节实装
- 做了什么：
  - `crates/kernel/src/tools.rs`：`TOOL_NAMES` 移除 `platform_methods`；`platform_call`
    成为唯一平台入口。给 `op` 执行；不给 `op` 列目录。
  - `crates/kernel/src/engine.rs`：新增 `find_platform_op` 统一解析；普通调用、票据申请、
    票据执行都走它。目录档支持 `path` 前缀、`tags` AND、`query` 搜索、`all` 连接摘要；
    执行档与目录档字段互斥，混用直接 `BadRequest`。
  - `crates/kernel/src/outbound.rs`：`MethodSpec.tags` 改为封闭枚举 `PlatformTag`
    （30 个固定值）；wire 入口拒绝未知 tag；`MethodSpec::path/in_path` 支持层级前缀。
  - `crates/adapters/onebot-snowluma/src/catalog.rs`：52 个 method 收敛为 50 个层级 op；
    `msg.send` / `msg.history` 合并群/私聊动作，按房间分派；路由、effect、record 同步改名；
    catalog 自动补风险/作用范围/幂等标签。
  - `harnesses/shared/tools.mjs`、dsh/opencode 提示与测试：删除 `platform_methods`，
    `platform_call` schema 改成 `op/path/tags/query/all` + 原有结果查询字段。
- 取舍：
  - 没有按设计原稿拆成 `platform_call` + `platform_call.list` 两个工具。原因：MCP 工具名
    不允许点号；单工具位最省上下文。改成同工具的「省略 op = 列目录」模式。
  - 按用户要求不留旧 method 兼容层、不做短别名；旧名直接删除，目录/路由/测试/harness 一次改齐。
  - 风险策略保持每条 op 的显式 `MethodSpec.risk`（catalog 是唯一 op→risk 表）；不从 tag 推导。
    未知 op 不在 catalog，直接 `unknown_op` 拒绝。没有新增 `admin` risk：core 目前无法验证群
    管理员身份，`requires_admin` 只作为标签，权限不足仍由平台侧拒绝。
  - 目录里的未知 tag 在 core/wire 两层都 fail closed；schema 里给封闭枚举。
- 证据：
  - adapter：`every_op_is_hierarchical_tagged_and_routed`（50 op 全部层级名、有 tag、有路由）、
    `unified_send_and_history_dispatch_by_room`（群/私聊动作分派）。
  - kernel：`unknown_op_suggests_a_direct_neighbour`、`destructive_method_is_refused_by_default`、
    `catalog_unknown_tag_lists_the_known_ones`、`ticket_and_normal_call_share_unknown_op_rejection`
    （票据与普通路径同一段 op 解析）；wire `bad_names_and_enums_are_rejected`。
  - `cargo test --workspace` 全绿；`cargo clippy --workspace --all-targets -- -D warnings`；
    `node --check harnesses/shared/tools.mjs`；dsh 40/40、opencode 45/45。
  - 体积：`tools.mjs` 里平台入口从 6,747 bytes（`platform_call` 4,297 + `platform_methods` 2,450）
    降到 3,922 bytes（单 `platform_call`），约 -42%；工具数 40 → 39；op 52 method → 50 op。
- 下一步入口：若还要压工具描述，按 `extension-point-catalog.md` 的 30 工具上限把
  `memory_git_*` / notes 合并成单入口。

## 2026-11-05 — 批次 1：版本与状态采集

- 任务编号：adapter-snowluma 批次 1
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/link.rs`：连接后的 `probe_identity` 依次调
    `get_login_info` / `get_version_info` / `get_status`；新增纯函数
    `parse_version_info` / `parse_status`，超时、缺字段、`retcode != 0`、非法协议版本
    全部落 `unknown`，不 panic。
  - 协议版本只认 `11` / `v11`，统一成 `v11`；识别不了记 `unknown`，继续用现有 v11 解析器。
  - `options.snowluma_min_version` 覆盖 SnowLuma 兼容下限（默认 `2.4.0`）；只有实现名
    能确认是 SnowLuma 时才比较，低版本写
    `version.adapter.compat.state = below_declared_floor` 并打日志，**不断连**。
    带预发布后缀/无法解析的版本记 `unknown`，不猜大小。
  - `crates/kernel/src/deps.rs`：`PlatformVersionInfo` 增加 `compat_floor` / `compat_state`；
    依赖条目 `version.adapter.compat` 序列化 `{protocol, snowlumaMin, state}`。
  - `crates/bridge/src/console.html`：依赖表在连接 id 下显示协议/实现/适配器/兼容状态摘要。
  - 文档：`docs/plan/adapter-snowluma.md` 更新 4.2/4.3/4.4、功能域 #19、计数说明、批次 1 状态；
    `docs/guide/config.md` / `docs/05-adapters.md` / `config.example.toml` 补 `snowluma_min_version`
    与 `/healthz` / `/api/plugins` 可见字段。
- 新增 op：**0 个**。复用已有 `account.version` / `account.status`；没有为探测另加散装 method，
  也没有新增 tag 或第二张 op→risk 表。
- 按文档判断先不做：
  - 运行时能力查询：SnowLuma 没有 `get_capabilities` / `list_actions` 这类运行时动作；
    `@snowluma/mcp` 的目录快照不算运行时接口，不实现。
  - `can_send_*`、资料设置、在线状态设置：文档有动作，但不属于「版本/状态采集」，
    留给后续批次，避免本批扩大范围。
  - 兼容下限低于时不硬失败：上游没有兼容矩阵和版本格式承诺，硬拒绝容易误伤。
- 证据：
  - adapter：`version_probe_normalizes_protocol_and_records_compat`、
    `version_probe_falls_back_to_unknown_without_panicking`、
    `status_probe_requires_retcode_and_online`、
    `compat_floor_marks_below_and_ignores_other_implementations`、
    `compat_floor_comes_from_options_with_default`、
    `ask_returns_none_when_the_action_queue_is_closed`、
    `probe_identity_writes_version_and_status_to_dependencies`（假动作回包 → 依赖条目三层 + accountOnline/Good）。
  - deps：`platform_version_records_three_layers_and_unknown_slots` 断言 `compat.state`。
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：功能域计数仍是 **3 完全 / 12 部分 / 8 未实现**；#19 账号/系统状态仍为「部分」，
  但缺口从「启动不调用版本/状态」缩小为「缺能力查询、资料设置、在线状态设置」。
- 未决/风险：没有真实的 SnowLuma 实例可联调；`app_name` / 版本格式按官方文档字段解析，
  实际命名差异会落 `unknown`，不会阻塞连接。

## 2026-11-05 — 批次 2：事件通道与平台健康

- 任务编号：adapter-snowluma 批次 2
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/event.rs`：`meta_event` 分流；
    `meta_event_type = heartbeat` → 新增 `Parsed::Heartbeat`，其它 meta（lifecycle）仍
    `Ignored`。心跳不解析未文档化的 `status.online` / `good` 字段。
  - `crates/adapters/onebot-snowluma/src/link.rs`：`Parsed::Heartbeat` →
    `ctx.status.heartbeat()`；`bot_status` / `bot_offline` 继续走 `Parsed::AccountStatus` →
    `ctx.status.account(...)`，不产生房间记录。
  - `crates/kernel/src/adapter.rs`：新增 `AdapterStatus::heartbeat()`。
  - `crates/kernel/src/deps.rs`：新增 `DependencyRegistry::platform_heartbeat`，只在
    `state = Up` 时刷新 `lastOkAt`，不改变 up/down；`bot_status` 离线走既有失败阈值，
    上线走 `mark_up` 并触发 `NoticeEvent::DependencyEdge`，由既有 notifier 消费
    （恢复通知不再另写一套）。
  - `message_sent` 继续整体忽略，覆盖 `reportSelfMessage = true` 的重复风险。
- 新增 op：**0 个**。批次 2 全是事件分流与健康上报，没有需要暴露的平台动作；
  `account.status` / `account.version` 已在 catalog 中。没有新增 `PlatformTag`，
  没有第二张 op→risk 表。
- 按文档判断不做：
  - `group_upload` / `essence` / `notify.*` 的字段解析：SnowLuma 文档未成文，不猜字段，
    保留 `[类型]（未解析事件）` + Opaque 显式降级，并记入功能域 #20 缺口。
  - `meta_event` 的 `status.online` / `good`：字段未成文，不解析；心跳只当存活信号。
  - `lifecycle`：不写流水、不驱动状态，保持忽略。
- 证据：
  - adapter：`heartbeats_are_liveness_only_and_never_records`（连续 10 条心跳均为
    Heartbeat，lifecycle 为 Ignored）、`bot_status_is_an_account_signal_not_a_private_room`
    （online / offline / bot_offline 三种状态都不产生房间）、
    `own_message_echoes_are_not_recorded_again`、
    `unrecognized_notices_keep_the_raw_event_in_an_opaque_element`。
  - deps：`platform_heartbeat_refreshes_only_up_entries`（down 后心跳不拉回 up、不刷新
    lastOkAt）、`account_status_edges_down_and_back_up_with_a_notice`（up→down→up 三条边沿，
    恢复走既有 DependencyEdge）。
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：功能域计数仍是 **3 完全 / 12 部分 / 8 未实现**；#20 事件类型仍为「部分」，
  但缺口从「`bot_status` 污染私聊房间、心跳不更新健康」缩小为「`group_upload` / `essence` /
  `notify.*` 字段未成文，只能显式降级」。平台依赖条目现在会同时反映连接与账号上下线。
- 未决/风险：没有真实 SnowLuma 实例抓包，`bot_offline` 到底是 `notice_type` 还是
  `sub_type` 仍按现有兼容写法处理（两种都落到「离线」）；真实字段差异只影响摘要文案，
  不影响「不写房间」。

## 2026-11-05 — 批次 3：目录扩展（消息 / 群管 / 成员 / 好友 / 请求 / 公告）

- 任务编号：adapter-snowluma 批次 3
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/catalog.rs`：新增 18 个层级 op、18 条路由，
    全部有显式 `Risk`、tag、`TargetSpec`、参数声明；50 op / 53 action → 68 op / 71 action。
  - 消息：`msg.face` → `send_custom_face`（Write；按房间补 `group_id` / `user_id`；
    `RecordMode::Message`，流水写「[自定义表情]」）。
  - 群管理：`member.kick_many`、`group.member_permissions`、`group.invite_policy`、
    `group.history_visibility` 全部 `Risk::Destructive`，继续受 `allow_destructive` 约束；
    `group.admin_settings` 只读。
  - 群信息：`group.share_link`、`group.detail`、`group.shut_list`、`group.signed_list`（Read）。
  - 好友：`friend.remark`、`friend.set_category`（Write）；`friend.categories`、
    `friend.one_way`、`friend.doubt_list`（Read）；`friend.doubt_approve`（Write）。
  - 请求：`group.ignored_requests` → `get_group_ignored_notifies`（Read，连接级）。
  - 公告：`group.notice.delete` → `_del_group_notice`（Destructive，`Effect::Delete`）。
  - `crates/adapters/onebot-snowluma/src/outbound.rs`：`validate_call_params` 补 core 管不到的
    约束——`msg.face` 的 `emoji_id` 非空且 ≤ 256 字符；`member.kick_many` 的 `user_ids`
    非空、≤ 20 个整数；`group.notice.delete` 的 `fid` / `notice_id` 至少一个非空；
    `friend.set_category` 的 `categoryId` / `categoryName` 至少一个。
  - `crates/i18n/locales/{zh-CN,en}.json`：新增 4 条参数校验错误文案。
- 新增 op：**18 个**（全部在 `catalog()` 的 `methods()` 里，catalog 是唯一 op→risk 表）。
  没有新增 `PlatformTag`，没有第二张 risk 表，没有引入兼容别名。
- 按文档判断不做（记缺口）：
  - `upload_forward_msg` / `upload_foward_msg` 与群/私聊专用转发别名：现有
    `msg.send_forward` 已覆盖发送；预上传返回 `res_id`，现有发送路径不接受它，且会把
    任意大小的转发节点载荷搬进 core，属媒体/附件类通道，先不暴露。
  - `set_group_add_option`：`add_type` 取值语义文档未列，不猜。
  - `set_group_search`：`no_finger_open` / `no_code_finger_open` 是未文档化的整数开关。
  - `add_group_tag` / `delete_group_tag` / `get_group_tags`：`group_class` 不是群号，
    映射关系未成文，不暴露。
  - `get_group_ignore_add_request`：与 `get_group_ignored_notifies` 重叠的 NapCat 兼容字段，
    只保留后者。
- 证据：
  - `batch_three_ops_have_explicit_risk_and_routes`（18 个新 op 的 risk / 路由逐个断言，
    破坏性群管理带 `requires_admin` 标签）
  - `custom_face_uses_the_current_room_as_target`、`custom_face_is_recorded_as_a_message`
  - `custom_face_rejects_empty_and_oversized_ids`
  - `batch_kick_rejects_empty_oversized_or_non_integer_lists`
  - `notice_delete_requires_fid_or_notice_id`
  - `friend_category_requires_a_target`
  - `every_op_is_hierarchical_tagged_and_routed`（68 op 全部层级名、有 tag、有路由）
  - `account_level_methods_are_connection_scoped`（新增连接级好友/请求 op 的作用域）
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：功能域计数从 **3 完全 / 12 部分 / 8 未实现** 变为
  **7 完全 / 8 部分 / 8 未实现**；#11 群信息、#12 好友、#13 请求、#14 公告升为「已实现」，
  #8 消息收口（补 `msg.face`），#10 群管理仍为「部分」，缺口缩为加群选项、群搜索、群分类标签。
- 未决/风险：没有真实 SnowLuma 实例联调；`get_group_ignored_notifies` 等扩展动作的实际
  回包形状按文档字段声明，字段差异只会体现在返回内容，不影响 catalog 与风险闸门。

## 2026-11-05 — 批次 4：文件 CRUD 与表情扩展

- 任务编号：adapter-snowluma 批次 4
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/catalog.rs`：新增 15 个层级 op、15 条路由；
    68 op / 71 action → 83 op / 86 action。
  - 群文件：`group.file.folder_create`、`group.file.folder_delete`（Destructive，
    `Effect::Delete`）、`group.file.move`、`group.file.rename`；`file.private_url`
    连接级取私聊文件链接，强制显式 `user_id`。
  - 表情回应：`msg.reactions`（get_msg_emoji_likes）、`msg.reaction_users`
    （fetch_emoji_like）、`group.reaction`（set_group_reaction，按房间补 group_id）。
  - 收藏/系统表情：`face.custom_list`、`face.custom_detail`、`face.custom_delete`
    （Destructive，`Effect::Delete`）、`face.custom_modify`、`face.system_list`、
    `face.system_search`、`face.entity`。
  - `crates/adapters/onebot-snowluma/src/outbound.rs`：`validate_call_params` 增加
    空值与范围校验（文件夹名/目录/文件名、私聊 user_id、消息 id、emojiId/code、
    query、face_id）。
  - `crates/kernel/src/engine.rs` + `tools.rs`：目录清单分页。`LIST_INDEX_BYTES`
    之外新增 `CATALOG_PAGE_DEFAULT = 20` / `CATALOG_PAGE_MAX = 60`；列目录支持
    `limit` / `offset`，返回 `_page`，无过滤且超过 4 KiB 时自动只回一页。
  - `harnesses/shared/tools.mjs`：`platform_call` 的 limit/offset 说明覆盖目录分页。
  - `crates/i18n/locales/{zh-CN,en}.json`：新增 3 条参数校验文案。
- 新增 op：**15 个**（全部在 `catalog()` 的 `methods()` 里，catalog 是唯一 op→risk 表）。
  没有新增 `PlatformTag`，没有第二张 risk 表，没有兼容别名。
- 目录体积实测（临时测量测试，测完已删除）：
  - 83 op 未分页清单：约 **17,329 B**；
  - 索引化（去 tags、摘要截 80 字符）后：约 **11,975 B**；
  - 一页 20 个索引化 op：约 **2,893 B**（engine 还会加 tags 概览与 `_page` 信封）。
  两个全量值都超过 `LIST_INDEX_BYTES = 4096`，所以自动分页生效。
- 按文档判断不做（记缺口）：
  - `rename_group_file_folder`：`new_folder_name` / `name` 两个参数并存且默认空，优先级未成文。
  - `get_share_link`：只收 `fileset_id`，不接受群 `file_id`，不属于群文件域。
  - `trans_group_file`：没有目标参数，转存到哪里未文档化。
  - `add_custom_face`：`file` 格式未成文，且属附件上传通道。
  - `get_emoji_likes`：与 `fetch_emoji_like` 重叠，只保留后者。
- 证据：
  - `batch_four_ops_have_explicit_risk_and_routes`（15 个新 op 的 risk / action / effect 逐个断言）
  - `state_dependent_ops_state_their_preconditions`（删除/移动/回应类描述含「必须存在」前提）
  - `group_file_crud_requires_non_empty_values`、`private_file_url_requires_a_positive_user`、
    `face_and_reaction_identifiers_are_validated`
  - `catalog_declares_effect_and_record`（文件夹删除、收藏表情删除均为 `Effect::Delete`）
  - `catalog_list_pages_by_offset_and_limit`、`catalog_list_still_rejects_execution_shaping_fields`
  - `every_op_is_hierarchical_tagged_and_routed`（83 op 全部层级名、有 tag、有路由）
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：功能域计数仍是 **7 完全 / 8 部分 / 8 未实现**；#15 群文件与 #17 表情回应
  仍为「部分」，但缺口分别缩为文件夹重命名/分享链接/转存与收藏表情新增/置顶。
- 未决/风险：没有真实 SnowLuma 实例联调；`fetch_sys_faces` 等返回结构按文档字段声明，
  字段差异只影响结果内容，不影响 catalog 与风险闸门。

## 2026-11-05 — 工具清单收口到 30 以内

- 任务来源：`docs/plan/extension-point-catalog.md` 的「工具数上限 30」计划项。
- 做了什么：`TOOL_NAMES` 从 **39 → 28**，4 个合并入口，全部用封闭 op：
  - `notes`：`get_notes` / `set_note` / `del_note` → `op=list/set/del`；
  - `tasks`：`task_list` / `task_get` → `op=list/get`；
  - `feedback`：`feedback_list` / `feedback_get` → `op=list/get`；
  - `memory_git`：`memory_git_status/log/diff/blame/add/commit/push/branch` → 8 个 op。
- 实现：
  - `crates/kernel/src/tools.rs`：新 `NotesToolArgs` / `TasksToolArgs` / `FeedbackToolArgs` /
    `MemoryGitToolArgs`；`TOOL_NAMES` 删旧名、加 4 个入口。
  - `crates/kernel/src/engine.rs`：单入口按 op 分派到原实现；未知 op 返回
    `[bad_params] unknown op ...; valid ops: ...`；缺必填字段返回 `op_requires`。
  - 权限/边界保留：notes 仍走 `ensure_readable` 与票据 `notes:*` 能力；`tasks` / `feedback`
    仍在迟到调用白名单；`feedback_reply` 不并入 `feedback`，避免把回复放进迟到放行。
  - 没有兼容别名：旧工具名从 `TOOL_NAMES`、分派和 `harnesses/shared/tools.mjs` 一次删净。
- schema 体积实测（`TOOLS` 的 `JSON.stringify` 求和）：
  - 合并前：39 个工具，schema 12,862 B，全量工具定义 21,737 B；
  - 合并后：28 个工具，schema 11,628 B（-1,234 B，-9.6%），全量 19,871 B（-1,866 B，-8.6%）；
  - 按 0.296 tokens/byte 粗估：schema 约 3,807 → 3,442 tokens（-365），全量约
    6,434 → 5,882 tokens（-552）。
- 不合并的族（判断）：
  - `access_request` / `access_ticket_get`：参数形状与安全边界不同；
  - `extensions_list/read/write/reload`：每个 op 的管理员检查不同，合并会扩大单次调用的权限面；
  - `plugin_tools` / `plugin_tool_call`：索引与调用是两种请求形状；
  - `memory_read/write/list/search`：参数差异大且高频；
  - 消息/历史/等待/平台入口：高频且参数简单，合并会变成 discriminated union。
- 证据：
  - `merged_tool_ops_are_closed_and_report_candidates`（4 个入口未知 op 报候选；notes 缺
    value 报明确错误）
  - `turn_flow` 笔记用例、`task_flow` / `drain_flow` 任务用例、`feedback_flow` 反馈用例
    全部改用新入口，原有行为通过。
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 文档：`extension-point-catalog.md` 把「计划，不实现」改为实装结果，并附
  「旧工具名 → 新入口 + op」对照表；6.1 表的旧工具名同步改成合并后的入口。

## 2026-11-05 — 批次 5：连接形态与鉴权（反向 WS）

- 任务编号：adapter-snowluma 批次 5
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/factory.rs`：manifest `transports` 增加
    `ws-server`；`create` 对 `ws-server` 只要求监听地址，不再一律要求 `url`。
  - `crates/adapters/onebot-snowluma/src/link.rs`：
    - `supervise` 按 `connection.transport` 分派到 `supervise_client` / `supervise_server`；
    - `supervise_server` 用 `TcpListener` 监听 `options.listen`（或
      `url = "ws://host:port"`），非阻塞 accept，握手完成后复用原有 `drive_ws`
      （安装发送槽、`probe_identity`、跑 pump）；一次只服务一条活动连接；
    - 握手鉴权：`token` 非空时接受 `?access_token=`（含 `%XX` 解码）或
      `Authorization: Bearer`，不匹配返回 401；日志不打印 token；
    - 监听/accept 失败标 `failed` 并退出；客户端断开后继续监听下一条。
  - `crates/i18n/locales/{zh-CN,en}.json`：新增反向 WS 监听/接入/拒绝/错误文案。
  - `config.example.toml`、`docs/05-adapters.md`、`docs/guide/config.md`：
    补 `ws-server` + `options.listen` 配置说明。
- 新增 op：**0 个**；新增工具位：**0 个**（工具数保持 28）。本批只改连接方式，
  `platform_call` 仍是唯一平台入口。
- 目录分页复核：op 数仍是 83，默认 20 条/页，一页约 3–4 KB（与批次 4 相同），
  没有因为本批产生新的单页膨胀。
- 按文档/成本判断不做：
  - HTTP API transport（`httpServers`）：要新增 HTTP 客户端依赖与独立动作通道，当前无部署只用 HTTP。
  - HTTP 事件上报接收端（`httpClients`）：要入站 HTTP 服务、去重与鉴权，文档未给签名细节。
  - `wss://`：要 TLS 依赖；现有部署是内网/同机 `ws://`，未确认前不声明支持。
  - 多 transport 独立 token：没有 HTTP transport，拆字段没有实际收益。
- 证据：
  - `manifest_keeps_persistent_ids_and_uses_the_snowluma_display_name`（manifest 同时声明
    `ws-client` / `ws-server`）
  - `ws_server_listen_prefers_options_and_falls_back_to_url`
  - `reverse_ws_token_accepts_query_and_bearer`（正确 query / Bearer、错误 token、无 token、
    `%XX` 解码）
  - 既有 `probe_identity_writes_version_and_status_to_dependencies`、
    `bot_status_is_an_account_signal_not_a_private_room`、`heartbeats_are_liveness_only_and_never_records`
    继续通过，证明反向连接复用的探测/事件路径没退化。
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：功能域计数 **7 完全 / 8 部分 / 8 未实现 → 8 完全 / 8 部分 / 7 未实现**；
  #3 反向 WS 升为「已实现」，HTTP API / HTTP 事件上报 / `wss://` 仍是「未实现」。
- 未决/风险：反向 WS 一次只服务一条活动连接；多客户端并发需要额外的路由/替换策略，
  当前 Deployment 只有一个 SnowLuma `wsClients`，不扩大范围。

## 2026-11-05 — 批次 6：媒体增强（流式按判断不做）

- 任务编号：adapter-snowluma 批次 6
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/catalog.rs`：新增 6 个层级 op、6 条路由；
    83 op / 86 action → 89 op / 92 action。
  - `msg.voice_text` → `fetch_ptt_text`（Read）：语音消息转文字。
  - `msg.ocr` → `ocr_image`（Read）：图片 URL 或缓存 file_id 的文字识别。
  - `file.info` → `get_file`（Read，连接级）：缓存图片/语音的路径、地址、大小。
  - `group.ai_characters` → `get_ai_characters`（Read）：群可用 AI 语音角色。
  - `group.ai_voice` → `get_ai_record`（Write）：只生成语音，不发送。
  - `group.ai_voice_send` → `send_group_ai_record`（Write，`Effect::Send`，
    `RecordMode::Message`）：生成并发送 AI 语音，流水写「[AI 语音]」。
  - `crates/adapters/onebot-snowluma/src/outbound.rs`：补充语音 id、OCR 地址长度、
    `file.info` 标识、AI 语音 `character`/`text` 的空值/长度校验。
  - `crates/i18n/locales/{zh-CN,en}.json`：新增 `file.info` 与长度上限文案。
- 新增 op：**6 个**；新增工具位：**0 个**（工具数保持 28）。
  没有新增 `PlatformTag`，没有第二张 risk 表。
- 目录体积复核：op 数 89；默认 20 条/页的页面仍是同样 20 条，约 **2.9 KB**；
  全量清单约 18 KB、索引化后约 12–13 KB（按 83 op 的实测线性外推）。
- 按文档/风险判断不做：
  - 原始流式动作（`upload_file_stream`、`download_file_image_stream` /
    `download_file_record_stream` / `download_file_stream`、`test_download_stream`）：
    多帧协议、无逐请求取消、WS 无背压，且会把大文件字节搬进 core，本轮不接。
  - `download_file`：任意 URL/base64、无文档化大小上限，等于把平台当下载器，不暴露。
  - `clean_stream_temp_file`：没有流式动作就没有清理入口的必要，留给流式一起做。
  - `musicSignUrl`：SnowLuma 配置项，不是 OneBot 动作，不能变成 op。
- 连接类改动：**本批没有改连接代码**；沿用批次 5 的
  `reverse_ws_token_accepts_query_and_bearer` / `ws_server_listen_prefers_options_and_falls_back_to_url`，
  role 仍是「单活动连接 + 必须配 Universal」。
- 证据：
  - `batch_six_media_ops_have_explicit_risk_and_routes`（6 个 op 的 risk/action/effect；
    AI 语音发送是 `Effect::Send` + `RecordMode::Message`）
  - `media_ops_state_their_preconditions`（语音必须是语音消息、OCR 失败报错、
    `file.info` 至少一个标识、AI 语音 text 非空）
  - `media_and_ai_identifiers_are_validated`
  - `ai_voice_send_is_recorded_as_a_message`
  - `every_op_is_hierarchical_tagged_and_routed`（89 op 全部层级名、有 tag、有路由）
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：计数仍是 **8 完全 / 8 部分 / 7 未实现**；#18 语音/媒体仍为「部分」，
  缺口缩为原始流式上传下载、`download_file`、音乐签名；#16 流式传输仍未实现。

## 2026-11-05 — 批次 7：扩展域收尾（只读群相册）

- 任务编号：adapter-snowluma 批次 7
- 做了什么：
  - `crates/adapters/onebot-snowluma/src/catalog.rs`：新增 2 个只读 op、2 条路由；
    89 op / 92 action → 91 op / 94 action。
  - `group.album.list` → `get_group_album_list`：群相册列表。
  - `group.album.media` → `get_group_album_media_list`：相册媒体列表，`album_id` 必填，
    `attach_info` 原样带回上一页 `nextAttachInfo`。
  - 两个 op 均为 `Scope::Room`、`Risk::Read`、`Effect::Read`，没有状态变更。
  - `crates/adapters/onebot-snowluma/src/outbound.rs`：`group.album.media` 的
    `album_id` 空值校验。
- 新增 op：**2 个**；新增工具位：**0 个**（工具数保持 28）。
  没有新增 `PlatformTag`，没有第二张 risk 表。
- 目录体积复核：op 数 91；默认 20 条/页仍是同样 20 条，约 **3 KB**；
  全量清单约 18–19 KB、索引化后约 13 KB，自动分页继续生效，没有调页大小。
- 产品范围判断不做（记缺口）：
  - 群相册上传/删除/评论/点赞不暴露；文档也没有创建/删除相册动作。
  - QQ 空间 26 个动作全部不暴露（账号级社交面）。
  - 收藏/闪传 115 个 extended 不排期；`send_packet`、`request_decrypt_key`、
    `get_credentials`、`get_cookies`、`get_clientkey`、`set_restart`、`bot_exit`、
    `clean_cache` 永不暴露。
- 证据：
  - `batch_seven_album_ops_are_read_only_and_routed`（2 个 op 只读、路由正确，
    catalog 不含 album upload/delete）
  - `album_media_requires_an_album_id`
  - `every_op_is_hierarchical_tagged_and_routed`（91 op）
  - `cargo test --workspace`、clippy `-D warnings`、dsh/opencode 测试、`node --check` 全绿。
- 能力状态：计数 **8 完全 / 8 部分 / 7 未实现 → 8 完全 / 9 部分 / 6 未实现**；
  #21 群相册升为「部分」，#22 QQ 空间 / #23 收藏闪传仍为「未实现」（产品范围）。

## 2026-11-05 — 载体级 AdapterStatus 上报收口

- 目标：让 in-process / dynamic / process 三种平台载体在「载体没加载、退出、panic」
  时都能进依赖表，不再停留在 `unknown` 或假的 `up`。
- 做了什么：
  - `crates/kernel/src/adapter.rs`：
    - 新增 `carrier_create_failed`：`create_adapter` 失败（in-process 工厂、动态库
      `dlopen`/入口校验、进程首次启动失败）时 `attempt` + `failed("carrier")`，
      硬失败立即 down。
    - 新增 `run_adapter`：core 统一包住 `launch`，正常退出但 shutdown 未置位时
      按载体消失报 `failed("carrier")`；panic 也立即 `failed("carrier")`。
    - 正常 shutdown 仍报 `disconnected("closed")`，不改变原有语义。
  - `crates/bridge/src/main.rs`：单条连接 `create_adapter` 失败不再 `?` 退出整个桥；
    标 down 后继续启动其他连接。`launch` 统一走 `run_adapter`。
  - `crates/bridge/src/console.html`：依赖卡新增「账号在线」列，直接显示现有
    `accountOnline`（在线/离线/未知），不新增重复字段；载体失败看 `lastErrorClass=carrier`
    且账号列为未知，账号离线看 `accountOnline=false` + `lastErrorClass=account_offline`。
- 取舍：
  - 载体创建失败后该连接没有 outbound；向它发送会得到明确「no outbound connection」，
    不会假装成功。桥继续运行，`/healthz` 能看到这条连接 down。
  - 进程载体自身的 `supervise` 仍在管子进程重启；`run_adapter` 只兜底「launch 返回/panic」，
    没有重复实现进程监督。
- 证据：
  - `carrier_create_failure_marks_down_immediately`（`state=down`、`lastErrorClass=carrier`、
    `accountOnline=null`、`attempts=1`）
  - `all_carrier_kinds_share_the_same_failure_mapping`（in-process / dynamic / process 同一映射）
  - `adapter_carrier_exit_after_connect_marks_down_not_stuck_up`
  - `adapter_carrier_panic_marks_down_immediately`
  - `hard_and_soft_carrier_failures_have_distinct_thresholds`（单次软失败仍 up；硬失败立即 down）
  - `/healthz` 依赖条目实际片段：
    `{"id":"dyn-adapter","kind":"platform","state":"down","accountOnline":null,`
    `"attempts":1,"lastErrorClass":"carrier","lastError":"dlopen failed","neverConnected":true}`
- 验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode、`node --check` 全绿。

## 2026-11-05 — supervisor 自动回滚接进真实循环

- 目标：让 `acb-supervisor-policy::decide` 真正被监督循环消费，回滚落到「切回 LKG」
  并可审计、可证明执行。
- 做了什么：
  - 新增 crate `crates/supervisor`（binary `acb-supervisor`）：
    - `Supervisor::tick()` 按 probe 间隔采样 → 构造 `Snapshot` → `evaluate_periodic`
      （`Arc<dyn Policy>`，`recv_timeout(decision_timeout_ms)`）→ `execute`；
    - 进程退出走内置策略同步决策（`process_exit`），故障路径不调用外部策略、不等待；
    - `execute` 真正执行 `Fail`/`Hold`/`Rollback`：Hold 按退避重启当前版本，
      Fail 停止自动恢复，Rollback 停当前进程并启动 LKG；
    - 审计 JSONL `state_dir/supervisor.log`：`decision` / `execute` / `failure` /
      `probe` / `policy_error` / `lkg_promoted` / `lkg_write_failed`，每条带决策依据、
      目标与结果；
    - LKG 指针 `state_dir/lkg.json`（program + args + promoted_at_ms），临时文件 +
      rename；稳定窗口（默认 60s）健康才提升；回滚成功后内存指针清空，等重新稳定
      再提升。
  - 策略可替换：`[supervisor.policy].provider` 默认 `acb.supervisor.default`
    （直接调用 `decide`）；`command` 走 stdin/stdout JSON；外部策略只用于周期触发。
  - `deploy/systemd/acb-supervisor.service` + `deploy/supervisor.example.toml`：
    systemd 只拉起/重启 supervisor，不碰 NixOS generation、不做健康门禁、不切版本。
- 取舍（保守侧）：
  - 回滚 stop/start 失败都直接 `failed` 并停止自动恢复，不在两个版本之间来回切；
  - 回滚后 LKG 置空，回滚版本需再稳定 `stable_window_secs` 才重新成为 LKG；
    若它再次失败则 `fail`（不尝试更早版本）；
  - `fail` 不杀正在运行的进程，supervisor 进程保持存活供检查；systemd 不重启它；
  - `policy_error_fail_after=3` 按文档语义：单次错误 `hold`，连续 3 次 `fail`。
    用户口述的「3 次转 hold」与参数名/文档冲突，按参数名和文档落地，并在
    `supervisor-policy-interface.md` §9.2 写明。
- 证据：
  - `crates/supervisor/tests/rollback.rs` 8 个测试：回滚成功/失败、超时按
    `policy_error`、连续错误先 hold 后 fail、启动宽限、`unhealthy_secs` 窗口、
    backoff 序列、稳定窗口提升。断言读真实审计与 runner 调用次数，不是只看 `Decision`。
  - 实际回滚日志：
    `{"event":"execute","decision":"rollback","result":"rolled_back","target":{"program":"/lkg"}}`
  - 验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode、`node --check` 全绿。
- 下一步入口：按新口径做 `critical` 风险档位与媒体/文件 op 补全（独立提交）。

## 2026-11-05 — critical 风险档与媒体/文件能力补全

- 目标：按新口径「任何平台功能都必须有 op，风险靠分级和审批管住」，补回此前按
  安全理由拒掉的媒体/文件动作，并把账号凭据/进程运维收进 `critical` 档。
- 做了什么：
  - `Risk::Critical`：默认拒绝，`allow_destructive` 不放行；新增
    `GrantCapability::Critical`，配置常驻授予会在启动期报错，只能由票据携带。
  - 直接 `platform_call` 命中 critical 一律 `ToolNotAllowed`；票据执行路径
    `execute_ticket_platform` 允许 connection-scope critical 并携带票据能力校验。
  - 审批私信显示 `[CRITICAL]` + op/目标 + 脱敏参数摘要（cookie/token/secret/
    password/clientkey/db_key/db_path/authorization/headers，send_packet 的 data
    也脱敏）；结果只进票据存储，`access_ticket_get` 显式查询可见。
  - 新增 27 个 op（91 → 118）/ 27 个 action（94 → 121）：
    - critical 9：`account.credentials` / `account.cookies` / `account.csrf_token` /
      `account.clientkey` / `account.decrypt_key` / `system.send_packet` /
      `system.bot_exit` / `system.restart` / `system.clean_cache`；
    - `file.download`：只收 http/https 或 base64，二选一；`max_bytes` 默认 8 MiB、
      上限 32 MiB（base64 本地预检），只回平台路径/元信息；
    - 流式 6：`stream.upload_file` / `download_image` / `download_record` /
      `download_file` / `test_download` / `clean`；单块 1 MiB、总大小 256 MiB；
      多帧/无逐请求取消/WS 无背压写进 op 描述，下载流当前只能拿首帧；
    - 转发 4：`msg.forward_upload` / `forward_upload_alias` /
      `send_group_forward` / `send_private_forward`；载荷上限 256 KiB / 100 节点；
    - 群相册 7：`qun_list` / `upload_image` / `upload_video` / `delete_media` /
      `comment` / `like` / `unlike`；删除为 Destructive + `Effect::Delete`。
  - 覆盖矩阵：新增 `docs/plan/snowluma-action-coverage.md`。SnowLuma 中文索引 200 个
    唯一动作，已暴露 121，未暴露 79（外部约束 16、未排期 63）。
- 取舍：
  - critical 也允许从目录发现；发现成本靠分页/risk，执行成本靠票据审批。
  - `file.download` 的 URL 大小无法预检（平台下载、文档无上限），`max_bytes` 只对
    base64 强制；跟随重定向由平台决定，已写进 op 描述。
  - 下载流不做多帧收集：现有 `Outbound::call` 是单响应语义，收集所有帧会把文件字节
    搬进 core；先暴露 op + 首帧，完整文件走 HTTP/SDK。
  - `get_csrf_token` 不在用户必列清单里，但它同样是凭据，按 critical 一并收进来。
- 证据：
  - `critical_is_refused_by_allow_destructive_and_runs_only_via_ticket`
  - `config_cannot_grant_critical_capability`
  - `media_and_critical_ops_are_routed_with_limits`
  - `batch_seven_album_ops_are_read_only_and_routed`（已扩写）
  - `every_op_is_hierarchical_tagged_and_routed`（118 op）
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode、
    `node --check` 全绿。
- 下一步入口：QQ 空间 10 个动作 + 收藏/闪传 115 个 extended；按新档位另起批次。

## 2026-11-05 — 票据词表、notes 分页与旧工具名清理

- 做了什么：
  - `access_request` 描述里的 `get_notes` / `set_note` / `del_note` 改为
    `notes:list` / `notes:set` / `notes:del`，与 `ticket_capabilities` 一致。
  - `notes op=list` 补 `limit` / `offset`（默认 20、上限 100）、`total` / `returned` /
    `has_more` / `next_offset`，并按 4 KiB 做索引降级；索引省略 `value`，可用 `key`
    精确查回单条。`kind` 描述拆开：list 不传=全部，set/del 不传=term。
  - `memory_git op=log` 的 `limit` clamp 到 1..=100，默认 10 不变。
  - `docs/02-glossary.md` 工具表按 28 个工具重写；同步修正
    `01-vision.md` / `03-architecture.md` / `06-harness-adapters.md` / ADR-0006 /
    ADR-0017 / ADR-0022 / ADR-0029 / `extension-point-catalog.md` /
    `capability-gap-matrix.md` 里的旧名或旧计数；历史计划文档加迁移注记。
- 证据：
  - `notes_list_paginates_clamps_and_indexes_large_pages`
  - `ticket_ops_use_the_notes_entry_names`
  - `engine::limit_tests::memory_git_log_limit_is_clamped_to_100`
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode、
    `node --check` 全绿。

## 2026-11-05 — 工具清单与票据词表的跨层一致性测试

- 做了什么：
  - 新增 `crates/kernel/tests/tool_schema_consistency.rs`：轻量解析
    `harnesses/shared/tools.mjs` 的 4 空格 `name:` 字段，与 `TOOL_NAMES` 双向比对；
    失败信息分列「harness 有、内核没有」和「内核有、harness 没有」。
  - 把 `ticket_capabilities` 的固定 op 表抽成 `engine::TICKET_OPS`，行为与测试共用
    同一份常量；测试解析 `access_request` 描述里的 op 词表并双向比对。
- 取舍：不引入 JS 运行时；用缩进 + `name: "..."` 的窄解析避免匹配嵌套 schema。
- 证据：
  - `tool_names_match_the_shared_schema`
  - `access_request_description_lists_exactly_the_kernel_ticket_ops`
  - 全量验证：`cargo test --workspace`、clippy `-D warnings` 全绿。

## 2026-11-05 — 新增只读 `capabilities` 工具（第 29 个）

- 做了什么：
  - 新工具 `capabilities`：`scope=session` 给整体摘要与计数；`scope=room` 给房间
    订阅/可读/可写与平台 op 的 risk、direct_executable、ticket_required、
    requestable、allow_destructive；`scope=tool` 给单个工具或 op 的稳定原因码。
  - 判定复用现有路径：`check_access` 增加 `probe=true` 分支，只回答不消费票据；
    `grants` / 外部 access provider / `connection_allows_destructive` / `TICKET_OPS`
    都是同一套实现，没有第二套判断。
  - tickets 块只统计本会话；`AccessTickets` 新增只读 `live_counts` 与
    `live_groups_for_turn`，不触发过期扫描。
  - 目录/大列表超过 4 KiB 走 `maybe_index_list` 降级；`scope=room` 的 118 个 op
    会自动索引。
  - `send_message` / `send_message_to` / `access_request` 描述加了「被拒先查
    capabilities」的指引；`tools.mjs` 与 `TOOL_NAMES` 同步到 29，跨层一致性测试
    自动覆盖。
- 取舍：`scope=tool` 的未知 op 返回 `{found:false,reason:"unknown_op"}` 而不是抛错，
  便于模型一次改对；工具不可用原因用稳定短码，不返回别的会话的票据信息。
- 证据：
  - `session_scope_summarizes_tools_rooms_and_tickets`
  - `room_scope_separates_subscription_read_and_write`
  - `tool_scope_reports_stable_reasons`
  - `ticket_counts_do_not_leak_other_sessions`
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode、
    `node --check` 全绿。

## 2026-11-05 — 票据申请准入（access grants 第一笔）

- 做了什么：
  - 新增 `GrantCapability::AccessRequest` 与 `grants::evaluate_access_request`；
    默认口径：群友只能申请本会话已订阅目标；`system_admin` / `bot_admin` 在私信
    或控制台可申请任意目标；显式 `[[grants.rules]]` 才能放开跨目标。
  - `access_request` 申请前先过 grants，再跑 fail-closed 预检：连接存在、op 在
    catalog、参数形状、`destructive` 未开 `allow_destructive` 时直接
    `[destructive_disabled]` 且不建票不发私信。
  - `/approve` `/deny` `/revoke` 身份改走 `grants.subject_for`（system_admin 或
    bot_admin），不再硬编码账号；允许/拒绝都写 `[access-approval]` 审计行。
  - 审批私信新增 `申请者：<id>（<显示名>）`；票据审计行补 `sender` /
    `sender_name` / `reason`；申请被拒写 `[access-request] denied ... code=...`。
  - 冷却与额度数值不变。
- 第二笔（同日完成）：`AccessTarget` 类型化、`account_read` / `account_secret`、
  按 kind 匹配规则、typed target 审批展示、`any` 的可达范围，见下一条。
- 已知接受：管理员冷却是全局单时间戳，存在轻量 DoS；已写进 access-grants.md。
- 证据：
  - `group_user_defaults_to_subscribed_targets_only`
  - `bot_admin_can_approve_but_ordinary_user_cannot`
  - `destructive_platform_call_is_refused_at_request_time`
  - `access_request_defaults_to_subscriptions_except_admin_private`
  - `batch_approve_sets_each_ticket_active_and_audits`（sender/name 审计）
  - 全量验证：`cargo test --workspace`、clippy `-D warnings` 全绿。

## 2026-11-05 — typed target（access grants 第二笔）

- 做了什么：
  - 新增 `AccessTarget { kind, id, sub }` 与封闭枚举 `AccessTargetKind`
    （`room` / `message` / `member` / `file` / `account` / `connection` /
    `plugin_tool`）；规则、票据、审批展示、审计统一用同一形状。
  - `GrantRule.target` 改为 typed 表；`scope = room` / `connection` 必须给
    typed target，并校验 `kind` 与 `id`。旧字符串 target 在配置解析期报错。
  - 能力表补 `account_read` / `account_secret`：`account_secret` 与 `critical`
    同级，写进 rules 启动失败；账号级 critical 票据能力集自动带
    `account_secret`，审批私信可见。
  - `scope = any` 改为「本会话可达目标」：房间/连接来自订阅，账号来自订阅连接，
    成员/消息/文件挂可达房间；跨目标必须显式写 `scope` + typed target。
  - `MethodSpec::access_kind()` 由 catalog 的 scope/名字命名空间决定 grants
    目标种类；票据申请与执行都校验 `[target_kind_mismatch]`。
  - 审批私信与票据审计显示 `房间 ...` / `账号 ...（连接 ...）` 等 typed 文本。
- 证据：
  - `any_does_not_escape_the_sessions_reachable_targets`
  - `config_cannot_grant_critical_capability`（critical / account_secret）
  - `critical_is_refused_by_allow_destructive_and_runs_only_via_ticket`
    （审批展示 `账号 bot（连接 qq）`）
  - `mixed_target_kinds_are_refused_before_a_ticket_exists`
  - `catalog_target_kinds_match_grant_kinds`
  - `system_admin_private_grants_stay_out_of_groups`
  - 全量验证：`cargo test --workspace`、clippy `-D warnings` 全绿。

## 2026-11-05 — 批次 8 + verification 合并

- 做了什么：
  - `MethodSpec.verification: Verified { source } | Unverified { reason, experiment }`
    构造期必填；wire 协议同样必填。unverified 门槛等同 critical：直调拒绝、
    `allow_destructive` 不适用、只能票据 + 审批；审批私信显示 `[UNVERIFIED]` +
    reason 摘要，票据/审计行带 `unverified=true`，catalog 与 `capabilities` 暴露字段。
  - 按 `snowluma-gap-research.md` 落地 A 类：群分类标签改连接级 `group.tag_*`；
    `group.add_option`（add_type 显式、55→5）；`group.search`（open/close → 0/1）；
    CQ 码解析；WS 下载流多帧累计；`httpClients` 可选 HTTP 入口 + HMAC-SHA1 + 原始
    JSON 去重；`wss` 部署前提写进文档；`file.download` 1 GiB / 60 s / http(s)；
    `face.custom_add`；`group.file.folder_rename` / `group.file.trans` /
    `file.share_link`；`group_upload` / `essence` / `notify.*` 字段解析。
  - op 118 → 127，action 121 → 130；覆盖矩阵未暴露 79 → 70（外部约束 8、未排期 62）。
  - 4 个 op 标 `Unverified`（`group.tag_list` / `group.tag_add` / `group.tag_delete` /
    `group.add_option`），实验设计写进 `adapter-snowluma.md`。
- 证据：
  - `unverified_op_is_direct_denied_and_ticket_approval_shows_the_reason`
  - `catalog_lists_the_unverified_ops`
  - `batch8_params_are_normalized_before_the_platform`
  - `cq_string_is_parsed_into_segments` / `cq_escapes_are_decoded_after_segment_parsing`
  - `download_stream_frames_are_accumulated_until_complete`
  - `group_upload_and_essence_fields_are_parsed` / `notify_subtypes_are_named_and_parsed`
  - `hmac_sha1_matches_the_rfc2202_vector` / `event_dedup_drops_repeats_but_not_heartbeats`
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode `npm test`、
    `node --check harnesses/shared/tools.mjs` 全绿。

## 2026-11-05 — supervisor 接入 systemd（acb-launcher）

- 做了什么：
  - 新增 `crates/launcher`（`acb-launcher`）：读 `state_dir/current.json` 决定启动
    哪个命令，嵌入 supervisor 策略循环；回滚时写 `current.json`=LKG、停子进程、
    以 `EXIT_ROLLBACK=75` 退出，systemd 拉起后自然换版本。
  - `acb-supervisor` 新增 `RollbackMode::{Internal, External}`、`EXIT_OK=0` /
    `EXIT_ROLLBACK=75` / `EXIT_GIVE_UP=78`；External 下 `fail`/回滚写指针失败/
    回滚后版本再失败都以 78 终态退出，不再无限重启。
  - 修复「决策延迟期间子进程退出被 hold 悄悄重启」：延迟后重新检查进程状态，
    退出计入失败并重新评估策略。
  - 新增 `deploy/systemd/acb-launcher.service`（`Type=notify`、`WatchdogSec=120`、
    `Restart=on-failure`、`RestartSec=1`、`RestartForceExitStatus=75`、
    `RestartPreventExitStatus=78`、`StartLimitBurst=3/60s`、`StartLimitAction=none`）。
  - 新增可选 Nix module：`deploy/nix/nixos.nix`、`deploy/nix/home-manager.nix`，
    默认不启用，不依赖 NixOS generation。
  - `EventDedup` 满 4096 条时按时间淘汰最老 1/4，避免高事件量下无界增长。
  - 设计/重启表/退出码契约/实测记录：`docs/plan/supervisor-systemd.md`。
- 证据：
  - `external_rollback_writes_pointer_and_exits_75_without_starting_lkg`
  - `external_fail_is_terminal_and_exits_78`
  - `second_failure_after_rollback_is_terminal_78`
  - `event_dedup_evicts_the_oldest_entries_at_capacity`
  - `./deploy/verify-launcher.sh`：正常重启 MainPID 不变、回滚日志
    `status=75/TEMPFAIL` + `current.json selects lkg`、连续失败 `status=78/CONFIG`
    + `Active: failed` + `MainPID=0`。
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode `npm test`、
    `node --check harnesses/shared/tools.mjs` 全绿。


## 2026-11-05 — 常驻集 10→8、按需注入与别名收敛

- 做了什么：
  - `crates/kernel/src/tools.rs`：`RESIDENT_TOOLS` 10 → 8，移出 `get_history` /
    `fetch_records`；新增 `ON_DEMAND_TOOLS`，这两个工具仍在 `TOOL_CATALOG`，
    但 `capabilities scope=surface` 默认不把它们算进可见集。
  - `harnesses/shared/tools.mjs`：`RESIDENT_TOOL_NAMES` 同步 8 个，新增
    `ON_DEMAND_TOOL_NAMES`；`toolsForSurface(surface, {includeOnDemand})` 只在
    调用方显式要求时给这两个工具完整 schema。
  - dsh / opencode：每轮根据 `delivery.omitted` / `delivery.more` 设置 `onDemand`，
    投递溢出时临时注入 `get_history` / `fetch_records`，普通轮次不注入，下一轮自动收回。
  - `MethodSpec` / `WireMethodSpec` 新增 `aliases`；`find_platform_op` 把别名归一化
    到主 op，`platform_call` 执行时用主 op 名；目录（brief/detail）不展开别名条目。
  - SnowLuma：`msg.send_alias` / `group.file.folder_delete_alias` /
    `group.ignored_requests_alias` / `msg.emoji_likes_alias` / `msg.poke_alias`
    收敛到 5 个主 op 的 `aliases`；op 189 → 184，action 仍 192。
- 取舍：
  - 常驻只留真正的高频路径；回看工具的使用场景框架自己知道，不该让模型预先看见。
  - 别名不单列 op：action 数不变，但目录和权限表少 5 条重复项；risk / target /
    verification 全部继承主 op，不维护第二份。
- 证据：
  - `surface_response_stays_within_budget`：surface 1,877 B（旧 1,762 B；两个按需工具
    进了目录索引）。
  - 空轮次（MCP tools/list 紧凑 JSON）：8 份常驻 schema 7,960 B + surface 1,877 B
    = 9,837 B；同口径旧 10 份 + 旧 surface 为 11,832 B（-16.9%）。
  - `resident_names_match_the_shared_schema` / `on_demand_names_match_the_shared_schema`
  - `投递溢出时临时注入 get_history/fetch_records`（dsh + opencode）
  - `alias_lookups_normalize_to_the_main_op`（内核）
  - `aliases_attach_to_the_main_op_and_stay_out_of_the_catalog`（SnowLuma）
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode `npm test` 全绿。

## 2026-11-05 — 批次 9：剩余 62 个 SnowLuma 动作全部暴露

- 做了什么：
  - `crates/adapters/onebot-snowluma/src/catalog.rs` 新增 62 个层级 op
    （127 → 189），映射 62 个动作（130 → 192）；`ROUTES` 同步补 62 条路由。
    覆盖账号资料/在线状态、群待办/内联键盘/机器人加群选项、收藏/闪传/文件集、
    单条转发/Ark 卡片/分享/翻译、群信息扩展、已读/拍一拍/发消息别名，以及
    QQ 空间 10 个动作。
  - 风险逐条按动作：`account.rkey` / `account.rkey_server` /
    `file.fileset_download` / `file.flash_file_url` 为 `Critical`；
    `group.todo.cancel` / `file.flash_file_delete` /
    `group.file.folder_delete_alias` 与 QQ 空间写操作（send/comment/like/
    unlike/delete/ban/right）为 `Destructive`；资料、在线状态、待办设置、
    上传、重命名、标记已读为 `Write`；纯查询为 `Read`。
  - verification：7 个别名 op（send_msg / delete_group_folder /
    get_group_ignore_add_request / get_emoji_likes / mark_group_msg_as_read /
    mark_private_msg_as_read / send_poke）语义等于已核实动作，标 `Verified`；
    其余 55 个只有 SnowLuma 动作目录来源，全部标 `Unverified { reason,
    experiment }`（research §3.4–§3.8），门槛等同 critical。
  - `DELETE_METHODS` / `record_of` 补上新的删除与发消息 op；
    `docs/plan/snowluma-action-coverage.md` 更新到 192 已暴露 / 8 外部约束 /
    0 未排期；`docs/plan/adapter-snowluma.md` 更新功能域计数。
- 取舍：
  - **全部暴露而不是继续按产品范围裁剪**：按用户口径「任何平台功能均要有，
    可以标高危」。宁可给 `Unverified` + critical 门槛，也不把动作留在目录外。
  - **别名也给独立 op**，不复用旧 op 的名字：覆盖矩阵按 action 计数，独立 op
    才能让「每个 action 都有 op」可被测试逐条验证；平台侧仍只发一个动作。
  - **QQ 空间写操作至少 `Destructive`**：点赞/取消赞也算代表账号对外表态，
    没有降成 `Write`。
  - **`get_flash_file_url` 按 `Critical` 处理**：它返回可直接下载的地址，
    与任意下载入口同档，不因为「只是取 URL」放宽。
  - **未验证语义不硬标 `Verified`**：55 个新 op 保留 reason + experiment，
    清单看起来不干净，但门槛正确。
  - **无新增配置项**：风险仍由 `allow_destructive` + 票据审批控制；
    `config.example.toml` 不需要改。
- 证据：
  - `every_op_is_hierarchical_tagged_and_routed`：189 个 op 逐个检查层级名、
    risk/scope tag、路由、TargetSpec 非空、verification 的 source/reason/experiment。
  - `action_coverage_matrix_is_complete_and_exceptions_have_reasons`：直接解析
    `snowluma-action-coverage.md`，断言 200 个 action、192 已暴露、8 未暴露且逐条
    命中白名单并有非空理由。
  - `catalog_lists_the_unverified_ops`：59 个 unverified op 的逐字清单。
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode
    `npm test`、`node --check` 全绿。

## 2026-11-05 — 白名单清零 + 批次 9 的 Unverified 审计

- 做了什么：
  - 8 个平台占位/未实现动作全部暴露成 `Unverified`：
    `account.model_show`（`_get_model_show`）、`account.model_show_set`（`_set_model_show`）、
    `msg.word_slices`（`.get_word_slices`）、`file.collection_create`（`create_collection`）、
    `friend.recent_contacts`（`get_recent_contact`）、`system.packet_status`
    （`nc_get_packet_status`）、`group.anonymous`（`set_group_anonymous`）、
    `group.anonymous_ban`（`set_group_anonymous_ban`）。op 184 → 192，action
    192 → 200，`UNEXPOSED` 白名单清零。
  - 55 个批次 9 `Unverified` 逐条对 `_snowluma-catalog.json`（190 个运行目录动作，
    含 summary / params）审计：52 个有摘要+参数出处，改 `Verified` 并写出处；
    3 个保留 `Unverified`：`account.qzone.detail` / `msg.tuwen_ark`（运行目录无条目）、
    `account.qzone.unlike`（目录自述「取消赞端点待真机核实」）。
  - 按目录参数修正批次 9 op 的参数名：`set_diy_online_status` 用
    face_id/face_type/wording，`set_input_status` 用 user_id/event_type，
    `set_online_status` 用 status/ext_status/battery_status，`set_qq_profile` 只留
    nickname/personal_note，`set_self_longnick` 用 longNick/long_nick，
    `get_online_clients` / `get_group_info_ex` 加 no_cache，`get_profile_like` 加 start，
    `get_group_todo_*` 用 message_id，`click_inline_keyboard_button` 用
    bot_appid/msg_seq 并改为群路由，`set_group_robot_add_option` 用两个开关，
    `get_collection_list` / `create_flash_task` / `get_flash_file_*` / `get_fileset_*` /
    `download_fileset` 用 fileset_id / share_code / file_name / file_index，
    `translate_en2zh` 用 words，`send_ark_share` / `share_peer` 用
    user_id/group_id/phone_number，空间动作用 tid/target_uin/ugc_right/target_uins。
- 取舍：
  - 白名单只留「技术上无法调用」的动作；这 8 个动作协议上都能调用（只是平台标
    mock/占位/未实现），所以清零，不假装它们不可调用。
  - 审计口径用运行目录（`_snowluma-catalog.json`）而不是中文索引：摘要与参数
    能逐条对照；中文索引才有的 2 个动作不硬标 `Verified`。
  - `Unverified` 从 59 降到 15，仍保留 reason + experiment；不为了清单干净把
    平台占位动作标成 `Verified`。
- 证据：
  - `every_op_is_hierarchical_tagged_and_routed`：192 个 op。
  - `action_coverage_matrix_is_complete_and_exceptions_have_reasons`：200 已暴露 /
    0 未暴露；白名单为空时仍保留「非空则每条必须有理由」。
  - `catalog_lists_the_unverified_ops`：15 个 unverified 的逐字清单。
  - `docs/plan/adapter-snowluma.md` 新增「批次 9 审计表」（70 行：62 个动作 +
    8 个白名单动作），逐条写 `查了哪里` 与结论。
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode
    `npm test` 全绿。

## 2026-11-05 — Verified 依据入库 + 只读真机实验

- 做了什么：
  - 新增 `docs/plan/catalog-snapshots/snowluma-2026-09-29-190-actions.json`：
    上游 Node bridge 仓库 `_snowluma-catalog.json` 原样复制（该仓库 release
    `v0.1.7` / commit `ce726bb` 引入，mtime 2026-09-29 21:54 +0800）。上游没有
    记录生成脚本，也没有版本字段；运行实例 2026-11-05 报 `SnowLuma 2.2.1-node`，
    只作版本参照，不能证明快照由该版本导出。来源/导出方式/过期规则写在
    `docs/plan/catalog-snapshots/README.md`。
  - `catalog.rs` 新增 `CATALOG_SNAPSHOT` / `EXPERIMENT_READONLY` /
    `SNAPSHOT_ACTIONS` / `EXPERIMENT_VERIFIED`。52 个批次 9 op 的 `Verified`
    source 由 `SNAPSHOT_ACTIONS` 生成，格式是
    `docs/plan/catalog-snapshots/snowluma-2026-09-29-190-actions.json#<action>`；
    `group.tag_list` / `account.qzone.detail` 的 source 指向只读实验记录。
  - 新增测试 `verified_sources_resolve_to_snapshot_or_experiment`：解析快照 JSON，
    对每个快照型 source 取 `#` 后的 action 断言存在；反向核对 52 条
    `SNAPSHOT_ACTIONS` 与目录 source；实验型 source 必须命中
    `conclusion=verified` 的探针。
  - 只读真机实验（SnowLuma `2.2.1-node`，账号 `3683924053`）：桥对 `Unverified`
    的 `platform_call` 一律拒绝、console 没有 tool_call 入口，只读探针直接打同一
    实例的 OneBot HTTP API（桥配置里的同一 token），写入类未执行。结果写进
    `docs/plan/catalog-snapshots/snowluma-2026-11-05-readonly-experiments.json`
    与 `adapter-snowluma.md` 的实验表。
  - 实验结论：`account.qzone.detail` 用有效 tid 返回 tid/content/comment_num/
    comments → `Verified`；`group.tag_list` 用 `group_class=0` 返回
    `tags:[]` → `Verified`。其余保留 `Unverified`：`account.qzone.unlike`
    （端点 ok 但无法确认状态变化）、`msg.tuwen_ark`（动作存在、title 必填、发送类
    未执行）、`account.model_show`（mock echo）、`friend.recent_contacts` /
    `system.packet_status`（占位返回空/null）、`msg.word_slices` /
    `file.collection_create`（not yet implemented）。`Unverified` 15 → 13。
  - 写入/破坏性档只出清单与脚本：
    `docs/plan/catalog-snapshots/snowluma-2026-11-05-write-experiments.sh`
    （默认 dry-run，必须 `--execute` + `ACB_TEST_GROUP`/`ACB_TEST_ACCOUNT`），
    覆盖 `group.add_option` / `group.anonymous` / `group.anonymous_ban` /
    `group.tag_add` / `group.tag_delete` / `account.model_show_set` 的
    基线、执行、读回、回滚步骤。**未执行**。
- 取舍：
  - 快照不硬标 SnowLuma 版本：上游没留版本字段/脚本，照实写「版本未知，运行实例
    2.2.1-node 只作参照」比编一个版本号可靠。
  - 只读实验能用有效 tid 确证 `account.qzone.detail`，但 `unlike_qzone` 不能靠
    tid=0 的 ok 确证，保留 `Unverified`，不为了降数量放宽。
  - 写入类一律不执行，脚本默认 dry-run，避免为了实验改真实群/号。
  - 占位/mock 动作单独标注「返回值不代表生效」。
- 证据：
  - `verified_sources_resolve_to_snapshot_or_experiment`（新）
  - `catalog_lists_the_unverified_ops`：13 个（15 → 13）
  - `account.qzone.detail` / `group.tag_list` 的 source 指向
    `docs/plan/catalog-snapshots/snowluma-2026-11-05-readonly-experiments.json#<op>`
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode
    `npm test` 全绿。

## 2026-11-05 — 纠正 `group.tag_list` 判定与「静默成功」清单

- 做了什么：
  - 对 `get_group_tags` 用 `group_class=0/1/2/3/5/10` 六次只读调用做语义对照，
    全部返回 `tags:[]`；无法证明 tags 与 group_class 有对应关系。`group.tag_list`
    从 `Verified` 退回 `Unverified`，reason 写明「契约已确认，语义未确证」。
    `account.qzone.detail` 仍保留 `Verified`（有效 tid 返回 tid/content/
    comment_num/comments）。
  - 文档新增 `Verified` 判定标准：参数语义 + 返回语义都确证才算 `Verified`；
    「能调用、返回形状对」= `Unverified + 契约已确认`。
  - 新增「静默成功」清单 7 条：`account.model_show`、`account.model_show_set`、
    `friend.recent_contacts`、`system.packet_status`、`group.anonymous`、
    `group.anonymous_ban`、`account.qzone.unlike`（tid=0）。明确
    **`retcode=0` 不代表生效**；`msg.word_slices` / `file.collection_create`
    返回 retcode=100，属于明确失败，不在静默成功清单。
  - `Unverified` 13 → 14；覆盖矩阵、adapter-snowluma.md 计数与测试同步。
- 取舍：
  - 不拿「返回形状对」冒充语义确证；宁可回到 `Unverified`，也不让 `Verified`
    失去含义。
  - 静默成功单独建表，不再散落在实验结论里；以后看到 retcode=0 先查这张表。
- 证据：
  - `catalog_lists_the_unverified_ops`：14 个
  - `verified_sources_resolve_to_snapshot_or_experiment`
  - 只读实验记录新增 6 条 group.tag_list 探针（均 `contract_only`）
  - 全量验证：`cargo test --workspace`、clippy `-D warnings`、dsh/opencode
    `npm test` 全绿。

## 部署注意事项（nix-config 打包）

- `acb-notifier` 必须与 `acb-bridge` 安装在同一目录（或显式设置
  `ACB_NOTIFIER_BIN`）。带外通知进程按 `acb-bridge` 同级路径找它。
- `cargo build -p acb-bridge` **不会**构建 `acb-notifier`；需要同时构建
  `acb-notifier-process`（或 `cargo build --workspace`）。`acb-payload` 现已把
  `bin/acb-notifier` 列为必带项，`acb-contract.json` 的 `layout` 会校验它存在；
  `acb-bridge --check` 在 `[notifications]` 启用但找不到 notifier 时直接失败
  （不再静默降级为“桥继续跑、通知丢失”）。
- 其他第二进制/资源（`process-tool-fixture` 等）仍在各自 crate，不需要进
  生产包。
- `RLIMIT_AS` 与 glibc malloc arena：每个外部 provider 一个调用线程（默认
  2 MiB 栈），线程首次分配会触发 glibc 保留 64 MiB 的 per-arena 虚拟地址。
  实测 128 个外部 provider 时线程数与 VmSize 同步上涨到 GB 级（约 +1.77 GB），
  但 RSS 只涨约 1.63 MB，空闲线程约 12–16 KB RSS。这些虚拟地址大多不会被
  commit，也不会进 RSS。
  - **不要给桥设 `RLIMIT_AS`**（或任何按地址空间记账的限制，包括把
    `systemd` 的内存限制配成地址空间口径）。它会按 VmSize 记账，会在真实
    RSS 还很小时就先在这里失败。
  - 资源限制用 cgroup 的 RSS/anon 口径（如 `MemoryMax`），不要用
    `RLIMIT_AS`/`ulimit -v`。

## 待办（用户指定，暂不实现）

- LKG 失败阈值分 class：panic 类失败也许应该 1 次就回滚，不应与 provider 解析
  失败共用 `[lkg] max_failures`。分 class 阈值是后续议题，现在不改行为。
- 外部 `delivery.compose` 输入快照：每轮把候选记录克隆进调用线程。实测默认
  30 条约 21 KB/轮，`delivery_limit = 200` 时约 143 KB/轮。触发条件是配置了
  外部 delivery composer（`namespace != acb`）且启用 provider 超时。当前量级
  可接受；若调大投递上限或记录元素变重，可改为共享只读快照或先传编号集合，
  避免每轮深拷贝全部记录。
- in-process / dynamic 平台载体 `AdapterStatus`：**已收口（2026-11-05）**，见
  「载体级 AdapterStatus 上报收口」；`/api/dependencies` 现在能看到载体 failed/down。
- 控制台尚未展示 `/api/dependencies`：目前只有 API 与 `/api/plugins` 的
  `connections` 字段，运维/管理员需要在页面上看到逐依赖状态与迁移历史。暂不实现。

## 待决（用户拍板前不实现）

### issue 责任归属模型（待用户拍板）

- 问题：GitHub issue 该分给谁——固定给 ACB 开发 agent，还是分给具名的呈现
  身份（如 trae宝），待商榷。
- 已记录的建议口径（尚未采纳）：
  1. issue 只带机器可判定属性 `component` + `severity`，不写人名/agent 名；
  2. `component → 执行体` 是独立映射表，可替换；
  3. 呈现身份只决定对话入口，不参与认领；
  4. 认领需带「下次更新时间」或截止，超时自动退回未认领；
  5. 工单面板是管理员面（本地 console + token），不暴露到对话面；
  6. 现阶段只做「标签路由 + 默认 owner + 每周人工巡未认领」，反馈量起来后再
     考虑自动分派。
- 状态：等用户下班后确认，确认前不实现任何分配逻辑。

## 环境状态

- `acb-bridge.service` 曾因上轮中断变为 inactive；2026-11-04 01:19 已重新启动，
  仍使用 `~/.config/systemd/user/acb-bridge.service.d/worktree.conf` 指向 worktree debug 构建。
- 未提交文件：`code-review-prompt.md`（按用户要求不提交）。
- `docs/09-cordis-mechanism-evaluation.md` 已补提交，作为 Cordis 评估的可评审产物。

## 2026-11-05 — issue 阶段 1：独立服务 + ACB 工具面

- 任务：按 `docs/plan/issue-system.md` 第 6 节实现阶段 1，并补一条首轮主动补投。
- 做了什么：
  - 新增 `crates/issue-service/`：`acb-issue-service`（SQLite WAL + HTTP API + CLI）
    与 `acb-issue-tools`（process-tools stdio provider，8 个工具）。
  - 表：`issues` / `issue_tags` / `work_items` / `issue_events` / `event_deliveries`
    / `subscriptions` / `tokens`；`work_items.issue_id` NOT NULL。
  - 鉴权：SHA-256 token、六种 scope、401/403、90 天过期、7 天轮换重叠、
    service token 禁 `issue:admin`、`on_behalf_of` 只进 actor。
  - 状态机 + `expected_version`、验收者规则、72h 升级、168h 自动关闭。
  - outbox：事件幂等键 + `(event_id, subscriber, channel)` 双唯一键、原子 claim、
    ack、5 分钟回 pending。
  - harness（dsh / opencode）：会话首轮前 internal `issue_my_work`，
    有未读注入一行 `[系统事件 issue_unread]`，注入后立即用同一账本 ack，
    失败静默降级并在下一轮重试。
  - 部署：`deploy/systemd/acb-issue-service.service`、`deploy/nix/issue-service.nix`
    （默认不启用）；文档 `docs/guide/issue-service.md`。
  - 修复：`random_hex` 曾用 `fs::read("/dev/urandom")` 无限读导致测试进程 OOM，
    改为固定 32 字节 `read_exact`。
- 证据：
  - `cargo test -p acb-issue-service` → 8 phase1 + 2 provider_flow 全过。
  - `cargo test -p acb-kernel --test tool_schema_consistency --test issue_tool_surface`
    → 5 + 1 全过。
  - `cargo test --workspace` → 全过（退出码 0）。
  - `cargo clippy --workspace --all-targets -- -D warnings` → 0 warning。
  - opencode `npm test` 52/52；dsh `npm test` 46/46。
  - 真机验收（curl）：空库启动/healthz 200/7 张表；401 + `WWW-Authenticate`；
    403；轮换新旧 token 均 200；409 `stale_version` / `invalid_transition`；
    非验收者 done=403；中文搜索 1 条；游标 6 条不重不漏；
    `kill -9` 后 pending 3 条仍在、重启后 my_work 3 条、ack 后 0；
    注入时钟 72h escalated=3、168h auto_closed=3 + `verification_timeout`。
  - core 工具数断言：`TOOL_NAMES.len() == 29`，8 个 issue 工具不在
    `TOOL_NAMES` / `RESIDENT_TOOLS` / `ON_DEMAND_TOOLS` / harness `TOOLS`。
- 偏离说明：阶段 1 仍不做 webhook 服务端直投与 Wake；第 6 节对应两项标 `[~]`。
- 下一步入口：阶段 2 的 Wake/outbox 游标补投（复用 `event_deliveries`）。

## 2026-11-05 — 授权场所轴拆分 + 审批路由

- 任务：把场所从「房间路径第一段」改成适配器声明的结构事实；审批目标改为配置
  驱动的路由表；群内批准与审计补齐。
- 做了什么：
  - `grants.rs`：`Place { Console, Room(RoomScope), None }`、`RoomScope { Direct,
    MultiParty }`、`Trust { Untrusted, Trusted }`；`Place` 不再和 Trust 合成一个值。
  - `config.rs` 侧新增 `[grants.rooms."<room_id>"]`（`trust` / `members` /
    `approval_route`）与 `[grants.accounts."<connection>:<account>"]`
    （`approval_route`）；解析期校验 room id / 路由目标。
  - 适配器：`WireManifest.direct_room`、`WireRecord.room_scope` / `room_labels`
    进协议；OneBot 适配器按自己的路径声明 Direct/MultiParty；manifest 实例化时
    提供 `direct_room`；缺失/未知一律 MultiParty + Untrusted。
  - `engine.rs`：删除 `place_of` 路径猜测；房间结构存 `room_profiles`；审批路由
    按房间级 > 账号级 > 适配器默认解析，core 不再拼 `private/{account}`；不可达
    记 `route_unavailable` 且不回退申请房间；群内批准要求 trusted + members +
    ticket id；多目标首个批准生效并给其余目标发 superseded；票据/审计落
    `approval_route` / `approval_room` / `approval_place` / `approver` /
    `trusted_group`。
  - `tickets.rs`：票据字段与 `ApprovalMeta`；PENDING_TTL 过期单测。
  - `check_target_params`：按显式 `room_kind` + 完整剩余路径比较，修 `stream.*`
    子路径误判。
- 证据：
  - `cargo test -p acb-kernel` → 全部通过（含新增
    `place_approval_flow` 8 条、`access_ticket_flow` 17 条、engine 子路径单测）。
  - `cargo test --workspace` → 全部通过（退出码 0）。
  - `cargo clippy --workspace --all-targets -- -D warnings` → 0 warning。
  - core 工具数仍 29（`tool_schema_consistency` / `issue_tool_surface`）。
- 偏离/遗留：审批目标必须被某个会话订阅；`approval_route` 已交付到目标时若其它
  目标发送失败，不会做 per-target 重试（只保留 route_unavailable 审计）。
  console 目前没有专门的 route_unavailable 面板，只能从票据查询/日志看。

