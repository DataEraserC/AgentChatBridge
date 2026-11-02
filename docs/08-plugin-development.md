# 插件开发：把适配器和策略写到 core 外

这一页写给**插件开发者**：你不需要改 bridge 源码，只要按契约写一个 Lua 脚本、外置进程
或动态库，就能把平台接进来，或者替换唤醒、访问、投递、失败通知等策略。

如果你要改 bridge 本身（core、协议、存储、内置 harness 适配器），看
[架构](03-architecture.md)、[协议](04-protocol.md) 和
[harness 适配器](06-harness-adapters.md)，不要从这一页开始。

## 先选载体

| 载体 | 能做什么 | 优点 | 代价 |
|------|----------|------|------|
| Lua（`ACB_LUA_DIR`） | 七种策略扩展点 | 不用编译；脚本在桥启动时加载，改完重启桥生效 | 每次调用 50ms 上限；拿不到 Store/流水；不能写平台适配器 |
| 外置进程（`kind = "process"`） | 平台适配器 | 任何语言；崩溃隔离；可自动重启 | 只能做适配器；策略判定在 core 锁内，不开放进程载体 |
| 动态库（`[[plugins]]`） | 平台适配器 + 七种策略 | 进程内、全扩展点、C ABI + JSON wire | 与桥同进程，panic/卡死会带倒桥；不支持 unload |
| 内置 native | core 开发者使用 | 无跨进程开销 | 要改仓库代码并重新构建 |

## 扩展点一览

| `extension` | 作用 | Lua | 动态库 | 外置进程 |
|-------------|------|-----|--------|----------|
| `platform_adapter` | 连接平台、发事件、发送/调用 | 否 | 是 | 是 |
| `wake` | 一条记录要不要叫醒会话 | 是 | 是 | 否 |
| `access` | 工具能读/写/碰哪些房间 | 是 | 是 | 否 |
| `delivery` | 一轮投哪些记录编号 | 是 | 是 | 否 |
| `send` | 发送前改写正文、发送后观察回执 | 是 | 是 | 否 |
| `turn_error` | 一轮失败后执行哪些通知动作 | 是 | 是 | 否 |
| `clock` | 现在要不要开例行检查 | 是 | 是 | 否 |
| `receive` | 一条入站记录收不收 | 是 | 是 | 否 |

策略的插件专属参数统一放在配置的 `.options` 自由表里。Lua 和动态库都会收到
`options.extra`；`send` 没有 options；`turn_error` 的 Lua 写法是
`decide(request, options)`，旧的单参数 `decide(request)` 仍然兼容。

## 写一个动态库插件

动态库只导出 `acb_adapter_entry_v1`，返回一张 C 函数表：

```rust
#[unsafe(no_mangle)]
pub extern "C" fn acb_adapter_entry_v1() -> *const AdapterV1 {
    &TABLE
}
```

函数表必须包含 `manifest` / `create` / `invoke` / `destroy` / `free_string`。
所有跨 ABI 的文本都是 JSON，形状定义在 `crates/adapter-api`：

```text
core ──▶ 插件   create(WireInstanceConfig) / invoke(CoreMessage) / Shutdown
插件 ──▶ core   manifest() / invoke 返回 AdapterMessage / HostV1.submit_event
```

- `manifest` 返回 `WireManifest`：`id`、`api_version`、`extension`、`capabilities`，
  以及平台适配器需要的 `platform` / `kind` / `transports` / `events` / `event_tags` /
  `supports_at` / `supports_reply`。
- `create` 收到 `WireInstanceConfig`：适配器带连接配置，策略 provider 带扩展点和 options。
- `invoke` 收到 `CoreMessage::Decide`（策略）或 `Describe` / `Send` / `Call`（适配器），
  返回 `AdapterMessage::Reply`。
- `destroy` 释放 handle；桥退出前会先发 `Shutdown`。

第三方动态库插件会被启动审计：

- `id` 用 `namespace.name`，`acb` 是内置保留 namespace。
- `api_version` 必须等于 `PLUGIN_API_VERSION`（当前 1），C 表 `abi_version` 必须等于
  `ABI_VERSION`（当前 1）。
