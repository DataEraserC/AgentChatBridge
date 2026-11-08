# plugin-api 设计稿

状态：**设计（2026-11-07）**。只出文档，不改代码、不提交、不重启。

这份文档定义 `plugin-api` 作为插件语义的唯一权威来源，以及它到三条载体
（内嵌 native、动态库、Lua）和动态库两条同级 ABI 通道（C ABI、`abi_stable`）
的投影方式。它同时记录当前代码的真实基线、必须修的缺口和分阶段迁移路径。

相关文档：

- [ADR-0026 插件宿主](../adr/0026-plugin-host.md)
- [ADR-0029 动态扩展](../adr/0029-dynamic-extensions.md)
- [ADR-0030 unload-safe native](../adr/0030-unload-safe-native-plugin.md)
- [扩展点图谱](extension-point-catalog.md)
- [WASM 与 `.so` 参考实现](wasm-carriers.md)

## 0. 结论摘要

1. **语义只有一份**：`crates/plugin-api`（包名 `acb-plugin-api`）持有扩展点、能力、
   manifest、请求/结果/错误类型、失败策略和一致性语料。kernel、C ABI、`abi_stable`、
   Lua 都只是它的消费者或投影，不得各自定义默认行为。
2. **两条 ABI 通道同级**：动态库插件在自己的 manifest 里声明 `abi = "c"` 或
   `abi = "abi_stable"`，core 按声明分派。core 没有全局偏好、没有自动探测、
   没有「生态优先」分支。脚手架和自有插件默认用 `abi_stable`，只是生态约定。
3. **8 个扩展点三载体全支持，不允许缺项**：wake / access / delivery / send / turn_error /
   clock / receive / route。native（`NativeLoader` + `NativeProviderLoader`）、动态库
   （C ABI 与 `abi_stable`）、Lua 的目标能力都是 8/8；任一载体少一个点都算未完成。
4. **当前的主要缺口是声明与注册不一致、两个载体有缺项**：`NativeLoader` 声明了 Route
   却没注册、Send 既没声明也没注册；`DynamicLoader` 缺 Route 的语义、wire、分派三层。
   处理方式是补齐到 8/8，而不是删声明或长期保留“暂不支持”。
5. **`.so` route 必须补齐，不是可选项**：语义层的 `RouteContext` 序列化、C ABI 的请求/
   结果类型、`Pass` / `Drop` / `Handle` 的等价表达、四个动作（`Wake` / `Send` /
   `Interrupt` / `Task`）都要有；只补一层不够。
6. **归因、交叉校验、超时/隔离与能力对齐同批交付**：它们是配套，不是准入否决条件，
   也不能因为“先对齐”被跳过。每一批交付都要同时满足三载体 8/8 和这三项。
7. **迁移按三载体同时对齐重排**：每个发布批次结束时三载体必须都是 8/8；批次内部允许
   暂时不一致，但必须标注为中间态且不得发布。

## 1. 范围与既定前提

### 1.1 三条载体

| 载体 | 形态 | 装载方式 | 迁移起点 / 目标 |
|---|---|---|---|
| 内嵌 native | 编进 `acb-bridge` 的 Rust 实现 | `NativeLoader` / `ToolLoader` / `NativeProviderLoader` 直接注册 | 迁移起点 6/8（缺 send、route）；目标 8/8 |
| 动态库 | `.so` / cdylib | `DynamicLoader` 用 `dlopen` 装入 | 迁移起点 7/8（缺 route）+ 平台适配器；目标 8/8 |
| Lua | `ACB_LUA_DIR` 下的 `*.lua` | `LuaLoader` 每个脚本注册一个 provider | 8/8 |

表中的“迁移起点”是事实基线，不是允许长期保留的差异；三者目标都是 8/8，矩阵见 3.4。

### 1.2 两条同级 ABI 通道

动态库插件在自己的 manifest 里声明使用哪条通道：

- `abi = "c"`：现有 `CoreMessage` / `WireResult` / `Wire*Request` 那套 JSON + 符号表。
- `abi = "abi_stable"`：`abi_stable` 的 `StableAbi` 类型 + `RootModule`。

规则：

- core 按插件声明分派，**不探测、不回退、不比较优劣**。
- 两条通道都走同一个 `DynamicLoader` 生命周期和同一个 kernel trait 面。
- `abi_stable` 优先只体现在脚手架模板、推荐路径和自有插件；core 不为它开特例，
  也不因为插件用 C ABI 就区别对待或打印警告。
- 同一份语义在两个通道上必须跑出一致结果，由第 7 节的一致性测试锁定。

### 1.3 单一权威语义

`plugin-api` 定义“一个决策是什么意思”，两条通道只定义“这些字段怎么摆”。
投影层不得自带默认值、兜底策略、字段改名规则或能力判断。所有默认行为都从
`plugin-api` 的同一组函数导出。

### 1.4 8 个扩展点

本设计只覆盖这 8 个策略扩展点：

```text
wake  access  delivery  send  turn_error  clock  receive  route
```

`ExtensionPoint` 枚举里还有 `ToolAccess`、`Observer`、`Tool`、`Task`、`Prompt`、
`PlatformAdapter`、`Manager`、`Loader`。它们不在本次 `plugin-api` 契约里，保持现状，
后续按各自载体单独设计。

### 1.5 route 是高权点

route 能代桥发消息、打断轮次、起后台任务、直接请求唤醒。插件本身可信，因此不设信任
分级或能力授权准入。三载体同时开放：native 补齐 Send/Route 注册，Lua 已是 8/8，
`.so` 必须补 route。与能力对齐同批交付的配套是：

1. 动作留痕：每个决定和动作进入归因流水，用于事后追溯与回滚定位；
2. 调用超时与隔离：防止插件卡死拖垮桥的可用性；
3. 启动期交叉校验：manifest 声明与实际注册严格一致，不一致就启动失败。

manifest 的能力声明仍然保留，但只用于启动期核对，不作为运行期授权依据。

### 1.6 非目标

- 不在本设计里引入 WASM；WASM 走独立进程载体，见 [wasm-carriers.md](wasm-carriers.md)。
- 不支持一个 `.so` 文件声明多个 provider；当前模型仍是一个产物一个 provider。
- 不支持运行时在两条 ABI 通道之间切换或自动回退。
- 不把 `abi_stable` 写成 core 的全局偏好。
- 不新增第 9 个策略扩展点。

## 2. 现状基线（代码事实）

### 2.1 语义类型今天在 kernel 里

| 内容 | 位置 |
|---|---|
| `ExtensionPoint` / `Capability` / `ProviderId` | [plugin.rs](../../crates/kernel/src/plugin.rs) |
| `ProviderDescriptor` / `LoaderManifest` / `ManagerManifest` | 同上 |
| 8 个点的请求/结果/trait | 同上 |
| `PluginError` | 同上 |
| `PluginSource` / `PluginEffect` / `PluginMetadata` | [plugin_meta.rs](../../crates/kernel/src/plugin_meta.rs) |
| 策略配置形状 | [config.rs](../../crates/kernel/src/config.rs) |
| wire 类型 | [adapter-api/src/lib.rs](../../crates/adapter-api/src/lib.rs) |

这带来两个问题：语义和宿主实现绑死；`adapter-api` 无法只依赖语义，只能被 kernel
反向依赖或复制字段。

### 2.2 三个载体的迁移前支持面（目标均为 8/8）

| 扩展点 | `NativeLoader`（内置） | `DynamicLoader`（C ABI） | `LuaLoader` |
|---|---|---|---|
| wake | 注册 | 支持 | 支持 |
| access | 注册 | 支持 | 支持 |
| delivery | 注册 | 支持 | 支持 |
| send | 未注册（必须补齐） | 支持 | 支持 |
| turn_error | 注册 | 支持 | 支持 |
| clock | 注册（off + interval_idle） | 支持 | 支持 |
| receive | 注册 | 支持 | 支持 |
| route | 已声明但未注册（必须补齐） | 未实现（必须补齐） | 支持 |

事实来源：

