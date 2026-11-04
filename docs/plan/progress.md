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

## 待办（用户指定，暂不实现）

- LKG 失败阈值分 class：panic 类失败也许应该 1 次就回滚，不应与 provider 解析
  失败共用 `[lkg] max_failures`。分 class 阈值是后续议题，现在不改行为。

## 环境状态

- `acb-bridge.service` 曾因上轮中断变为 inactive；2026-11-04 01:19 已重新启动，
  仍使用 `~/.config/systemd/user/acb-bridge.service.d/worktree.conf` 指向 worktree debug 构建。
- 未提交文件：`code-review-prompt.md`（按用户要求不提交）。
- `docs/09-cordis-mechanism-evaluation.md` 已补提交，作为 Cordis 评估的可评审产物。