- `metadata.summary` 必填；`wake` 还必须声明 `effects`（至少 `may_wake` /
  `may_stay_silent`）。`options_schema` 目前只是说明，不强制校验。
- 平台适配器的 `kind` 必须和 `[[plugins]] kind` 一致（如果配置里写了）。

最小可构建示例见 [`examples/dynamic/wake-provider`](https://github.com/DataEraserC/AgentChatBridge/tree/dev_20261025/examples/dynamic/wake-provider)；
更完整的 conformance fixture 见 `crates/adapters/dynamic-fixture` 和
`crates/plugins/wake-fixture`。

## 写一个外置进程适配器

桥按连接配置启动子进程，双方在 stdin/stdout 上一行一条 JSON：

```toml
[[connections]]
id = "demo"
platform = "demo"
kind = "process"
transport = "process"
account = "bot"

[connections.options]
command = ["/path/to/adapter", "--stdio"]
# 可选：args / env / cwd / startup_timeout_secs
```

子进程的第一行必须是 `hello`：

```json
{
  "type": "hello",
  "name": "demo",
  "catalog": [],
  "info": {
    "events": ["demo.greet"],
    "event_tags": ["notice"],
    "supports_at": true,
    "supports_reply": true
  }
}
```

之后：

- 入站记录发 `{"type":"event","record":{...}}`。
- `send` / `call` 按 `id` 回 `reply`。错误用 `kind` 分类（`bad_params` / `forbidden` /
  `unavailable` …）。
- 子进程退出后自动重启，退避从 500ms 起翻倍、最多 10 秒；重启期间请求明确失败。
- 桥退出时发 `shutdown`，2 秒内不收尾就 kill。

参考实现：[`examples/process/adapter.mjs`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/process/adapter.mjs)。

### 入站 `WireRecord` 字段

适配器交给桥的每条记录都是 `WireRecord`（`crates/adapter-api`）：

| 字段 | 含义 |
|------|------|
| `room` / `kind` / `sender` / `at` / `elements` | 房间、消息或事件、发送者、时间、内容；事件还要在 `kind` 里带 `tags` / `payload` / `summary` |
| `at_self` | 是不是冲本账号来的（@ 到我、拍我） |
| `platform_id` | 平台自己的消息编号；反查引用用 |
| `reply_to_self` | 由桥判定：这条是不是引用了我发的消息。适配器填 `false` 即可，桥会用被引用记录的 `platform_id` 和发送账号反查后覆盖 |
| `target_platform_id` | 事件指向的旧记录编号（如撤回通知指向被撤回的消息） |
| `target_labels` | 给目标旧记录追加的标签（如 `["recall"]`） |
| `suppressed` | 只进流水：不投递、不唤醒、不计未读；落库后对所有订阅者生效 |

`labels` 不是适配器填的：桥在写入/追加标签时才产生，再随投递和查询返回。事件和记录
的形状细节见 [04-protocol](04-protocol.md) 与 [05-adapters](05-adapters.md)。

## 写一个 Lua 策略

文件名就是 provider 名：`recent_only.lua` → `lua.recent_only`。脚本返回一个表，
至少包含 `extension` 和该扩展点要求的函数：

```lua
return {
  extension = "delivery",
  compose = function(options, request)
    return { keep = { request.records[#request.records].id } }
  end,
}
```

各扩展点的函数名：

| 扩展点 | 函数 |
|--------|------|
| wake / access | `decide(options, context)` / `decide(_, request)` |
| delivery | `compose(options, request)` |
| send | `before(context, elements)` / `after(context, sent)` 至少一个 |
| turn_error | `decide(request, options)` |
| clock | `is_due(options, context)` |
| receive | `decide(request, options)` |
| route | `decide(options, context)`（按连接配置，可能挂多个，按顺序执行） |

access 的 `request` 字段：`session`、`connection`、`room`（这次要访问的目标房间；
连接级请求为 `nil`）、`origin_room`（当前轮次是从哪个房间触发的）、`action`
（`read` / `send` / `connection`）、`subscriptions`（本会话订阅的房间）。内置的
`acb.access.privileged_origins` 会按 `origin_room` 提权；其它内置 access 策略不读它。
Lua / 动态库也可以拿它做自己的「按来源房间授权」。

其它扩展点收到的字段：

| 扩展点 | `context` / `request` 里的字段 |
|--------|------------------------------|
| wake | `session`、`room`、`connection`、`platform`、`sender_id`、`text`、`at_self`、`reply_to_self`、`has_body`、`event`（消息为 `nil`）、`tags`、`unread`、`last_wake_at`、`now` |
| clock | `session`、`room`、`now`、`last_clock_at`、`last_wake_at`、`last_record_at`（房间没记录时为 `nil`） |
| delivery | `session`、`room`、`limit`；`records` 是投递候选数组，每条带 `id`、`room`、`at`、`at_self`、`reply_to_self`、`redelivery`、`note`、`platform_id`、`text`、`sender`（`id`/`name`/`role`）、`kind`、`event`、`elements`、`quote_ref`。完整 wire 形状（含 `labels`、`suppressed`、事件的 `tags`/`payload`/`summary`）见 [04-protocol](04-protocol.md)；Lua table 只暴露上面这些字段，动态库插件拿到的是完整 JSON |
| send | `context` 是 `session`、`room`、`origin`（`message` / `turn_failure` / `route`）；`before` 收到可改写的 `elements`，`after` 收到 `sent`（`platform_id`、`at`） |
| turn_error | `session`、`room`、`error`、`actions`、`default_actions`；动作是 `{kind="log"}` 或 `{kind="send", room="qq:group/123"}` |
| receive | `connection`、`platform`、`room`、`sender`、`kind`、`elements`、`text`、`at_self` |
| route | `connection`、`platform`、`room`、`sender`、`is_admin`、`kind`、`elements`、`text`（只拼 Text 段）、`at_self`、`reply_to_self`、`has_body`、`event`、`tags`、`sessions`（订阅这个房间的会话） |

`has_body` 是跨平台的“有没有正文”：消息里只有 `At` / `Quote` / 空白文本时为 `false`，
图片、文件、表情等为 `true`；事件由适配器在事件记录的 `body` 字段声明（拍一拍这类纯
触发事件为 `false`，入群、上传、请求等带信息的事件默认 `true`，旧记录缺省也按 `true`）。
策略可以用它判断“这只是个裸 @ / 裸引用 / poke”，返回带 `delay_ms` 的 wake 表让 core
等一会儿再开轮，等待期间的新消息会在开轮时一起投递：

```lua
return { decision = "wake", reason = "只有 @，等后续内容", delay_ms = 5000 }
```

动态库的 `WireResult::Decision` 也接受同一个 `delay_ms`（默认 0）。
`WakeContext` 和 `ClockContext` 都带 `last_wake_at`（这个会话×房间上一次因任何原因
开轮的时间）。做「多久没醒就醒一次」时，wake 端只能覆盖「有入站记录时」的判定；
没人说话也要到点开轮，必须再配一个 clock provider。参考
[`examples/lua/wake_policy.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/wake_policy.lua) 和
[`examples/lua/three_hour_clock.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/three_hour_clock.lua)。

### 路由：入库前的命令 / 链接处理

`route` 在订阅检查之后、receive/入库/唤醒之前执行。它返回：

- `"pass"`：继续下一个 route provider，最后走正常入库/唤醒。
- `"drop"`：吃掉，不写流水、不叫醒。
- 动作 table：`{ action = "interrupt" | "send" | "task" ... }`，或者 `{ store=…, wake=…, actions={…} }`。
  - `interrupt`：`scope = "room"`（默认）/ `"connection"`，或 `sessions = { "main" }`。
  - `send`：`text` 或 `elements`，可选 `room`；发出去走正常发送钩子和文件中继。
  - `task`：`handler`（目前 `exec`）和 `params`；后台 worker 执行，完成后按结果发送。
- `store` 默认 `false`；`wake` 默认 `false`。`store=true, wake=false` 的记录会标成
  suppressed：归档备查，但不算未读、不叫醒 AI。`wake=true` 隐含 `store=true`。

路由回调必须快（Lua 上限 50ms）：匹配 b23 链接只返回 `task`，真正的下载/转码在
`exec` 任务里跑，不挡入站队列。`exec` 直接跑参数数组（不经过 shell），但仍是高权限
动作——只挂你信任的脚本。示例：
[`stop_command.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/stop_command.lua)、
[`send_command.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/send_command.lua)、
[`b23_video.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/b23_video.lua)。
路由目前只支持进程内载体（Lua / native），不支持外置进程插件。

每次调用最多 50ms，超时按安全侧兜底；脚本拿不到 Store / Bridge / 配置写入。
脚本和 provider 集合在桥启动时加载：改脚本内容或增删 `.lua` 文件都要重启桥才生效。
可抄的示例在 [`examples/lua`](https://github.com/DataEraserC/AgentChatBridge/tree/dev_20261025/examples/lua)，总览见
[`examples/README.md`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/README.md)。

## 配置与装配

```toml
# 动态库策略
[[plugins]]
path = "/绝对路径/到/libacb_example_wake.so"

[sessions.subscriptions.wake]
provider = "example.wake"

[sessions.subscriptions.wake.options]
keyword = "在吗"

# 动态库平台适配器
[[plugins]]
path = "/绝对路径/到/libdemo_adapter.so"
kind = "demo"

# Lua 策略
[sessions.delivery]
provider = "lua.recent_only"

[sessions.delivery.options]
keep = 20

[sessions.outbound]
before = ["lua.signature"]

[sessions.turn_error]
provider = "lua.timeout_log_only"

[sessions.turn_error.options]
ops_room = "qq:group/ops"
```

provider id、扩展点、kind 写错会在启动时拒绝，不会等到第一轮才报。配置检查用：

```bash
acb-bridge --config config.toml --check
```

## 错误与兜底

插件出错时 core 不会静默跳过，也不会把会话带进危险状态：

| 扩展点 | 失败时的行为 |
|--------|--------------|
| wake | `stay`（不叫醒） |
| access | 拒绝 |
| receive | 照收（宁可多记一条，不误丢） |
| clock | 不触发 |
| delivery | 回退内置「最旧一小段 + 最新一批」 |
| turn_error | 回退会话动作 / 全局默认动作 |
| send.before | 本次发送失败 |
| send.after | 已发消息不受影响，只累计插件失败次数 |
| 平台适配器 | 发送/调用返回明确的不可用或平台错误 |

判定值本身不合法也按安全侧兜底（例如未知的 wake 判定按 `stay`）。插件失败次数会进
日志和 manager health，可通过 `describe_plugins` / 控制台 `/api/plugins` 查看。

## 开发与测试

```bash
# 动态库示例：独立构建
cargo build --manifest-path examples/dynamic/wake-provider/Cargo.toml

# 各载体的 conformance 测试
cargo test -p acb-adapter-process --test conformance
cargo test -p acb-adapter-dynamic-fixture --test loader
cargo test -p acb-plugin-lua --lib
cargo test -p acb-adapter-wake-fixture --test policy
```

上线前至少确认：

1. 配置 `--check` 通过；
2. 桥启动日志里没有「加载失败 / 未知 provider」；
3. `describe_plugins` 能看到插件、扩展点、metadata 和 health；
4. 真实触发一次扩展点（叫醒、丢一条、发一条、跑一次失败通知），看日志和结果。

## 权限、隔离与版本

- 插件能力由 descriptor 授予；插件拿不到 `Store` / `Bridge` / 配置写入，也不能替别的
  扩展点做决定。`effects` 是给人和 agent 看的说明，不是强制保证。
- 访问策略和连接的 `allow_destructive` 是两道独立闸门；插件不能替破坏性操作放行。
- 动态库在桥进程内，panic/死循环会影响整座桥；需要崩溃隔离就用外置进程。
- 动态库策略判定是同步调用，没有超时；把插件写成快速、无阻塞的判定函数。
- 动态库卸载不支持：换 `.so` 需要重启桥；外置进程崩溃会自动重启。
- wire 的演进是加字段/加消息，ABI 版本和 `PLUGIN_API_VERSION` 对不上会被拒绝。

## 权威定义

- wire / ABI 类型：`crates/adapter-api`
- 平台适配器与事件映射：[05-adapters.md](05-adapters.md)
- 插件宿主设计：[ADR-0026](adr/0026-plugin-host.md)
- 适配器注册与生命周期：[ADR-0027](adr/0027-platform-adapter-registration.md)
- 可运行示例：[examples](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/README.md)