- [`NativeLoader::manifest`](../../crates/kernel/src/plugin.rs#L1474-L1488) 声明
  Wake、Access、Delivery、TurnError、Clock、Receive、Route；
  [`install`](../../crates/kernel/src/plugin.rs#L1490-L1585) 只注册 wake、access、
  delivery、turn_error、clock（两个）、receive。迁移目标：manifest 与注册同时补到
  Send + Route，最终 8 声明 / 8 注册。
- [`DynamicLoader::manifest`](../../crates/adapters/dynamic/src/lib.rs#L288-L304) 声明
  PlatformAdapter + 7/8（缺 Route）；迁移目标：加 Route，落到 8/8。
- [`LoadedPlugin::load`](../../crates/adapters/dynamic/src/lib.rs#L608-L700) 没有
  `"route"` 分支；迁移目标：补 `route` 分支和 `DylibRouteManager`。
- [`LuaLoader::manifest`](../../crates/plugins/lua/src/lib.rs#L611-L626) 声明全部 8 个点，
  已是 8/8。

### 2.3 已经在做对的部分

- kernel trait 面已经覆盖 8 个点，`ProviderInstance`、`resolve_*`、`register_*` 都在。
- 外部 provider 调用已经有按 provider 的专用执行线程、单飞（busy）、超时标记 stuck、
  多代 stuck 标记 dead 的熔断，见
  [`call_timed`](../../crates/kernel/src/plugin.rs#L1786-L1851)。
- 每个 provider 的 descriptor 已经带 `source`、`capabilities`、`metadata`，能力审计
  的骨架存在。
- manager 安装时已经校验 manager manifest 的能力并集，见
  [`collect_manager`](../../crates/kernel/src/plugin.rs#L3044-L3220)。
- 直接唤醒当前还有来源/能力检查、频率限制和审计，见
  [`apply_direct_wake`](../../crates/kernel/src/engine.rs#L2967-L3012)。
  按“插件可信”的新前提，来源/能力检查应退化为声明核对，运行期只保留订阅检查、
  频率限制和审计。
- route 调用已经按连接配置顺序执行，外部 provider 走 `call_timed`，错误和 panic 都按
  `Pass` 处理，见 [engine.rs](../../crates/kernel/src/engine.rs#L2134-L2293)。

### 2.4 五个具体缺口

**缺口 1：loader 声明不参与注册校验。**

[`install_loader`](../../crates/kernel/src/plugin.rs#L2049-L2089) 只检查 API 版本、
扩展点非空、扩展点是已知名字，然后直接 `loader.install(self)`。它没有检查：

- loader 实际注册的 descriptor 的扩展点是否在 `manifest.extensions` 里；
- descriptor 的能力是否在 loader 声明的能力里；
- loader 声明的点是否真有 core 的分派路径。

[`check_descriptor`](../../crates/kernel/src/plugin.rs#L3580-L3606) 也只校验
“期望点、API 版本、必需能力”，不知道 loader manifest 长什么样。

**缺口 2：`NativeLoader` 的声明与实际不一致。**

声明了 Route，但没有 `register_route`；没有声明 Send，也没有 `register_send_hook`。
今天不会报错，是因为缺口 1。处理方式是**补齐注册**，不是删声明：`SendHook` trait 和
`register_send_hook` 已存在，`RouteProvider` trait 和 `register_route` 已存在，
`PassThroughRouteProvider` 也已写好；把两者注册进 `install`，让 manifest 的 8 个声明与
8 个注册严格一致。

**缺口 3：`.so` 的 route 在语义、wire、分派三层都还没有，且必须补齐。**

- manifest 不含 Route；
- `LoadedPlugin::load` 没有 route 分支；
- [`WireResult`](../../crates/adapter-api/src/lib.rs#L512-L554) 只有
  `Decision` / `Delivery` / `Actions` / `Elements` 等，没有路由决定和动作；
  也没有对 `RouteContext` 的序列化形状、`WireRouteDecision`、`WireRouteAction`、
  `WireInterruptScope`。

需要覆盖：`Pass` / `Drop` / `Handle { store, wake, actions }` 的等价表达，以及
`Wake` / `Send` / `Interrupt` / `Task` 四个动作。`.so` route 是本设计的必交付项，
不是“暂不支持”或“后续再说”。

**缺口 4：`WireManifest` 表达不了通道和契约版本。**

[`WireManifest`](../../crates/adapter-api/src/lib.rs#L104-L139) 只有一个
`extension: String`，没有 `abi`、没有 `abi_version`、没有 `contract_version`。
core 无法按声明分派，也无法在调用前拒绝版本不匹配的库。

**缺口 5：route 没有统一归因。**

目标不是授权，也不是限流，而是事后回答四个问题：哪个 provider、什么动作、打到哪个
目标、结果如何（成功 / 被拒 / 失败 / 超时）。当前状态：

| 对象 | 现状 | 代码 |
|---|---|---|
| 决策 | 只有错误 / panic 日志，没有一条决策归因记录 | [engine.rs](../../crates/kernel/src/engine.rs#L2143-L2247) |
| `Wake` | 有 `WakeAuditEntry`，拒绝与接受都记；但只在进程内环形保留 100 条，重启清空 | [engine.rs](../../crates/kernel/src/engine.rs#L487-L496)、[engine.rs](../../crates/kernel/src/engine.rs#L3130-L3150) |
| `Send` | 成功才写出站流水（带记录 ID / 平台 ID），但没有归因；失败只有日志；无订阅者时直接返回 `Ok` | [engine.rs](../../crates/kernel/src/engine.rs#L2902-L2933)、[engine.rs](../../crates/kernel/src/engine.rs#L7834-L7920) |
| `Interrupt` | 无任何前置检查，只有一行 `route_interrupt` 日志；无记录、无审计 | [engine.rs](../../crates/kernel/src/engine.rs#L3253-L3304) |
| `Task` | 只有内存 `TaskRecord`（`TASK_KEEP=200`），重启清空；无归因 | [engine.rs](../../crates/kernel/src/engine.rs#L1272-L1307) |

注册期要求 `RouteDecision`，运行期没有逐动作能力校验。按本设计的新前提，插件可信，
逐动作能力校验不作为准入要求；这里要补的是**归因覆盖**与**超时/隔离**，不是授权。

## 3. 权威语义模型

### 3.1 分层与依赖方向

```text
acb-proto                    基础数据（Record / Element / RoomId / Sender）
    ↑
plugin-api                   权威语义：扩展点、能力、manifest、请求/结果/错误、
    ↑                        失败策略、默认实现、一致性语料
    ├── kernel               宿主：注册表、解析、分派、引擎集成
    ├── adapter-api          C ABI 投影：Wire* 类型 + JSON schema
    ├── plugin-abi-stable    abi_stable 投影：StableAbi 类型 + RootModule
    └── lua                  Lua 投影：JSON 形状 + 脚本侧约定
```

依赖规则：

- `plugin-api` 只依赖 `acb-proto`，不依赖 kernel、adapter、任何运行时。
- kernel 依赖 `plugin-api`，并在过渡期用 `pub use` 保留 `crate::plugin::*` 路径。
- `adapter-api` 依赖 `plugin-api`，Wire 类型负责与语义类型互转。
- `plugin-abi-stable` 依赖 `plugin-api` + `abi_stable`，不依赖 kernel。
- 两条投影都不得反向依赖 kernel；否则通道会选择宿主实现，语义又会分裂。

### 3.2 公共约定

#### 3.2.1 ID、扩展点、能力

- `ProviderId` 仍是 `namespace.name`，namespace 不允许含点，内置保留 `acb`。
- 扩展点字符串是稳定标识：`wake` / `access` / `delivery` / `send` / `turn_error` /
  `clock` / `receive` / `route`，不随 Rust 枚举改名。
- 能力是“这个 provider 声明自己提供什么”的标签，不是运行期授权票据。安装期校验
  只核对声明与实际注册一致：descriptor 的能力必须是该扩展点必需能力的超集，
  且是 loader 声明能力的子集；不一致就启动失败。
- 能力字符串稳定，新增能力只能追加；删除或改名视为破坏性变更。

#### 3.2.2 错误分类

`plugin-api` 定义唯一一套故障分类：

| 分类 | 含义 | 典型来源 |
|---|---|---|
| `Unsupported` | 这个 provider 不支持这次调用 | 旧插件、能力协商失败 |
| `InvalidInput` | 输入不合法（字段缺失、超限） | 投影解码失败、动作超限 |
| `Timeout` | 超过调用预算 | `call_timed` |
| `Busy` | 同一 provider 已有在途调用 | 单飞执行器 |
| `Dead` | provider 连续超时，已被熔断 | 执行器 |
| `Panic` | 插件 panic | `catch_unwind` |
| `Rejected` | 策略主动拒绝，带原因 | 插件返回错误 |
| `Internal` | 宿主或插件内部错误 | 兜底 |

投影层必须把底层错误映射到这张表，不允许把“字符串错误”直接透传给引擎做判断。
未知错误码一律归 `Internal`。

#### 3.2.3 失败策略

失败策略集中在 `plugin-api` 的 `fail_policy(point, fault)`，两条通道共用：

| 扩展点 | 失败/超时/panic 时的行为 | 理由 |
|---|---|---|
| wake | `Stay` | 通知策略失败不应造成误唤醒 |
| access | `Deny` | 失败关闭 |
| delivery | 内置默认选批 `default_delivery_plan` | 一轮不能空手开 |
| send.before | 整次发送失败；`Unsupported` 视为 no-op | 不静默跳过改写 |
| send.after | 忽略 | 消息已发出，不能回滚 |
| turn_error | 用请求里带进来的 `default_actions` | 失败通知不能因策略故障消失 |
| clock | `false` | 不因策略故障主动开轮 |
| receive | `Record` | 失败开放，避免静默丢消息 |
| route | `Pass` | 路由不是安全闸门，失败不能吞消息 |

这张表是权威。C ABI、`abi_stable`、Lua 的适配器只能调用它，不能各自写一套
`unwrap_or(...)`。

#### 3.2.4 状态与顺序保证

**状态归属：**

| 状态 | 归属 | 插件可见性 |
|---|---|---|
| 记录、水位、未读、票据、任务记录 | core | 通过请求字段只读 |
| `last_wake_at` / `last_clock_at` / `last_record_at` | core | 通过上下文只读 |
| provider 自己的缓存、去重表 | 插件 | 插件自己负责，core 不持久化 |
| 策略配置 `options` | core 持有，按次传入 | 插件自己解析，schema 由 manifest 声明 |
| 归因流水 | core | 只读展示 |

**顺序保证：**

- route：按连接配置顺序调用，前一个返回 `Pass` 才轮到下一个；首个 `Drop` 或
  `Handle` 终止，后续 provider 不再调用；route 在 receive 和 wake 之前。
- receive：连接级，route 未处理时才调用；`Drop` 后不写流水、不唤醒。
- wake：记录入库后，按订阅者配置顺序逐个调用；同一订阅者的判定串行。
- delivery：一轮一次。
- send：`before` 按配置顺序；`after` 按配置顺序；`before` 失败终止后续 `before`。
- turn_error：一轮一次。
- clock：按 tick 对每个（会话，房间）调用一次。

**幂等与重试：**

- core 不自动重试决策调用。超时后该 provider 被标记 stuck，后续调用快速失败；
  执行器重置后同一输入可能再次被评估。插件必须允许“同一输入被看到多次”。
- 决策函数必须是纯的或只读写自己的内存状态；所有外部副作用只能通过返回值里的
  动作表达。

#### 3.2.5 输入快照、超时与隔离

- 外部 provider（Lua、动态库、进程）的调用统一走 `call_timed`：
  每个 provider 一个专用执行线程、单飞、按 `provider_calls.timeout_ms` 超时。
- 超时后：本次结果丢弃，按该点的失败策略处理；执行器标记 stuck；
  后续调用返回 `Busy`/`Stuck` 快失败；多代 stuck 后标记 `Dead`，需要重启或重载。
- 内置 provider 不走执行器，直接同步调用；因此内置实现必须自己保证线程安全。
- 输入在跨线程前必须做 owned 快照；插件不能持有 `&Record` 等借用越过调用边界。

### 3.3 八个扩展点

下面每个点都按“触发 / 入参 / 结果 / 错误 / 状态 / 权限 / 副作用”说明。
类型名以当前 kernel 为准，迁移后原样搬进 `plugin-api`。

#### 3.3.1 wake

- **触发**：一条记录入库后，对每个订阅该房间的会话调用。
- **入参**：`WakeContext`，字段包括 `session`、`room`、`connection`、`platform`、
  `sender_id`、`text`、`at_self`、`reply_to_self`、`has_body`、`event`、`tags`、
  `unread`、`last_wake_at`、`now`。
- **结果**：`WakeVerdict { decision: Wake | Stay, reason: Option<String>, delay_ms }`。
  `delay_ms` 由 core 截断到 `MAX_WAKE_DELAY_MS`（60 秒）。
- **错误**：任何错误、超时、panic → `Stay`；记录 provider 错误，不计入唤醒。
- **状态**：只读。插件不能改水位、未读或票据。
- **权限**：`WakeDecision`。
- **副作用**：禁止。不能发消息、不能打断、不能起任务。

#### 3.3.2 access

- **触发**：工具要读/写房间或访问连接之前。
- **入参**：`AccessRequest`，字段包括 `session`、`connection`、`room: Option<&RoomId>`、
  `subject`、`place`、`origin_room`、`action: Read | Send | Connection`、`subscriptions`。
- **结果**：`AccessDecision::Allow` 或 `Deny`。拒绝文案由 core 组织，插件不返回文案。
- **错误**：任何错误、超时、panic → `Deny`。
- **状态**：只读。
- **权限**：`AccessDecision`。
- **副作用**：禁止。
- **补充**：`origin_room` 与目标 `room` 刻意分开，跨房间发送时来源仍然是本轮房间。

#### 3.3.3 delivery

- **触发**：一轮开始、组装投递批次时。
- **入参**：`DeliveryRequest { session, room, records: &[Record], limit }`。
- **结果**：`DeliveryPlan { keep: Vec<u64>, omitted: Option<Omitted> }`。
  `keep` 是记录编号，顺序就是投递顺序；core 忽略未知编号。
- **错误**：任何错误、超时、panic → `default_delivery_plan(request)`。
- **校验**：`keep` 必须是输入 `records` 编号的子集；重复编号去重；超过 `limit`
  截断；非法结果按默认计划处理。composer 不能改内容、不能改水位、不能 ack。
- **状态**：只读。
- **权限**：`DeliverPlan`。
- **副作用**：禁止。

#### 3.3.4 send

- **触发**：`before_send` 在消息组装后、真正发出前；`after_send` 在平台回执后。
- **入参**：`SendContext { session, room, origin: Message | TurnFailure | Route }`；
  `before_send` 额外拿到 `&mut Vec<Element>`。
- **结果**：`before_send` 返回 `Result<(), PluginError>`；`after_send` 无返回值。
- **错误**：
  - `before_send` 返回错误 → 整次发送失败；`Unsupported` 例外，视为 no-op；
  - `after_send` 的错误忽略；
  - 超时/panic 按 `before` 失败处理。
- **状态**：`before` 可以改写元素；不能改目标房间、不能改发送者身份；`after` 只读。
- **权限**：`SendHook`。
- **副作用**：不能直接发消息；只能改写或拒绝这次发送。
- **补充**：路由动作触发的发送（`origin = Route`）同样经过 `before/after`，不能绕开。

#### 3.3.5 turn_error

- **触发**：一轮失败收尾时。
- **入参**：`TurnErrorRequest { session, room, error, actions, default_actions }`。
- **结果**：`Vec<TurnErrorAction>`；动作只有 `Log` 和 `Send { room }`。
- **错误**：任何错误、超时、panic → 使用 `default_actions`。
- **状态**：只读。
- **权限**：`TurnErrorDecision`。
- **副作用**：不能直接发送；只能选择动作，由 core 执行。
- **约束**：返回的动作数量由 core 限制；`Send` 目标必须通过 access 校验；
  同一故障的 in-band 去重仍由 core 负责。

#### 3.3.6 clock

- **触发**：周期 tick / 房间空闲检查时，对每个（会话，房间）调用。
- **入参**：`ClockContext { session, room, now, last_clock_at, last_wake_at,
  last_record_at }`。
- **结果**：`bool`。
- **错误**：任何错误、超时、panic → `false`。
- **状态**：只读；core 在调用前已经检查“是否有 pending 轮次”。
- **权限**：`ClockDecision`。
- **副作用**：禁止。不能注入内容、不能发消息；只能决定开不开一轮。

#### 3.3.7 receive

- **触发**：记录入库前，route 没有处理这条消息时。按连接配置。
- **入参**：`ReceiveRequest { connection, platform, room, sender, kind, elements,
  text, at_self }`。
- **结果**：`ReceiveDecision::Record` 或 `Drop`。
- **错误**：任何错误、超时、panic → `Record`。
- **状态**：只读；`Drop` 后不写流水、不唤醒。
- **权限**：`ReceiveDecision`。
- **副作用**：禁止。
- **补充**：receive 是连接级，不是房间级；配置缺省等价于 `acb.receive.all`。

#### 3.3.8 route

- **触发**：消息入库前，按连接配置顺序调用。
- **入参**：`RouteContext { connection, platform, room, sender, is_admin, kind,
  elements, text, at_self, reply_to_self, has_body, event, tags, sessions }`。
  `text` 只拼 `Text` 段，避免 @ / 引用占位符干扰命令匹配。
- **结果**：
  - `Pass`：继续下一个 route provider，最终走 receive + 入库 + wake；
  - `Drop { reason }`：吞掉，不写流水、不唤醒；
  - `Handle { store, wake, actions }`：交给代码处理；`store` 决定命令本身是否写流水，
    `wake` 决定是否让 AI 也看到，`actions` 是动作列表。
- **错误**：任何错误、超时、panic → `Pass`。路由不是安全闸门，失败不能吞消息。
- **状态**：只读。插件自己的去重表是易失的；core 不提供入库前的记录 ID。
- **声明**：descriptor 带 `RouteDecision`，表示它实现了 route；manifest 也可以列出
  它打算使用的动作面（`send` / `interrupt` / `task` / `wake`）。这些只用于启动期
  核对声明与实际注册是否一致，不作为运行期授权；插件本身可信。
- **副作用**：插件不能直接做 I/O；只能返回动作，由 core 在锁外执行。
- **动作语义**：
  - `Interrupt { scope: Room | Sessions(Vec<String>) | Connection }`：core 立即清理
    pending、标记 active、发 interrupt；`Sessions` 必须是订阅了触发房间的会话；
    受影响作用域写归因记录（迁移目标）；
  - `Send { room: Option<RoomId>, elements }`：目标缺省是触发房间；以桥的身份发送。
    **目标行为**：先按目标房间跑 access 校验，再走 `prepare_send` 的 send hooks。
    **当前只检查目标房间是否有订阅者，没有 access 校验**——这是迁移时要补的缺口；
  - `Task { handler, params }`：创建任务记录并交给已注册的 handler；handler 不存在
    时拒绝并写归因记录（迁移目标；当前回退内置 `task::run`，且无归因）；
  - `Wake { session: Option<String>, room: Option<RoomId>, reason }`：绕过 wake 策略
    直接请求唤醒；必须检查目标订阅关系、频率（当前 6 次/分钟），并写归因记录。
    不做信任或能力授权检查。
- **归因**：`Pass` / `Drop` / `Handle` 以及四个动作都要留痕，见第 9.4 节。

### 3.4 权威矩阵

“支持”指该载体能注册这个点的 provider；“内置”指 `NativeLoader` 默认装了什么。

| 扩展点 | 目标：native（编入） | 目标：dynamic C ABI | 目标：dynamic abi_stable | 目标：Lua |
|---|---|---|---|---|
| wake | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| access | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| delivery | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| send | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| turn_error | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| clock | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| receive | 支持 / 内置默认 | 支持 | 支持 | 支持 |
| route | 支持 / 内置默认 | 支持（必须补齐） | 支持 | 支持（已有） |

注意：`NativeLoader` 是“内置默认实现”的 loader，本身也必须是 8/8——补齐 send 的默认
hook 和 route 的 `PassThroughRouteProvider`。native 载体的第三方实现由新的
`NativeProviderLoader`（编译期注册）承担，同样 8/8。两个 loader 的 manifest 都必须与
实际注册严格一致。

## 4. plugin-api crate 结构

### 4.1 crate 划分

```text
crates/plugin-api/           acb-plugin-api        权威语义
crates/adapter-api/          acb-adapter-api       C ABI 投影（现有 crate）
crates/plugin-abi-stable/    acb-plugin-abi-stable abi_stable 投影（新增）
crates/kernel/               acb-kernel            宿主；依赖 plugin-api
crates/adapters/dynamic/     acb-adapter-dynamic   dlopen + 双通道分派
crates/plugins/lua/          acb-plugin-lua        Lua 投影与宿主
```

### 4.2 模块

```text
plugin-api/src/
  lib.rs                re-export；版本常量；feature 门
  ids.rs                ProviderId / ExtensionPoint / Capability / PluginSource
  manifest.rs           Carrier / AbiChannel / PluginManifest / ProviderDescriptor /
                        LoaderManifest / ManagerManifest
  error.rs              PluginError / PluginFault / fail_policy
  limits.rs             每个点的输入/结果上限（动作数、元素字节、会话数…）
  state.rs              PluginLifecycle / ReloadScope / 状态归属文档
  points/
    mod.rs
    wake.rs             WakeContext / WakeVerdict / WakeDecision
    access.rs           AccessRequest / AccessDecision
    delivery.rs         DeliveryRequest / DeliveryPlan / default_delivery_plan
    send.rs             SendContext / SendOrigin / SendHook 契约
    turn_error.rs       TurnErrorRequest / TurnErrorAction
    clock.rs            ClockContext
    receive.rs          ReceiveRequest / ReceiveDecision
    route.rs            RouteContext / RouteDecision / RouteAction / InterruptScope
  native.rs             native 载体的 trait 定义（WakeProvider 等）
  conformance/
    mod.rs              corpus 类型与 runner trait（feature = "conformance"）
    corpus/             *.json golden cases
```

依赖：

```toml
[dependencies]
acb-proto = { path = "../proto" }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
```

`abi_stable`、`dlopen`、Lua 运行时都不进 `plugin-api`。

### 4.3 与现有 kernel / adapter-api 的关系

迁移期：

```rust
// crates/kernel/src/plugin.rs
pub use acb_plugin_api::{
    AccessAction, AccessDecision, AccessRequest, Capability, ClockContext, DeliveryPlan,
    DeliveryRequest, ExtensionPoint, InterruptScope, PluginError, ProviderDescriptor,
    ProviderId, ReceiveDecision, ReceiveRequest, RouteAction, RouteContext, RouteDecision,
    SendContext, SendOrigin, SendHook, TurnErrorAction, TurnErrorRequest, WakeContext,
    WakeDecision, WakeVerdict, ...
};
```

这样引擎和现有调用点不用一次性改路径；但类型的所有权已经归 `plugin-api`。
kernel 只保留注册表、loader trait、`resolve_*`、执行器和引擎集成。

`adapter-api` 的 Wire 类型改为：

```rust
impl From<&RouteContext<'_>> for WireRouteContext { ... }
impl TryFrom<WireRouteDecision> for RouteDecision { ... }
```

转换只做字段搬运和校验，不写业务默认值。

## 5. 版本策略

### 5.1 四层版本

| 版本 | 归属 | 初始值 | 含义 | 变更规则 |
|---|---|---|---|---|
| `API_VERSION` | `plugin-api` | 1 | 语义契约版本 | 语义不兼容改动升主版本；新增带默认行为的能力升次版本 |
| `C_ABI_VERSION` | `adapter-api` | 1 | C ABI 的 JSON schema 版本 | Wire 类型增删改时调整；删字段/改含义升主版本 |
| `C_ENTRY_REVISION` | `adapter-api` | 1 / 2 | 符号表形态：`acb_plugin_entry` 与 `AcbPluginV2` | 只描述“怎么找到入口”，与语义无关 |
| `ABI_STABLE_VERSION` | `plugin-abi-stable` | 1 | `StableAbi` 布局版本 | 任何布局不兼容改动升版本 |
| `LUA_API_VERSION` | `plugins/lua` | 1 | 脚本可见的字段/错误形状 | 脚本 API 不兼容改动升版本 |

明确区分：

- **入口修订**（entry revision）解决“怎么调用”；
- **schema 版本**（C ABI version）解决“字段长什么样”；
- **语义版本**（API version）解决“结果是什么意思”。

今天 C ABI 的 v1/v2 属于入口修订，不是“两条 ABI 通道”之一。文档和 manifest 里不得
把 `AcbPluginV2` 叫作 `abi_stable`。

### 5.2 兼容窗口

- `API_VERSION`：core 要求主版本精确匹配；次版本可以向前兼容（旧插件缺新字段时用
  文档化的默认值）。
- `C_ABI_VERSION`：core 维护 `[MIN_C_ABI, CURRENT_C_ABI]` 窗口。窗口内的旧插件继续
  工作；低于下限直接拒绝。
- `ABI_STABLE_VERSION`：要求精确匹配；`abi_stable` 自身的布局校验在前，拒绝时给出
  “布局不匹配”而不是调用后崩溃。
- `C_ENTRY_REVISION`：core 同时识别 v1 和 v2；v2 优先用于可卸载场景。
- 插件 manifest 必须显式写 `api_version`、`abi`、`abi_version`；缺省只对旧 C ABI
  插件生效（`abi = "c"`、`abi_version = 1`），并在控制台标记为“旧清单”。

### 5.3 能力声明核对

- 插件声明能力集合 `capabilities`；loader 声明它实际提供的能力集合 `capabilities`。
- 安装期校验：`plugin.capabilities ⊆ loader.capabilities`，且包含该扩展点的必需能力。
  这是“声明与实际注册是否一致”的核对，不是运行期授权。
- 插件声明的扩展点必须在其 loader 声明的扩展点内；loader 声明的点必须真有实现，
  否则启动失败。
- 未知能力字符串 → 启动失败，不忽略。理由：忽略会让“声明了但没生效”重新出现。
- 运行期不改变能力集合；重载时重新走安装期核对。

### 5.4 变更流程

任何影响语义或 wire 的改动必须同时更新：

1. `plugin-api` 的类型/默认函数；
2. 两条投影的映射；
3. `conformance/corpus` 的 golden case；
4. 版本常量与兼容窗口；
5. 文档里的矩阵和迁移说明。

缺少任意一项，视为未完成。

## 6. 两条 ABI 通道

### 6.1 共同边界

两条通道共享同一组硬边界：

- **只能传值**：请求和结果都是 owned 数据；不跨边界传借用、引用、回调闭包。
- **UTF-8**：字符串统一 UTF-8；长度按字节计。
- **错误必须可分类**：都映射到 3.2.2 的故障分类。
- **无通道私有默认值**：默认行为只能来自 `plugin-api`。
- **无通道私有能力**：能力字符串和含义完全一致。
- **调用方持有生命周期**：core 在调用返回后立即把结果转成语义类型；插件不得保留
  指向 core 内存的指针。
- **panic 不得穿过边界**：投影层必须在插件侧包 `catch_unwind`；跨 `extern "C"`
  展开是未定义行为。
- **超时由 core 负责**：通道本身不实现超时；core 的执行器负责。
- **不做自动探测**：manifest 说哪条就哪条；缺符号/布局不符 = 装载失败。

### 6.2 C ABI 投影

**入口：**

- v1：导出 `acb_plugin_entry`，返回函数表；
- v2：导出 `AcbPluginV2`，带 unload-safe 元数据。

**消息：**

- core → 插件：`CoreMessage::Decide { id, point, options, request }`；
- 插件 → core：`AdapterMessage::Reply { id, result: WireResult }`。

**边界：**

- 数据是 JSON，Rust 布局无关；schema 由 `C_ABI_VERSION` 管理。
- 只能表达 JSON 能表达的值；枚举用字符串标签；`Option` 用缺省字段或 `null`。
- 插件返回未知 `decision` 字符串 → `InvalidInput`，按该点失败策略处理。
- 缓冲区由插件自己管理；core 只读返回的字符串。
- 动态库必须编译为 `cdylib`，且不得让 panic 跨 FFI 边界。

**今天的缺口：**

- [`WireManifest`](../../crates/adapter-api/src/lib.rs#L104-L139) 需要新增
  `abi`、`abi_version`、`api_version`（已有）、`capabilities`（已有）；
- [`WireResult`](../../crates/adapter-api/src/lib.rs#L512-L554) 需要新增路由结果；
- 需要新增 `WireRouteContext`、`WireRouteDecision`、`WireRouteAction`、
  `WireInterruptScope`。

**路由的 C ABI 形状（建议）：**

```rust
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum WireRouteDecision {
    Pass,
    Drop { #[serde(default)] reason: Option<String> },
    Handle {
        #[serde(default)] store: bool,
        #[serde(default)] wake: bool,
        #[serde(default)] actions: Vec<WireRouteAction>,
    },
}

#[serde(tag = "action", rename_all = "snake_case")]
pub enum WireRouteAction {
    Interrupt { scope: WireInterruptScope },
    Send { #[serde(default)] room: Option<RoomId>, #[serde(default)] elements: Vec<Element> },
    Task { handler: String, #[serde(default)] params: Value },
    Wake {
        #[serde(default)] session: Option<String>,
        #[serde(default)] room: Option<RoomId>,
        #[serde(default)] reason: Option<String>,
    },
}
```

`WireResult` 新增：

```rust
Route { decision: WireRouteDecision },
```

不复用 `Decision { decision: String }`，因为路由结果带结构化动作，字符串化会丢失
类型并让两条通道难以一致。

### 6.3 abi_stable 投影

**入口：**

```rust
#[no_mangle]
pub extern "C" fn acb_abi_stable_root() -> acb_plugin_abi_stable::RootModule { ... }
```

`RootModule` 持有：

- `api_version: u32`
- `abi_version: u32`
- `manifest() -> PluginManifest`
- `descriptors() -> RVec<ProviderDescriptor>`
- `instantiate(options: RStr) -> RResult<ProviderVTable, PluginFault>`
- `shutdown()`

**边界：**

- 所有跨边界类型实现 `StableAbi`；枚举显式 `#[repr(C)]` 和固定判别值。
- 用 `RString` / `RStr` / `RVec` / `ROption` / `RResult`，不用 `String` / `Vec` /
  `Option` 直接跨边界。
- 不使用泛型跨边界；需要泛型时在插件侧单态化。
- 布局版本由 `ABI_STABLE_VERSION` 和 `abi_stable` 的布局校验共同保证。
- provider 实例的 vtable 必须保持 ABI 稳定；实例生命周期内库不能卸载。
- 卸载仅在 `UNLOAD_SAFE` 且无实例引用时进行；否则保持 resident。

**路由的 `abi_stable` 形状：**

- `RouteContext` 的借用字段在边界上变成 owned 的 `RStr` / `RVec`；
- `RouteDecision` 用 `#[repr(C)]` 枚举，`Handle` 里是 `RVec<RouteAction>`；
- `RouteAction` 的 `params: Value` 改为 `RStr`（JSON 文本），避免把 `serde_json`
  布局带过边界；
- 所有字符串长度和动作数量在插件侧先按 `plugin-api` 的上限校验，core 再校验一次。

### 6.4 投影对照表

| 扩展点 | 权威类型（plugin-api） | C ABI 投影 | abi_stable 投影 |
|---|---|---|---|
| wake | `WakeContext` → `WakeVerdict` | `WireWakeRequest` → `WireResult::Decision` | `WakeContextRepr` → `WakeVerdictRepr` |
| access | `AccessRequest` → `AccessDecision` | `WireAccessRequest` → `WireResult::Decision` | `AccessRequestRepr` → `AccessDecisionRepr` |
| delivery | `DeliveryRequest` → `DeliveryPlan` | `WireDeliveryRequest` → `WireResult::Delivery` | `DeliveryRequestRepr` → `DeliveryPlanRepr` |
| send.before | `SendContext` + `Vec<Element>` | `WireSendRequest` → `WireResult::Elements` | `SendContextRepr` + `RVec<ElementRepr>` |
| send.after | `SendContext` + `Sent` | `WireSendRequest`（after 标记）→ `WireResult::Sent` | `SendContextRepr` + `SentRepr` |
| turn_error | `TurnErrorRequest` → `Vec<TurnErrorAction>` | `WireTurnErrorRequest` → `WireResult::Actions` | `TurnErrorRequestRepr` → `RVec<TurnErrorActionRepr>` |
| clock | `ClockContext` → `bool` | `WireClockRequest` → `WireResult::Decision` | `ClockContextRepr` → `bool` |
| receive | `ReceiveRequest` → `ReceiveDecision` | `WireReceiveRequest` → `WireResult::Decision` | `ReceiveRequestRepr` → `ReceiveDecisionRepr` |
| route | `RouteContext` → `RouteDecision` | `WireRouteContext` → `WireResult::Route`（新增） | `RouteContextRepr` → `RouteDecisionRepr`（新增） |

### 6.5 分派流程

```text
DynamicLoader 扫描 .so
  → 读 manifest（C: 符号表；abi_stable: RootModule）
  → 校验 carrier/abi/abi_version/api_version/extensions/capabilities
  → 按 abi 构造通道适配器
      abi = "c"          → CAbiProvider
      abi = "abi_stable" → StableAbiProvider
  → 两者都实现同一组 kernel trait（WakeProvider…RouteProvider）
  → 注册到 PluginManager，descriptor.source = Dynamic
```

引擎只看到 `Arc<dyn RouteProvider>` 等 trait，看不到通道。通道选择不得进入
`engine.rs`。

### 6.6 明确不做

- 不因为 C ABI 缺某个新字段就自动降级到旧行为；缺字段 = 用 `plugin-api` 的默认值，
  与通道无关。
- 不在 manifest 声明 `abi = "c"` 时去探测 `abi_stable` 符号。
- 不在一个插件里混用两条通道。
- 不把 `AcbPluginV2` 当作 `abi_stable`。

## 7. 跨通道一致性测试

### 7.1 三层测试

**第一层：类型往返。**

对每个扩展点，构造代表性输入：

- 最小字段；
- 全部字段；
- 空集合 / 边界值 / 超限值；
- 错误路径。

分别走 C ABI 和 `abi_stable` 的编码/解码，再转回语义类型，断言两边得到的
`plugin-api` 值完全相等。比较用规范 JSON（键排序、浮点规范化、`Option` 显式化）。

**第二层：语义一致性（核心）。**

同一份参考策略实现，编译成两个 `.so`：

- `fixtures/policy-c-abi`：C ABI 出口；
- `fixtures/policy-abi-stable`：`abi_stable` 出口。

两份共用 `plugin-api/conformance/reference` 里的同一份 Rust 策略代码，只替换入口
shim。这样测的是通道差异，不是两份实现的差异。

用同一份 golden corpus 分别喂给两个 `.so`，断言：

- 每个用例的 `RouteDecision` / `WakeVerdict` / `AccessDecision` / … 相等；
- 错误用例的故障分类和失败策略结果相等；
- 超时用例的最终行为相等（都按该点的 fail policy）。

**第三层：跨载体一致性。**

同一份语义在 native 实现和 Lua 脚本上再跑一遍。Lua 只覆盖它能表达的字段；
数字精度、空值、布尔转换必须在 corpus 里显式规定，避免语言差异被误判为语义差异。

### 7.2 参考实现与双出口

```text
plugin-api/src/conformance/
  reference/           同一份策略实现（纯 Rust，不依赖宿主）
  corpus/              golden cases
  runner.rs            读取 corpus、调用语义 trait、比较结果
fixtures/policy-c-abi/        cdylib：C ABI shim → reference
fixtures/policy-abi-stable/   cdylib：abi_stable shim → reference
```

约束：

- reference 只依赖 `plugin-api`，不依赖 kernel；
- 两个 fixture 的 `Cargo.toml` 只差入口依赖；
- corpus 是唯一期望值来源，测试代码里不写第二套期望。

### 7.3 golden corpus 格式

```json
{
  "point": "route",
  "case": "admin-stop-command",
  "options": { "prefix": "!!" },
  "request": {
    "connection": "qq",
    "platform": "qq",
    "room": "qq:group/769843907",
    "sender": { "id": "10001", "name": "alice" },
    "is_admin": true,
    "kind": "message",
    "elements": [{ "type": "text", "text": "!!stop" }],
    "text": "!!stop",
    "at_self": false,
    "reply_to_self": false,
    "has_body": true,
    "event": null,
    "tags": [],
    "sessions": ["main"]
  },
  "expect": {
    "decision": "handle",
    "store": false,
    "wake": false,
    "actions": [{ "action": "interrupt", "scope": "room" }]
  }
}
```

错误用例：

```json
{
  "point": "send",
  "case": "before-error-fails-send",
  "request": { "...": "..." },
  "expect": {
    "fault": "rejected",
    "policy": "fail"
  }
}
```

### 7.4 失败模式一致性

必须覆盖：

- 插件返回 `Error`；
- 插件 panic；
- 插件超时（用一个 sleep 超过 `timeout_ms` 的 fixture）；
- 插件返回超限动作（动作数、元素字节、会话数）；
- 插件返回未知枚举值；
- 插件缺符号 / 布局不匹配 / 版本过低。

断言：两条通道得到同一故障分类，并按同一 fail policy 落地。任何一侧出现“静默
成功”都算失败。

### 7.5 CI 门禁

- 任何 `plugin-api` 类型/默认函数改动 → 必须跑 corpus；
- 任何 `C_ABI_VERSION` / `ABI_STABLE_VERSION` / `API_VERSION` 改动 → 必须同时更新
  corpus 和兼容窗口；
- 两条通道的 corpus 结果必须逐字节相等（规范 JSON）；
- 不允许“通道专属豁免”；如果确实无法表达，必须改语义类型或明确标记该字段为
  通道不支持，并在启动期拒绝，而不是运行时静默。

## 8. manifest 与启动期校验

### 8.1 manifest 形状

**loader manifest（载体级）：**

```rust
pub struct LoaderManifest {
    pub kind: LoaderKind,
    pub carrier: Carrier,                 // Native | Dynamic | Script
    pub abi_channels: Vec<AbiChannel>,    // Dynamic: [C, AbiStable]；其它为空
    pub api_version: u32,
    pub capabilities: Vec<Capability>,    // 本载体实际提供的能力声明
    pub extensions: Vec<ExtensionPoint>,  // 本载体声明支持的扩展点
}
```

**插件 manifest（产物级，一个产物一个 provider）：**

```toml
[plugin]
id = "vendor.policy"
carrier = "dynamic"          # native | dynamic | script
abi = "abi_stable"           # dynamic 必填：c | abi_stable
abi_version = 1
api_version = 1
extension = "route"
capabilities = ["route_decision", "emit_send"]
summary = "把 !! 命令转成路由动作"
```

规则：

- 插件本身可信，manifest 不需要 `trust` 字段，也没有信任分级。
- `capabilities` 必须显式列出；缺省为空，不能“默认全给”。它只用于启动期核对
  “声明与实际注册是否一致”。
- 旧 C ABI 清单缺 `abi` 时按 `abi = "c"`、`abi_version = 1` 处理，并在控制台标记。
- `extension` 目前是单值。多 provider 产物不在本期范围。

### 8.2 校验顺序

启动期分四段，任何一段失败都不进入监听：

**A. loader manifest**

1. `api_version` 精确匹配；
2. `extensions` 非空；
3. 每个扩展点都是 core 认识的点；
4. 每个声明的点都有 core 的分派路径（否则 loader 的承诺无法兑现）；
5. `abi_channels` 与 `carrier` 匹配：dynamic 必须至少包含一条；native/script 必须为空。

**B. 插件 manifest**

1. `carrier` 与 loader 的 `kind` 匹配；
2. `abi` 在 loader 的 `abi_channels` 里；
3. `abi_version` 在 core 的兼容窗口内；
4. `api_version` 匹配；
5. `extension` 在 `loader.extensions` 里——**这是今天缺的那一条**；
6. 该扩展点的必需能力在 `capabilities` 里；
7. 声明能力不得超过 loader 实际提供的能力（`capabilities ⊆ loader.capabilities`）；
8. 没有未知能力字符串；
9. `id` 全局唯一，namespace 不属于保留区。

这不是准入授权，只是“声明和实际能力对得上”的一致性检查。

**C. 注册后一致性检查（事务内）**

loader 的 `install` 执行完后，core 在同一个事务里检查：

```text
for descriptor in loader 本次注册的 descriptor:
    descriptor.extension_point ∈ loader.manifest.extensions
    descriptor.capabilities ⊆ loader.manifest.capabilities
    descriptor.api_version == loader.manifest.api_version
```

任一条不满足：回滚本次 loader 的全部注册，返回
`PluginError::LoaderManifestMismatch { loader, provider, point }`，启动失败。

这一条同时修掉 `NativeLoader` 声明 Route 但不注册的问题：要么把 Route 从 manifest
删掉，要么补一个 `acb.route.pass` 内置实现。推荐前者 + 单独的 native 第三方 loader。

**D. 配置引用**

所有配置里引用的 provider id 都必须在已注册集合里，并且点的类型匹配：

- `[connections.*.routes]` → `route`
- `[connections.*.receive]` → `receive`
- `[sessions.*.wake]` → `wake`
- `[sessions.*.access]` → `access`
- `[sessions.*.delivery]` → `delivery`
- `[sessions.*.turn_error]` → `turn_error`
- `[sessions.*.clock]` → `clock`
- `[sessions.*.outbound].before/after` → `send`

失败返回 `ConfigError::UnknownProviderReference { path, id, expected_point }`。
空列表、缺省字段跳过。

### 8.3 错误示例

| 场景 | 今天 | 目标 |
|---|---|---|
| 插件声明了 loader 未实现的扩展点 | 装载时拒绝（非静默） | 启动失败：`plugin X declares route; loader dynamic supports [...]` |
| `NativeLoader` 声明 Route 但不注册 | 无感 | 启动失败：`loader native declared route but registered none` |
| 配置引用不存在的 provider | 运行到该点才报错/回退 | 启动失败，指出配置路径和期望点 |
| 插件声明未知能力 | 被忽略 | 启动失败 |
| 插件声明 `abi = "abi_stable"` 但缺符号 | 未定义 | 装载失败，不尝试 C 入口 |
| 插件 `abi_version` 过低 | 可能崩溃 | 装载前拒绝 |

### 8.4 事务性与退出码

- 每个 loader 的安装是一个事务：注册、一致性校验、失败回滚。
- 所有 loader 安装完后才做配置引用校验；失败则整体启动失败。
- 退出码沿用现有约定：加载失败退出 `78`。
- 不允许“装一半、报个日志、继续跑”。

### 8.5 控制台可见性

`/api/plugins` 的每条记录增加：

- `carrier`
- `abi`
- `abi_version`
- `api_version`
- `declared_extensions`
- `capabilities`
- `lastError`

`/api/plugins` 的 loader 区块增加 `declared_extensions` 和 `capabilities`，
让“声明”和“实际注册”可以对照。

## 9. route 高权开放

### 9.1 开放口径

插件本身可信，因此 route 不设信任分级、allowlist 或能力授权准入。三载体同时开放到
8/8：native 注册 Send/Route，Lua 已是 8/8，`.so` 补 route。与能力对齐**同批交付**的
配套是：

1. **动作留痕**：决定和动作进入归因流水，能定位到消息、目标、产生的副作用；
2. **超时与隔离**：调用有预算，卡死不会拖垮桥；
3. **启动期交叉校验**：manifest 声明与实际注册严格一致，不一致启动失败。

这三项是交付内容，不是“先满足才能开放”的否决条件；反过来也不能因为要补齐能力就
跳过它们。归因与超时是配套，不是准入审查。

manifest 声明（`route` + 动作面 + 能力）只用于启动期核对声明与实际注册是否一致；
不一致就启动失败，但一致之后不会在运行期再次拦截。

### 9.2 声明核对

- route 产物的 manifest 声明 `extension = "route"`，并可列出它使用的动作面
  （`send` / `interrupt` / `task` / `wake`）。
- loader manifest 声明它实际提供 route 和哪些动作面。
- 启动期检查：插件声明的动作面必须是 loader 实际提供的子集；loader 声明支持的
  route 必须真有实现。任一条不满足，启动失败。
- 运行期不因来源、通道、动作类型做二次授权。归因负责可追溯，超时负责可用性。

### 9.3 超时与隔离

**决策超时：**

- route 调用已经走 `call_timed`，沿用 `[provider_calls] timeout_ms`。
- route 的超时可以与其它点不同：建议增加 `route_timeout_ms`，缺省沿用全局值。
- 超时 → `Pass` + 归因记录 + 执行器标记 stuck。

**隔离：**

- 每个外部 provider 一个专用执行线程，单飞；同一 provider 的 route 调用不会并发。
- 执行器 busy → `Busy` 快失败，按 `Pass` 处理。
- 多代 stuck → `Dead`，后续调用快速失败，直到重载/重启。
- in-process 动态库无法被强制抢占；超时只是“core 不再等”，线程仍可能继续跑。
  因此每个 provider 的并发上限为 1，并用熔断阻止一个卡死的插件持续占用执行器。
  需要更强可用性隔离时，可以把该 provider 放到进程载体，但不以此做信任分级。

**动作执行预算：**

- 每个 decision 最多 `MAX_ROUTE_ACTIONS`（建议 16）个动作；
- `Send` 的元素数量和字节数受 `plugin-api::limits` 限制；
- `Interrupt::Sessions` 最多 `MAX_INTERRUPT_SESSIONS`；
- `Task.params` 最大字节数受限；
- 超限 → 整个 decision 按 `Pass` 处理，并记归因（不做部分执行）。

### 9.4 归因（审计记录）

新增 `RouteAuditEntry`，与现有 `WakeAuditEntry` 并列。每条记录回答四件事：**哪个
provider 触发、什么动作、打到哪个目标、结果如何**（成功 / 被拒 / 失败 / 超时）。字段要能
支撑“事后追溯”和“回滚定位”，而不只是记一句“发生过”。

归因只做记录，不做授权、不做限流：不因为来源、通道或动作类型拒绝调用。理由是插件
本身可信；归因服务的是“哪次迭代、哪个插件干了什么”，用于回滚与自恢复。

```rust
pub struct RouteAuditEntry {
    pub at: i64,
    pub source: PluginSource,
    pub provider: ProviderId,
    pub connection: String,
    pub room: RoomId,
    pub correlation_id: String,        // 本次入站事件/消息的可关联 ID
    pub decision: RouteDecisionKind,   // pass / drop / handle
    pub actions: Vec<RouteActionAudit>,// 动作类型 + 目标 + 结果引用
    pub outcome: RouteOutcome,         // applied / rejected / timeout / oversize / error
    pub elapsed_ms: u64,
}

pub struct RouteActionAudit {
    pub kind: RouteActionKind,         // send / interrupt / task / wake
    pub target: Option<String>,        // 房间、会话、handler
    pub effect_ref: Option<String>,    // 出站记录 ID / 平台消息 ID / 任务 ID
    pub result: RouteActionResult,     // applied / rejected / failed
}
```

规则：

- `Pass` / `Drop` / `Handle` 都记一条；`Handle` 的每个动作都带自己的 `effect_ref`；
- `Send` 的 `effect_ref` 指向出站记录 ID 和平台回执 ID，便于定位和撤回/补偿；
- `Task` 的 `effect_ref` 指向任务 ID；
- `Interrupt` 记录受影响的作用域和会话列表；
- `Wake` 记录目标会话、房间和结果；
- 归因只活在进程内，环形保留（建议 200 条；当前 `Wake` 为 100 条），重启清空；
- 控制台提供只读查询；工具面可以后续加 `route_audit`，不在本期；
- 归因写入失败不能阻断消息主流程，但要在日志里报错。

### 9.5 动作执行约束

这些是正确性约束，不是信任/授权检查：

- `Send`：目标缺省为触发房间；必须以订阅者身份发送；执行前先按目标房间跑 access
  校验（当前缺），再走 `prepare_send` 的 send hooks；不能跨会话冒名。
- `Interrupt`：`Sessions` 必须是订阅触发房间的会话；`Connection` 只影响触发连接；
  不能打断其它连接。
- `Task`：handler 必须已注册；不存在则拒绝 + 归因；任务记录进入现有 `tasks`。
- `Wake`：检查目标订阅关系和频率（当前 6 次/分钟），写归因；不做信任或能力授权。
- 所有动作在锁外执行；decision 本身不持有引擎锁。

### 9.6 隔离的边界

超时和归因是**可用性与可追溯性**手段，不是安全沙箱。in-process 动态库的超时不能让
线程停下，只能让 core 不再等待；单飞和熔断防止它继续拖累别的调用。插件本身可信，
所以本设计不把“防恶意代码”作为目标；需要更强资源隔离时再选进程载体。

## 10. 迁移路径

### 10.1 阶段划分

| 阶段 | 内容 | 三载体点位数（做完后） | 验收 |
|---|---|---|---|
| P0 | 冻结基线：补现有行为的回归测试 | 6 / 7 / 8（起点） | 现有测试全绿；**中间态，不得发布** |
| P1 | 抽出 `plugin-api`，kernel re-export | 6 / 7 / 8 | 编译、测试全绿；**中间态，不得发布** |
| P2 | native 补 Send/Route 注册 + route 归因 + 交叉校验 + 超时/隔离 | 8 / 7 / 8 | native 8 声明 / 8 注册；归因四动作覆盖；**中间态，不得发布** |
| P3 | `.so` route 三件套：语义 + wire + 分派；同批接入归因/超时 | 8 / 8 / 8 | C ABI 8/8；四个动作与三种决定可跑；**可发布** |
| P4 | `abi_stable` route 投影 + 双通道一致性 | 8 / 8 / 8 | 跨通道 corpus 全绿；**可发布** |
| P5 | 第三方 native loader + Lua 对齐 + 文档/脚手架 | 8 / 8 / 8 | 矩阵全绿；**可发布** |

规则：每个发布批次结束时断言三载体的点位数相同（8/8/8）。表里 P0–P2 是同一批次内的
中间态，只能内部验证、不得发布；P2 与 P3 合并为一次发布，归因、交叉校验、超时/隔离
必须随 `.so` route 一起上线，不能拆开上线，也不能等 `.so` 补完再补。

### 10.2 `NativeLoader` 补齐到 8/8

**问题拆解：**

- `NativeLoader` 是“内置默认实现装载器”，不是“native 载体”的全部。
- 它声明 Route 但不注册，是声明不诚实；它不声明 Send，也不注册。
- 第三方编入 native 需要另一个入口，今天只有 `ToolLoader` 支持工具。

**迁移：**

1. **保留 8 点声明**：manifest 继续声明 wake、access、delivery、send、turn_error、
   clock、receive、route；不通过删声明来对齐。
2. **补 Send 注册**：在 `install` 里用 `register_send_hook` 注册一个默认发送钩子
   （透传，不做额外改写），让 Send 从“未声明未注册”变成“已声明已注册”。
3. **补 Route 注册**：在 `install` 里 `register_route` 注册
   `PassThroughRouteProvider`（[plugin.rs](../../crates/kernel/src/plugin.rs#L1158-L1164)），
   提供 `RouteDecision` 能力；它只做透传，不改变默认路由语义。
4. **新增 `NativeProviderLoader`**：接受
   `Vec<(ProviderDescriptor, ProviderInstance)>`，按点的类型调用
   `register_wake` / `register_access` / … / `register_route` / `register_send_hook`，
   支持全部 8 个点。它和 `NativeLoader` 一样是编译期装载，不读文件。
5. **第三方编入实现**用 `NativeProviderLoader` 注册，namespace 不能是 `acb`。
6. **测试**：断言 `NativeLoader` 的 manifest 与它实际注册的 descriptor 集合完全相等
   （8 个点、9 个 provider：clock 两个）；`NativeProviderLoader` 能注册 8 个点各一个
   provider。

### 10.3 route 归因、交叉校验与超时/隔离（与能力对齐同批交付）

**目标**：route 的决策与四个动作（`Wake` / `Send` / `Interrupt` / `Task`）各产生一条
可追溯记录，回答四个问题——哪个 provider 触发、什么动作、打到哪个目标、结果如何
（成功 / 被拒 / 失败 / 超时）。只做归因，不做授权、不做限流；理由是插件可信，归因
用于事后定位“哪次迭代、哪个插件干了什么”，服务回滚与自恢复。

**同批交付的理由**：三载体补齐 8/8 的同一批改动里，route 的动作面会同时扩大到 native
和 `.so`。如果归因、交叉校验、超时/隔离不同批到位，动作面扩大后无法定位问题。这三项
不是准入门槛，而是同一批交付的必要组成。

**现状差距（迁移前）：**

| 对象 | 现状 |
|---|---|
| 决策 | 只有错误 / panic 日志，没有决策归因；`Pass` / `Drop` / `Handle` 无法事后统计 |
| `Wake` | 有 `WakeAuditEntry`，拒绝与接受都记；但进程内环形 100 条，重启清空 |
| `Send` | 成功才写出站流水，没有归因；失败、无订阅者、组装失败都只有日志 |
| `Interrupt` | 无前置检查，只有一行 `route_interrupt` 日志；无记录、无审计 |
| `Task` | 只有内存 `TaskRecord`（上限 200），重启清空；无归因 |

**要做的事：**

1. 定义 `RouteAuditEntry` / `RouteActionAudit`（字段见 9.4），决策与每个动作都落一条；
2. `Wake` 从 `WakeAuditEntry` 并入统一归因，保留“拒绝也记录”的行为；
3. `Send` 记录目标房间、是否被拒（无订阅者 / 组装失败 / 组装后被拒）、`effect_ref`
   （出站记录 ID + 平台回执 ID）；失败必须有记录；
4. `Interrupt` 记录作用域（Room / Sessions / Connection）与受影响会话列表；
5. `Task` 记录 handler、任务 ID、最终状态；handler 不存在时记录“被拒”；
6. 超时、`Pass`、`Drop` 也各记一条，带 `elapsed_ms`；
7. 归因写入不能阻断主流程；写入失败只报日志。

**同时补交叉校验**：归因里的 `provider` 必须是真实注册的 provider，否则记录会指向
不存在的实现。因此同批把 `install_loader` 的“manifest 声明 vs 实际注册 descriptor”
交叉校验补齐（见 8.2、10.2）：三载体的声明集合与实际注册集合必须严格相等；`NativeLoader`
的 8 个声明对应 8 个点注册，`.so` / Lua / manager 声明的扩展点必须是其实际注册集合的
超集。不一致在启动期报错，不存在“删声明来对齐”的选项。

**验收**：fixture 覆盖四个动作的成功、被拒、失败、超时；每条记录能走通
provider → 动作 → 目标 → 结果；`Send` / `Task` 能凭 `effect_ref` 定位到出站记录或任务；
进程存活期间可查，重启丢失可接受（持久化不在本期）。

### 10.4 `.so` 补齐 route 到 8/8

与 10.3 同批交付，不设先后前置。`.so` 侧要补的完整链路：

1. **语义层**：`plugin-api` 定义 `RouteContext` / `RouteDecision` / `RouteAction` /
   `InterruptScope`（从 kernel 搬过去），并为 `RouteContext` 提供稳定的 serde 形状；
   `Pass` / `Drop` / `Handle { store, wake, actions }` 的等价表达在语义层固定。
2. **C ABI**：`adapter-api` 增加 `WireRouteContext`、`WireRouteDecision`、
   `WireRouteAction`、`WireInterruptScope`；`WireResult` 增加 `Route` 变体；
   `WireRouteAction` 必须覆盖 `Wake` / `Send` / `Interrupt` / `Task` 四个动作；
   `C_ABI_VERSION` 升到 2，兼容窗口同时接受 1 和 2（旧插件不会返回新变体）。
3. **manifest**：`WireManifest` 增加 `abi`、`abi_version`；`DynamicLoader::manifest`
   增加 `Route` 和对应能力，声明变为 8/8。
4. **分派**：`LoadedPlugin::load` 增加 `"route" => ExtensionPoint::Route`；
   新增 `DylibRouteManager`，`capability()` 返回 `RouteDecision`；
   `decide` 把 `WireResult::Route` 转成 `RouteDecision`；错误/超时 → `Pass`。
5. **超时与隔离**：route 决策走 `call_timed`；单飞、stuck/dead 熔断；动作数量和
   载荷受 `plugin-api::limits` 约束。不引入信任清单或能力授权。
6. **归因**：接入 10.3 的统一归因，不另造一套；`.so` route 的决策与四个动作必须与
   native / Lua 写同一种 `RouteAuditEntry`，`Send` / `Task` 填 `effect_ref`。
7. **fixture**：新增 `crates/fixtures/route-so`（C ABI）和
   `crates/fixtures/route-abi-stable`，共用同一份 reference 实现。
8. **回归**：旧 `.so`（无 route）继续工作；新 route `.so` 在旧 core 上装载失败，
   而不是运行期静默。

### 10.5 Lua 对齐（保持 8/8）

Lua 已是 8/8，本阶段不新增点，只把共用控制补齐，防止 Lua 成为唯一“特殊路径”：

- manifest 显式声明 `carrier = "script"`、`api_version`；能力由 `extension` 推导；
- 路由动作归因与动态库共用同一套 `RouteAuditEntry`；
- 启动期校验脚本声明的扩展点是否在 `LuaLoader` 声明内，且脚本提供的 provider 与
  声明一致；
- 脚本错误/超时的失败策略从 `plugin-api` 的 `fail_policy` 取，不再在 Lua 宿主里
  各写一份。

### 10.6 兼容性

- 旧 C ABI 插件：无 `abi` / `abi_version` → 默认 C ABI + v1；扩展点仍在
  `DynamicLoader` 声明内 → 行为不变。
- 旧 Lua 脚本：`extension` 字段不变；能力由扩展点推导，不需要脚本手写。
- 旧 native 配置：`NativeLoader` 从迁移起点补齐到 8/8，新增 send/route 的默认 provider；
  默认行为不变（透传），但控制台会多出两个点。
- 旧配置：引用内置 provider 的配置不变；引用不存在 provider 的配置从“运行期报错”
  变成“启动失败”，这是有意的破坏性变化。
- 旧 core + 新插件：新字段/新变体不认识 → 装载失败；不能静默降级。

## 11. 必须动 core 的改动

### 11.1 改动清单

| # | 改动 | 位置 | 为什么必须动 core |
|---|---|---|---|
| 1 | 抽出 `plugin-api`，kernel re-export | 新 crate + `kernel/src/plugin.rs` | 语义所有权从 kernel 移出；引擎调用点不变 |
| 2 | `LoaderManifest` 增加 `carrier` / `abi_channels` / `capabilities` | `kernel/src/plugin.rs` | 只有 core 能核对载体声明与实际注册 |
| 3 | `install_loader` 事务化 + 注册后一致性检查（manifest 声明 vs 实际注册） | `kernel/src/plugin.rs` | 注册表在 core；回滚只能在这里做 |
| 4 | 配置引用启动期校验 | `kernel/src/config.rs` + 启动路径 | 配置解析和 provider 解析都在 core |
| 5 | route 统一归因（决策 + 四动作，回答 provider / 动作 / 目标 / 结果） | `kernel/src/engine.rs` | 归因需要 core 的连接、房间、订阅者上下文和副作用 ID；与三载体对齐同批交付 |
| 6 | route 决策超时 / 隔离 / 动作预算 | `kernel/src/engine.rs` + `kernel/src/plugin.rs` | 执行器和引擎在 core |
| 7 | `NativeLoader` 补 Send/Route + `NativeProviderLoader` | `kernel/src/plugin.rs` | native 注册入口在 core；补齐到 8/8 |
| 8 | `DynamicLoader` 增加 route 分支 + 双通道分派 | `adapters/dynamic/src/lib.rs` | 装载、符号解析、manifest 校验在动态 loader；补齐到 8/8 |
| 9 | `Wire*` 增加 route 类型和 `abi` 字段 | `adapter-api/src/lib.rs` | wire schema 的唯一定义处 |
| 10 | `abi_stable` 投影 crate + 宿主接入 | 新 crate + `adapters/dynamic` | 跨边界入口和布局校验由宿主负责 |
| 11 | Lua manifest / 能力 / 归因对齐 | `plugins/lua/src/lib.rs` | 脚本宿主在 core 的注册体系内 |
| 12 | 控制台展示 `abi` / 声明 / 归因 | `bridge` / 控制台 API | 可观测性由 core 提供 |
| 13 | 一致性测试与 fixture | 新 fixture + CI | 跨通道测试需要真实 dlopen 两条通道 |

### 11.2 不需要动 core 的改动

- 在已经支持的扩展点内新增一个 provider 实现；
- 改插件自己的策略逻辑、配置 schema、错误文案；
- 用已有的 `options` 字段传新配置；
- 在 Lua 里新增一个脚本；
- 用 `abi_stable` 或 C ABI 写一个新插件（只要 manifest 合法）。

判断规则：**新增扩展点、能力、manifest 字段、wire 字段、归因字段，
必须动 core；其余尽量留在插件侧。**

## 12. 验收清单

**语义与 crate**

- [ ] `plugin-api` 不依赖 kernel / adapter / 运行时。
- [ ] kernel 通过 re-export 保持现有类型路径兼容。
- [ ] 8 个点的请求/结果/错误/失败策略只在 `plugin-api` 定义一次。
- [ ] 两条投影没有私有默认值。

**manifest 与启动期**

- [ ] loader manifest 校验 `carrier` / `abi_channels` / `capabilities`。
- [ ] `install_loader` 把 manifest 声明与实际注册的 descriptor 对起来；声明未注册或
      注册未声明 → 回滚 + 启动失败。
- [ ] `NativeLoader` 声明 8 个点、注册 8 个点；Send 与 Route 都已注册，
      不存在“声明了但没注册”。
- [ ] 三载体的点位数都是 8/8；任一载体缺项视为未完成。
- [ ] 插件声明的扩展点不在 loader 声明内 → 启动失败。
- [ ] 配置引用不存在的 provider → 启动失败。
- [ ] 失败退出码为 78，无半套装载。

**route 归因（与三载体对齐同批交付）**

- [ ] route 决策与四个动作各有一条归因记录，含 provider / 动作 / 目标 / 结果。
- [ ] 成功、被拒、失败、超时四种结果都落记录；`Send` 无订阅者、组装失败、relay
      失败不例外。
- [ ] `Send` / `Task` 的 `effect_ref` 能定位到出站记录、平台回执或任务 ID。
- [ ] `Interrupt` 记录作用域与受影响会话；`Task` 记录 handler 与最终状态。
- [ ] 归因只记录，不引入授权判定或新增限流。
- [ ] 归因、交叉校验、超时/隔离与三载体 8/8 同批交付；缺一不得发布。

**路由开放**

- [ ] 插件声明的 route 动作面与实际注册一致；不一致启动失败。
- [ ] `.so` 的 wire 能表达 `Pass` / `Drop` / `Handle { store, wake, actions }`。
- [ ] `.so` 的 wire 覆盖 `Wake` / `Send` / `Interrupt` / `Task` 四个动作。
- [ ] route decision 有超时；超时按 `Pass`。
- [ ] 超限 decision 整体拒绝，不做部分执行。

**一致性**

- [ ] 同一 corpus 在 C ABI 和 `abi_stable` 上结果逐字节相等。
- [ ] 错误/panic/超时/超限的失败策略一致。
- [ ] native / Lua 在可表达范围内结果一致。
- [ ] 版本不匹配在装载前拒绝。

## 13. 风险与取舍

1. **超时不等于抢占。** in-process 动态库卡死后，core 只能不再等待，不能杀死线程。
   缓解：单飞、stuck/dead 熔断、动作预算；需要更强可用性隔离时改走进程载体。
   插件本身可信，这不是安全边界问题。
2. **启动期配置校验会打破旧配置。** 引用拼错或 provider 被移除的配置会从“运行期
   报错”变成“启动失败”。这是有意的，因为静默不生效正是本次要消灭的问题。
   需要迁移说明和清晰的错误信息。
3. **`plugin-api` 抽离是一次大搬迁。** 缓解：先用 re-export 保持路径，分阶段搬，
   不一次性改引擎调用点。
4. **两条通道的字段表达能力不完全相同。** 例如 `abi_stable` 跨边界传
   `serde_json::Value` 不合适。缓解：边界上统一传 JSON 文本，语义层再解析；
   并在 corpus 里覆盖。
5. **`NativeLoader` 补齐 Send/Route 会新增默认 provider。** 默认行为是透传，不应改变
   现有结果；但控制台会多出两个点、默认路由路径多一层透传。缓解：用回归测试锁住
   “补齐前后同一输入的决策与动作不变”，并在变更说明里写清楚。
6. **归因会带来额外内存和 I/O。** 缓解：环形保留、异步写入、失败不阻断主流程；
   `effect_ref` 只存 ID，不存消息正文。
7. **归因只解决可追溯，不解决恢复。** 进程内环形记录重启即丢，不能当合规账本。
   缓解：先保证进程存活期可查、`effect_ref` 可定位；需要跨重启追溯时再把归因落持久化，
   不在本期扩展。
