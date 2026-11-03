# Cordis 机制对 ACB 插件体系的评估

状态：**评估稿（只出结论，不改代码/ADR 原文）**。日期：2026-11-04。  
范围：Cordis 的 Rust 实现与核心机制，对照 ACB 的 ADR-0026/0029/0030、T4b/T6 已落地部分。

## 0. 一句话结论

Cordis 最值得借鉴的不是“再引入一个运行时”，而是三件内部机制：**effect/disposable 逆序回退**、
**依赖声明与依赖驱动重载**、**事务式热替换**。ACB 应把它们作为 core 内部的生命周期实现方式吸收，
而不是把 Cordis 的 Context/ServiceRealm/事件总线整体替换掉现有扩展点模型。

## 1. Cordis 的 Rust 实现盘点

> 以下是截至 2026-11-04 检索到的公开 crate/仓库。除特别说明外，版本、下载量和版本数来自
> crates.io 页面；这些数字会变化，README 中的能力描述属于项目自述，本文标注为“自述”。

### 1.1 `cordis-rs`（dshbox）

- 仓库：[github.com/dshbox/cordis-rs](https://github.com/dshbox/cordis-rs)
- crates：`cordis-rs`（应用 facade，0.11.1，80 个版本，约 30,002 次下载）、
  `cordis-core`（语义 core，0.6.1，53 个版本，约 2,355 次下载）、可选
  `cordis-loader` / `cordis-timer`。
- 许可证：MIT。MSRV：Rust 1.88，Edition 2024。发布节奏活跃（0.11.1 约 27 天前）。
- API 形态：**异步、显式类型、机制层优先**。`Context` 是不可变视图；`Plugin::prepare` 同步，
  `apply` 异步；`Context::spawn` 返回 `FiberHandle`；有 `ready/restart/update/era_swap/dispose`。
  `cordis-core` 依赖 `futures` + `parking_lot` + `tokio`（4 features）；README 自述
  “不使用生产 Tokio time-driver”，但并非完全 runtime-free。
- 机制覆盖：Context / Fiber 生命周期、typed Service（`(Service, ServiceRealm)` 精确槽位）、
  typed Event（observer/responder/mapper/around + Scoped/Unscoped routing）、generation-owned
  effects/listeners/tasks/timers、runtime observation。明确声明**不提供隐藏的 Context 层级**，
  Scope 只负责事件可达性，ServiceRealm 只负责服务放置，spawn origin 只是来源记录，不是父子所有权。
- 不覆盖：core 不做 serde/JSON 加载策略、文件监听、动态插件发现；HMR 不在 core；
  这些留给 loader/leaf crates。因此它更像“机制层”，但异步/`tokio` 依赖仍会传导到使用者。
- 成熟度判断：四个实现里**最成熟、下载量最高、版本最多**；仍是 pre-1.0，API freeze 候选已记录，
  1.0 仍需稳定版。适合作为机制语义参考，不适合直接作为 ACB 的 runtime 依赖。

### 1.2 `cordis-rs`（jincdream，crates.io 命名空间 `cordis-rs-*`）

- 仓库：[github.com/jincdream/cordis-rs](https://github.com/jincdream/cordis-rs)
- crates：facade `cordis-rs-framework` 0.1.0；core `cordis-rs-core` 0.1.0；
  loader/group/include 0.1.0；**`cordis-rs-hmr` 0.1.0（336 SLoC，事务式依赖感知热替换）**；
  timer/utils/logger/create。
- 许可证：MIT。MSRV：Rust 1.85。发布约 2 个月；`cordis-rs-group` 约 40 次下载，
  `cordis-rs-loader` 约 93 次下载，`cordis-rs-hmr` 约 13 次下载（截至抓取时）。
- 定位：官方 `cordiverse/cordis` 的**独立 Rust port**，冻结在上游 commit
  `8cc9e33...`；README 自述维护 163 个上游场景的兼容性清单、场景到 Rust 测试映射、
  public export 映射。Rust API 用显式类型/futures 替代 TS 的 Proxy、原型继承、可调用对象、
  decorators 和 `any`。
- API 形态：`Context` + `Plugin` + `Fiber`；`Plugin::new(name, closure)`；
  配置用 `serde_json::Value`；异步。`cordis-rs-hmr` 依赖 `tokio`（6 features）。
- 成熟度判断：**新、下载量低、0.1 版本**，但测试/兼容性投入很重，HMR 是最贴近 ACB 的部分；
  适合作为 HMR 设计参考，不适合作为生产依赖。

### 1.3 `rutis`（arcships / eric8810）

- crates：[rutis 0.19.1](https://crates.io/crates/rutis/0.19.1)（32 个版本，约 2,948 次近期下载，
  12K SLoC）；工作区还包含 `rutis-cordis`、`aimux-llm`、`rutis-dsh`、`rutis-agent`、`rutis-cli`。
- 仓库：crates.io 页面给 `github.com/arcships/rutis`；README 中的链接多为
  `github.com/eric8810/rutis`。**仓库归属/重定向存在不确定，使用前应确认最新地址。**
- 许可证：MIT（README 写明继承自 Cordis/Shigma）。
- API 形态：**Tokio 绑定较深**，依赖 `tokio`（7 features）+ `tokio-util` + `shuttle`；
  异步 `Plugin` trait，`type Deps` 声明依赖，`async fn apply(&self, ctx, deps)`；
  `Ctx` 克隆廉价，`plugin`/`isolate` 返回新 context。
- 机制覆盖：五概念——Plugin 装配单元、Fiber 六态状态机 + 依赖门控 + 级联卸载 +
  恰好一次清理、Service 类型键注册表 + isolate scopes、事件总线五种分发模式、
  **依赖驱动重载（provider 卸载 → 消费者驱逐并自动重载）**。
- 成熟度判断：版本较多、近期活跃，适合参考“依赖驱动重载”和“级联卸载”；但 Tokio 绑定、
  agent/dsh 工作区耦合度高，不适合直接嵌入 ACB。

### 1.4 `ares-cordis`（dirmacs/ares）

- crates：[ares-cordis 0.11.9](https://crates.io/crates/ares-cordis)（14 个版本，约 978 次下载，
  19K SLoC；2024 edition；约 2 天前发布）。
- 仓库：[github.com/dirmacs/ares](https://github.com/dirmacs/ares)。许可证：MIT。
- 定位：ARES 的**叶子 crate**，自述“no other ARES dependencies”，只提供 Cordis primitives：
  `Context`、`Fiber`、`EffectHandle`、`Disposable`、`FiberState`、`UndoMeta`、events、
  loader/module graph。
- API 形态：**Tokio 绑定**；依赖 `tokio`（9 features）、`notify`、`parking_lot`、`serde`/`toml`、
  `tracing`；可选 `inventory`/`libloading`/`linkme`/`rhai`。
- 机制覆盖：公开 `cycles::DependencyGraph` / `find_dependency_cycle`（README 标注
  “Cordis paper §6.5”）、`loader`、`module_graph`、`reload_entries_from_disk`、
  `stamp::FileStamp`、`watcher::SettleBarrier`；effect/fiber/events 也在同一 crate。
  这是四个实现里**对“文件热重载 + 依赖图 + 稳定点”覆盖最直接**的一个，但非常新、与 ARES 生态绑定。
- 成熟度判断：功能密度高、发布很新、下载量小；适合作为 HMR/依赖图的参考实现，不适合直接依赖。

### 1.5 官方 Cordis

- 官方仓库：[github.com/cordiverse/cordis](https://github.com/cordiverse/cordis)，TypeScript，
  MIT，约 9.6k stars，2026-11-02 仍有提交。
- 理论来源：[A Programming Paradigm for Spatiotemporal Composability](https://arxiv.org/abs/2608.25512)
  （arXiv:2608.25512，2026-08-26，Yifan Shi / Wei Zhang / Tianyi Cui；Peking University / DeepSeek-AI）。
- 结论：**官方仓库本身不是 Rust 实现**；检索到的 Rust 实现均为社区/第三方 crate（dshbox、
  jincdream、rutis、ares）。因此本文的“机制”参考官方论文与 TypeScript 语义，
  “Rust 形态”参考上述 port，而不是假设存在官方 Rust runtime。

### 1.6 选型判断

- **不引入任一 crate 作为运行时**：四个 Rust 实现都绑定异步/futures，其中 rutis/ares 深绑 Tokio，
  dshbox 也依赖 tokio（4 features）；ACB 的 core 当前是同步锁模型 + 线程，不是 async runtime。
  直接引入会改变 core 的并发/生命周期模型，超出“借鉴机制”的范围。
- **可以借鉴的成熟度顺序**：dshbox（语义/生命周期）→ jincdream HMR（事务式热替换）→
  rutis（依赖驱动重载/级联卸载）→ ares（文件监听/依赖图/稳定点）。
- **许可证**：以上均为 MIT，许可证层面不构成障碍；但仍需按仓库实际 LICENSE 复核。

## 2. 机制清单（只列可迁移的“机制”，不列 API 细节）

1. **Context / Scope / ServiceRealm**
   - dshbox：`Context` 是不可变视图；`Scope` 是事件可达性；`ServiceRealm` 是服务放置；
     三者独立，明确不提供隐藏层级。
   - rutis：`Ctx` + `isolate` 作用域；Service 是类型键注册表。
   - ares：Context + Fiber + EventsService + Loader/ModuleGraph。
2. **依赖注入 / 依赖解析**
   - dshbox：`InjectSpec` 是生命周期前置条件，不是构造器注入；依赖出现/消失时 Fiber 收敛到
     `Active` / `Pending`。
   - rutis：`type Deps` + 依赖门控；provider 卸载触发消费者驱逐/重载。
   - ares：依赖图与环路检测。
3. **可撤销副作用（effect / disposable）**
   - dshbox：listener/service/task/effect/timer 都归 apply generation 所有；dispose 时回收。
   - rutis：`Ctx` 注册 service/listener/cleanup/child plugin，停止时全部撤回；exactly-once cleanup。
   - ares：`EffectHandle` / `Disposable` / `UndoMeta`。
4. **插件 manifest 与生命周期声明**
   - dshbox：`prepare` → `PreparedPlugin` → `spawn` → `FiberHandle`；`restart` / `update` /
     `era_swap` / `dispose`；`FiberHandle` drop **不等于** dispose，所有权显式。
   - rutis：`Plugin` + `Definition`，同一插件可多次挂载，每次独立 config；六态 fiber。
5. **热重载语义**
   - jincdream：`cordis-rs-hmr` 事务式、依赖感知热替换。
   - ares：`reload_entries_from_disk` + `ModuleGraph` + `DependencyGraph` + `FileStamp` +
     `SettleBarrier`（文件变更 → 稳定点 → 替换）。
   - rutis：provider 卸载 → 消费者驱逐并自动重载。
   - dshbox：loader 只做不可变声明式加载计划，core 不做文件监听/动态发现。
6. **父子 namespace / manager 树**
   - dshbox：**明确拒绝**“一个隐藏 Context 层级同时控制生命周期、服务和事件”；
     spawn origin 只是 provenance，不是 parent/child ownership。
   - rutis：`Ctx::plugin` 可挂 child plugins，cascade unload；这是显式树，不是隐藏树。
   - ACB：`ManagerPlugin::children()` + `manager_order` 是显式树；应保留这个方向。

## 3. 现状差距分析（对照 ACB 代码）

### 3.1 手写清单 / 遗漏风险

**证据：** [plugin.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/plugin.rs#L1454-L1474)

`PluginManager::uninstall_manager` 目前按 map 逐个 `retain`：

- `descriptors`
- `wake` / `access` / `delivery` / `send_hooks` / `turn_errors` / `clocks` / `receives` / `routes`
- `adapter_providers`

风险：

1. 新增一个扩展点（例如把 T7 的 `task` 加进 Root）时，如果忘记在这里加一行，卸载后会留下
   stale provider；`catalog` 仍显示它，`resolve_*` 可能拿到已 `shutdown` 的实例。
2. `manager.shutdown()` 只对当前 namespace 调用一次；`install_manager` 是递归安装整棵子树
   （[plugin.rs L1807-L1823](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/plugin.rs#L1807-L1823)），
   但 `uninstall_manager` **不递归**子 namespace。T5 的 `parent → child` 样例如果卸载 parent，
   child 的 descriptor / manager / `adapter_providers` 会留下。
3. `manager_order` 只 retain 精确 namespace；`shutdown_managers` 的“逆序”只在进程退出路径生效，
   单棵子树卸载没有逆序 shutdown。
4. `uninstall_manager` 的注释自己写明“嵌套 manager 的子 namespace 需要调用方逐层卸载”
   （L1450-L1453），这是一个已知的手写顺序缺口，不是隐藏风险。

**判断：** 这是 Cordis effect/disposable 机制最直接能改善的地方；不是“换个框架重写”，
而是把“注册时登记、卸载时逆序撤销”做成 core 的内部约束。

### 3.2 隐式 / 无法声明的依赖关系

**证据：** [plugin.rs L1056-L1095](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/plugin.rs#L1056-L1095)
的 `ManagerPlugin` 只有 `manifest/descriptors/instantiate/children/reload/shutdown/health/details/root`；
没有 `requires` / `provides`。`ProviderDescriptor` 有 `capabilities`，但没有“我依赖哪个 provider”。

**证据：** [engine.rs L486-L545](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/engine.rs#L486-L545)
的 `plugin_references` 是手写扫描：

- `connection.receive.provider`
- `connection.routes[].provider`
- `session.access` / `delivery` / `outbound.before/after` / `turn_error`
- `subscription.wake` / `clock`

风险：

1. 新增配置字段（例如 T7 的 `task` 配置、T8 的反馈通知 provider）时，如果忘记加进
   `plugin_references`，`queue_unload` 会误以为“无引用”而允许卸载，下一轮解析配置失败。
2. 这个函数只回答“当前 config 引用了它”，不回答“哪个模块依赖它”。T6 的动态模块
   （[extensions/src/lib.rs L69-L77](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/extensions/src/lib.rs#L69-L77)）
   有 `capabilities` / `effects`，但**没有 `requires`**；卸载一个被另一个模块使用的模块，
   当前不会检测。
3. T6 的 route 模块按目录名排序执行
   （[extensions/src/lib.rs L495](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/extensions/src/lib.rs#L495)），
   没有依赖拓扑排序；未来模块需要“先 wake 策略、后 route 动作”时只能靠命名约定。

**判断：** 这是 Cordis dependency graph / `InjectSpec` 最值得吸收的部分；但也需要承认：
ACB 的 `config.toml` 是 composition root，很多依赖天然是配置表达，不能全部搬进插件 manifest。

### 3.3 已被轮次边界/安全点覆盖的语义

**证据：** [engine.rs L941-L1005](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/engine.rs#L941-L1005)

`apply_pending_plugin_actions` 在 `active.is_empty()` 时处理：

- `pending_unloads` → `unload_provider`
- `pending_reloads` → `reload_provider`
- `pending_manager_reloads` → `reload_managers`
- 失败写 `plugin_errors` / `manager_errors`

这与 ADR-0029 D4 / ADR-0030 D3 的“安全点”是同一语义。Cordis effect stack 不会改变
“什么时候卸载”，只会改变“卸载时按什么顺序撤销、如何记录”。因此这是**形式不同，
不是机制被 ADR 挡住**；可以吸收 effect 栈，但必须仍然在轮次边界调用。

**证据：** [extensions/src/lib.rs L301-L361](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/extensions/src/lib.rs#L301-L361)
的 `DynamicRouteManager::reload` 已经是“扫描 → 校验 → 有错不 swap → 无错整体替换”；
这与 Cordis HMR 的“事务式替换”方向一致，ACB 已经实现了声明式模块的候选校验/不替换。

**证据：** [adapters/dynamic/src/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/adapters/dynamic/src/lib.rs)
的 `DynamicLoader::unload/reload` 是“先 unload，再 reload”，不是“先准备候选，再替换”。
这对应 ADR-0030 D3 的“回滚到上一份已加载实现”在 native 同路径下**并未真正满足**：
`dlclose` 后旧库已经不可恢复。Cordis 的 effect/HMR 也不能解决这一点；它只能帮助清理顺序，
不能把 `dlclose` 变成可逆操作。

## 4. 冲突处理原则与 ADR 修订提案

### 4.1 先写清不变量要防什么

- **轮次边界卸载**要防的具体失败：一轮正在执行时，provider 实例/函数指针被释放；
  下一轮开始时配置引用的 provider 已经不存在；动态 namespace 只替换了一半，出现新旧混合。
- **失败不替换线上版本**要防的具体失败：新的 manifest/`.so` 校验失败，旧插件已经被移除，
  线上从“能用”变成“不可用”。
- **UNLOAD_SAFE**要防的具体失败：`dlclose` 后插件线程/TLS/全局析构/回调继续访问 core，
  导致随机崩溃；C ABI 本身不构成安全保证（ADR-0030 D4）。

### 4.2 判断：哪些是“机制被挡住”，哪些只是“形式不同”

| 机制 | 与 ACB 不变量关系 | 判断 |
|------|------------------|------|
| effect/disposable 逆序回退 | 不改变何时卸载；只改变卸载内部顺序与遗漏风险 | **形式不同，可吸收** |
| 依赖图 / 依赖驱动重载 | 不改变轮次边界；补足“谁依赖谁”的声明与消费者驱逐 | **可吸收，但需 ADR 增补** |
| 事务式 HMR | T6 声明式模块已做到“坏候选不 swap”；native 同路径无法保留旧库 | **声明式可吸收；native 需单独方案** |
| Context/ServiceRealm/typed event bus | 会替换 ACB 的扩展点/ProviderId/C ABI 组合根 | **不整体采纳**；只借“独立 scope”原则 |
| 父子 Context 隐藏层级 | ACB 已有显式 manager 树；Cordis 自己也拒绝隐藏层级 | **保留 ACB 显式树，不引入 Cordis 隐藏树** |

### 4.3 ADR 修订提案

#### 提案 A：ADR-0029 D4 增加“依赖图与消费者驱逐”

- 改哪一条：D4「热重载在轮次边界完成，校验失败不替换线上版本」。
- 新表述（建议追加，不替换原文）：
  > 动态模块可在 manifest 声明 `requires` / `provides`。重载时先校验依赖图：重复 provider、
  > 缺失依赖、环路均拒绝，且不替换线上快照。依赖图按拓扑序执行模块；被替换 provider 的
  > 消费者在同一轮次边界被驱逐并按新图重载。没有声明依赖的模块保持目录名稳定顺序。
- 影响：`crates/extensions` manifest schema、`scan_dir`/`validate`/`reload`；
  `docs/08-plugin-development.md`；T6 测试。兼容性：新增可选字段，旧 manifest 不写
  `requires` 时行为不变。
- 迁移代价：小；但需要新增依赖图测试（缺失依赖、环路、消费者驱逐）。

#### 提案 B：ADR-0029 D8 增加“依赖边与 pending 原因”

- 改哪一条：D8「每个模块至少暴露 … 引用关系 …」。
- 新表述：在现有 `refs` 之外，增加 `requires` / `provides` / `dependents` 与
  `pending` 原因（例如“等待 provider X”）。`GET /api/extensions` 与 `extensions_list`
  同步返回。
- 影响：`crates/extensions` `details`、`engine.rs` `extension_catalog`、工具 schema。
- 兼容性：纯增量字段；旧客户端忽略即可。

#### 提案 C：ADR-0030 D3 明确 native 同路径 reload 的限制（或补候选预加载）

当前实现与 ADR-0030 D3 的“回滚到上一份已加载实现（如果仍在内存中）”存在差距：

- 现状：`DynamicLoader::unload` 先丢弃 provider/manager，再 `drop(library)` 触发 `dlclose`；
  后续 `reload` 如果失败，旧版本已经不在内存中（[adapters/dynamic/src/lib.rs](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/adapters/dynamic/src/lib.rs)）。
- 两种修订方向：
  1. **严格方案（推荐优先评估）**：在安全点前先把新 `.so` 以唯一临时路径/独立命名空间
     预加载，调用 manifest/create/invoke/destroy 做探针校验；通过后再销毁旧实例、替换。
     需要 `UNLOAD_SAFE`、无全局符号冲突；必要时用 `dlmopen` 或 process carrier 做强隔离。
  2. **务实方案**：修订 D3，明确“同路径 native reload 是不可逆的；失败后插件保持
     `unloaded`，通过 `lastError` 暴露；旧版本恢复仅在候选预加载实现后提供。”
- 影响：`crates/adapters/dynamic`、T4c 测试、控制台 `lastError` 文案。
- 迁移代价：严格方案中高（库加载/符号隔离/测试）；务实方案零代码，但弱化 ADR 承诺。
- **Cordis effect/HMR 不能替代这个决定**：effect 栈能保证清理顺序，不能让 `dlclose`
  回滚；事务式 HMR 对进程内 Rust 对象有效，对 C ABI 库边界无效。

#### 提案 D：ADR-0029 D3 / D9 的 scope 与 trust 不随 Cordis 改变

Cordis 的 ServiceRealm/Scope 是机制层的隔离，不等于 ACB 的“信任层级/审批”。
T6 的 `scope = global/connection/session` 已经表达部署范围；authoring 仍应保留
`admin_qqs` + 控制台 token + 显式 reload，不因为引入 effect 机制而放宽。

## 5. 结论矩阵

| 机制 | 现状差距（证据） | 建议 | 落地位置与最小改动 | 冲突 / ADR 修订 |
|------|------------------|------|--------------------|------------------|
| effect / disposable 逆序回退 | `uninstall_manager` 手写 retain 列表；嵌套 manager 不递归；`shutdown` 只当前层（[plugin.rs L1454-L1474](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/plugin.rs#L1454-L1474)） | **吸收（内部）** | 在 `PluginManager` 内部维护注册 effect 栈：每次 `register_*`/`install_manager` 返回 guard，卸载按逆序撤销；不暴露给 C ABI | 无直接冲突；ADR-0026/0029 可补一句“teardown 由 effect 栈驱动”；native `dlclose` 仍在栈外 |
| 依赖声明 / DI | `ManagerPlugin` 无 `requires`；`plugin_references` 手写扫描 config；T6 模块无 `requires`（[engine.rs L486-L545](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/kernel/src/engine.rs#L486-L545)、[extensions L69-L77](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/extensions/src/lib.rs#L69-L77)） | **部分吸收** | `crates/extensions` manifest 增加可选 `requires`/`provides`；`scan_dir` 构建依赖图，拓扑排序 + 环路拒绝；`engine.rs` 的 refs 改为调用集中式 `Config::provider_references()` | 提案 A/B；旧 manifest 兼容 |
| 依赖驱动重载 / 消费者驱逐 | T6 只有整快照 swap，没有“provider 变了 → 消费者重载”；T5 父子树无卸载递归 | **吸收概念** | 在 `DynamicRouteManager::reload` 成功后，对依赖变更的模块重建/驱逐；`PluginManager` 的 manager reload 仍由 `Bridge` 安全点触发 | 提案 A；需要依赖图测试 |
| 事务式 HMR（声明式） | T6 已实现“坏候选不 swap”（[extensions L301-L361](file:///home/nixos/Documents/code/qq-bridge-fork/.agents/worktrees/qqab-arch/crates/extensions/src/lib.rs#L301-L361)） | **已覆盖，继续强化** | 增加依赖图校验与消费者驱逐；保持整快照 swap | 无冲突；ADR-0029 D4 已支持 |
| 事务式 HMR（native） | T4c 先 unload 再 reload；失败留在 unloaded，非“保留旧版本” | **不采纳 effect 式回滚；另案** | 先做候选预加载探针（严格方案），或修订 ADR-0030 D3 明确限制（务实方案） | 提案 C；Cordis 不能解决 `dlclose` |
| Context / ServiceRealm / typed event bus | ACB 用 `ProviderId` + 扩展点 + 各 `HashMap`，不是 typed service registry；C ABI/JSON 是跨边界 | **不整体采纳** | 保留现有扩展点；只借“独立 scope/placement”原则；T6 已有 `scope` | 强行替换会破坏 ADR-0026/0027/0030；不建议 |
| 插件 `prepare/spawn/update/era_swap/dispose` 生命周期词汇 | ACB 已有 install / queue unload / queue reload / manager reload / shutdown；缺少 Pending 依赖状态 | **部分吸收** | 统一文档词汇：install≈prepare+spawn，`queue_reload`≈era_swap，`queue_extension_reload`≈update（identity 保留）；如需 Pending，先在动态模块 health/details 里表达 | 不改代码也可先统一文档；改状态机需新 ADR 段落 |
| 父子 namespace / manager 树 | ACB 显式 `children()` + `manager_order`；`uninstall_manager` 不递归 | **保留显式树；修递归卸载** | `uninstall_manager` 先收集子树、逆序 shutdown、统一 effect 撤销；T5 样例补卸载测试 | 与 Cordis “拒绝隐藏层级”一致；无 ADR 冲突 |
| 通用 typed event bus / 五种分发 | ACB 的 wake/access/delivery/send/turn_error/clock/receive/route 已是显式扩展点 | **不采纳为替代** | 如需要，只为生命周期审计增加内部事件（unloaded/reloaded/reload_failed），不替换策略接口 | 大改会破坏现有 C ABI/JSON wire；不建议 |
| runtime observation / 依赖边 | ACB 已有 catalog/health/lastError/refs；缺 `dependents`/pending 原因 | **吸收概念** | `extensions::details` 增加 `requires/provides/dependents`；`engine.extension_catalog` 透传 | 提案 B；纯增量 |
| 配置 loader / 声明式计划 | ACB `config.toml` + `ACB_DYNAMIC_DIR`；T6 已校验/暂存/边界替换 | **部分吸收** | 只吸收“候选校验/不可变计划/稳定点”；不引入 `cordis-loader` | 无冲突；现有配置管理 ADR-0009 仍优先 |

## 6. 对高级用户写插件的门槛影响

### 6.1 native / Rust / C ABI

- **如果 effect 栈只在 core 内部使用**：C 插件作者不受影响；Rust native manager 作者
  不必手写 `uninstall_manager` 的清单，生命周期更安全。门槛**略降**。
- **如果把 effect/disposable 暴露到 C ABI**：需要在 `create/invoke/destroy` 之外增加
  “注册/撤销 effect”的协议或回调表；C 插件作者要理解逆序、所有权、错误路径。门槛**上升**。
- **如果把 Cordis `Plugin`/`Context` 作为 Rust native SDK**：Rust 作者写生命周期更简单，
  但需要接受 async/futures、`FiberHandle`、scope/isolate 概念；对不熟悉 async 的 native
  作者反而更难。建议只提供**同步的 Rust manager 包装层**，不把 Cordis runtime 语义强加给 C ABI。
- `UNLOAD_SAFE` 的线程/TLS/全局状态约束不会因为 Cordis 机制而消失；native 高级用户的
  真正门槛仍是 ADR-0030 D4 的契约与 conformance 测试。

### 6.2 Lua / 声明式 / JS（process）

- **声明式 route**：如果只增加可选 `requires`/`provides`，门槛只增加几行 manifest；
  依赖缺失/环路/坏模块的错误在 `extensions_list` 可见，调试更容易。总体**变易**。
- **Lua**：脚本本身不需要实现 effect 栈；core 负责注册/撤销。若把依赖声明放进 manifest，
  Lua 作者需要维护模块清单，但热重载更安全。总体**略变易**（前提是 manifest 工具能生成/校验）。
- **JS/process**：effect 语义跨进程需要协议层的 dispose/cleanup 消息；如果强制，门槛**上升**。
  当前 process 载体应保持“进程隔离 + 进程级 teardown”，不引入进程内 effect 栈。
- **工具面**：`extensions_list/read/write/reload` 已让 AI 能自查；再加入 `dependents` 与
  `pending reason` 会进一步提升可观察性，但不会降低 native 的 ABI 门槛。

## 7. 最终建议（按优先级）

1. **先吸收 effect/disposable 作为 core 内部机制**：修 `uninstall_manager` 的手写清单与
   嵌套 manager 递归卸载；这是低风险、高收益，不改变 ADR 不变量。
2. **再吸收依赖图**：给 T6 manifest 增加可选 `requires`/`provides`，做拓扑排序、环路拒绝、
   消费者驱逐；这是 Cordis 对 ACB 最有价值的部分，且与轮次边界不冲突。
3. **对 native 事务式热替换单独决策**：要么做候选预加载探针，要么修订 ADR-0030 D3 明确
   “同路径 reload 不可逆”；不要把 Cordis HMR 当成 `dlclose` 的解决方案。
4. **不整体引入 Cordis Context/ServiceRealm/typed event bus**：ACB 的扩展点 + ProviderId +
   C ABI 是更适合当前载体的组合；强行替换会破坏 ADR-0026/0027/0030 的兼容性。
5. **保留显式 manager 树**：Cordis 自己也拒绝隐藏层级；ACB 应继续用 `children()` 显式树，
   但补递归卸载与逆序 shutdown。

## 参考来源

- [cordis-rs (dshbox)](https://crates.io/crates/cordis-rs) / [cordis-core](https://crates.io/crates/cordis-core) / [GitHub](https://github.com/dshbox/cordis-rs)
- [cordis-rs-loader](https://crates.io/crates/cordis-rs-loader)
- [cordis-rs (jincdream)](https://github.com/jincdream/cordis-rs) / [cordis-rs-hmr](https://crates.io/crates/cordis-rs-hmr) / [cordis-rs-group](https://crates.io/crates/cordis-rs-group)
- [rutis 0.19.1](https://crates.io/crates/rutis/0.19.1) / [rutis README](https://github.com/arcships/rutis) / [eric8810/rutis](https://github.com/eric8810/rutis)
- [ares-cordis](https://crates.io/crates/ares-cordis) / [dirmacs/ares](https://github.com/dirmacs/ares)
- [cordiverse/cordis](https://github.com/cordiverse/cordis)
- [arXiv:2608.25512](https://arxiv.org/abs/2608.25512)

## 不确定项

- crates.io 下载量/版本数/发布时间是抓取快照，会随时间变化；README 中的成熟度/兼容性声明属于项目自述。
- `rutis` 的仓库归属存在 `arcships/rutis` 与 `eric8810/rutis` 两种链接，使用前应确认权威仓库。
- `cordis-core` 依赖 tokio（4 features）；README 自述不使用生产 Tokio time-driver，
  但“是否完全 runtime-agnostic”需要按具体 feature 复核，本文不把它当作纯同步机制。
- 各实现的 HMR/effect 语义细节需要读对应 ADR/源码；本文只把公开 README/文档作为证据，
  未逐行审计实现。
