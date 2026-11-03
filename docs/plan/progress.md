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

## 环境状态

- `acb-bridge.service` 曾因上轮中断变为 inactive；2026-11-04 01:19 已重新启动，
  仍使用 `~/.config/systemd/user/acb-bridge.service.d/worktree.conf` 指向 worktree debug 构建。
- 未提交文件：`code-review-prompt.md`（按用户要求不提交）、
  `docs/09-cordis-mechanism-evaluation.md`（上一轮评估稿，按“不提交”要求保留在工作区）。
