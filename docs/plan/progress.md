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

## 环境状态

- `acb-bridge.service` 曾因上轮中断变为 inactive；2026-11-04 01:19 已重新启动，
  仍使用 `~/.config/systemd/user/acb-bridge.service.d/worktree.conf` 指向 worktree debug 构建。
- 未提交文件：`code-review-prompt.md`（按用户要求不提交）。
- `docs/09-cordis-mechanism-evaluation.md` 已补提交，作为 Cordis 评估的可评审产物。
