# 配置手册

配置文件是 `config.toml`（模板见 `config.example.toml`），每份配置在示例里都有带注释的完整写法。
本页按「改得多的 → 改得少的」排序讲。

::: tip 不想手改 TOML？
绝大多数配置（连接/会话/人格/记忆/订阅/唤醒策略）都可以在[网页控制台](./console)里用表单增删改，
保存即热重载，注释和排版不会丢。TOML 手改只是兜底。
:::

## 密钥来源（[secrets]）

配置里**不写密钥本体，也不做文本插值**。先声明来源，再在字段里引用来源名：

```toml
[secrets]
default_kind = "env"

[[secrets.sources]]
name = "console-token"
kind = "env"
variable = "ACB_CONSOLE_TOKEN"

[[secrets.sources]]
name = "main-session-token"
kind = "file"
path = "/run/agenix/acb-dsh-main.token"
format = "raw"

[console]
token = { secret = "console-token" }

[[sessions]]
id = "main"
harness = "dsh"
persona = "default"
token = { secret = "main-session-token" }
```

规则：

- 密钥字段（连接 / 会话 / 控制台 / 通知的 token，通知 topic）只接受
  `{ secret = "<来源名>" }` 或省略。写普通字符串会被当成字面量密钥，启动直接失败。
- `kind = "env"`：读进程环境变量（`variable` 必填）；无 systemd 且用 `env -i`
  白名单时，未传进来的变量会在启动期明确报错，不会静默变成空串。
- `kind = "file"`：桥自己读文件，`path` **必须是绝对路径**。`format = "raw"`
  （默认）表示整个文件就是一个值；`format = "dotenv"` 逐行读 `KEY=VALUE`，
  忽略空行和 `#` 开头的整行注释，用 `key` 选键。dotenv 是严格子集：不处理引号、
  不做续行、不做变量展开。
- 文件必须是普通文件，权限要求 group/other 无任何权限（如 `0600`），属主是当前
  用户或 root；否则启动失败。
- 加载期一次性解析**所有**声明的来源；任一缺失、不可读、权限不合规、值为空、
  引用未声明，都算配置错误，桥以 78 退出（systemd 不重启）。
- 同名来源全局唯一；`[[secrets.sources]]` 可以分散在 base 和普通分片，加载时合并。
  `90-console.toml` 不允许声明 `[secrets]`，控制台只引用已有来源。
- dsh / opencode 等子进程直接消费的 API key 不在这里，仍走它们自己的环境。

这些值只在启动时解析；改密钥文件或环境仍要重启桥。

## 配置分片（config.d）

运行期策略（`[grants]`、`admin_qqs`、订阅等）可以放在不进仓库的分片文件里，base 只留公共结构。

加载入口：

- `--config` 仍是 base（必填，默认 `config.toml`）。
- `--config-dir <dir>` 指定分片目录；不写时取 **base 配置所在目录**下的 `config.d/`。
- 目录不存在 = 未启用分片，行为和旧版完全一致。
- 目录存在时只加载 `*.toml`，按文件名字典序排序；子目录和非 `.toml` 文件忽略。
- base 和每个分片都不做文本插值；密钥字段用 `{ secret = "<来源名>" }` 引用上面的 `[secrets]`。

合并规则只有三句（外加一个密钥来源特例）：

- **数组追加**（base 在前，分片按序在后）。
- **标量与表：后出现者覆盖先出现者**。
- **带 key 的表按键覆盖**（例如 `[grants.rooms."<room>"]` / `[grants.accounts."<account>"]`，分片只写自己想加的那一条）。
  `[[secrets.sources]]` 是例外：跨分片追加，同名来源重复即启动失败。

细节：

- `[[connections]]` / `[[sessions]]` 按 `id` 全局唯一；跨文件重复 id 直接启动失败，错误会同时指出两个来源文件。
- `[[grants.rules]]` 追加、不去重；`admin_qqs` 这类只增不减的列表追加后按值去重。
- 任何分片解析失败（语法、未知字段、缺必填字段）都会启动失败，错误带文件路径。
- 启动日志会按顺序列出每个配置文件及其贡献的 connection / session / rule 数，最后给出生效 rule 总数；`--check` 的摘要和 `/api/status` 的 `config_fragments` / `rules` 字段也能看到。

### 控制台保存

控制台的保存目标固定为 `<config_dir>/90-console.toml`，不再改写 `--config` 指向的文件；
`config.d/` 不存在时会自动创建，旧覆盖层备份成 `90-console.toml.bak`。

`90-console.toml` 是**覆盖层（overlay layer）**，与普通分片不同：

- 它按**顶层 key 整体替换**下层（不做按 key 合并，也不做数组追加）。
- 它**豁免** `connections` / `sessions` 的跨文件重复 id 检查。必须豁免，否则控制台
  保存一份包含已有条目的完整文档时，会立刻和 base/分片里的同名 id 冲突，控制台就
  无法编辑已有 connection/session。
- 它最后加载，所以部署侧重新生成 base 不会冲掉控制台改动。

runtime 策略分片仍按上面的普通合并规则处理。手工排查时可以直接看 `config.d/`：
`90-console.toml` 是控制台写的，其余文件是部署/运维写的。

### 已知限制

- 分片**无法给已有 connection 追加 `admin_qqs`**：`[[connections]]` 按 id 全局唯一，
  重复 id 会启动失败；只能在定义该连接的文件里维护它的 `admin_qqs`。
- `[[grants.rules]]` 追加但**不去重**：同一规则在两层各写一份会得到两条。启动摘要会
  对完全相同的跨层规则提示一次，但不会降级为错误，也不会自动去重。
- `/api/status` 的 `config_fragments` 来源列表是**启动时**加载的文件；控制台保存产生/
  更新 `90-console.toml` 后，要重启桥才会出现在这个列表里（`rules` 取的是当前暂存配置）。

## 热重载：改了什么时候生效

大部分改动在**当前这轮对话结束后的下一轮**生效——正在进行的对话不会被腰斩，改完不用重启：
会话/订阅/唤醒、人格、记忆挂载、保留策略、`delivery_limit`、`default_on_turn_error`、
`turn_timeout_secs`，以及 `file_relay.public_base` / `dir`。

「启动时才知道」的参数改完必须重启桥：`[endpoint]`、`[console]`、`[[connections]]`
（连接守护线程持有旧连接信息）、`storage.path`、`worktrees_root`、`[[plugins]]` 的增删
（动态库在启动时加载且不支持 unload），以及 `file_relay.listen`（增删 `[file_relay]`
也算）。控制台保存后会把这些列成「需要重启」项。

## 连接：一个账号一条

```toml
[[connections]]
id = "qq"                  # 房间名的前缀就是它
platform = "qq"
kind = "onebot11"          # 规范值；也可写 "onebot-snowluma"（等价别名）
transport = "ws-client"    # ws-client 桥主动连平台；ws-server 桥监听、平台连入
url = "ws://127.0.0.1:3001"  # ws-client 必填；ws-server 用 options.listen
token = { secret = "qq-token" }  # 可选；引用 [secrets] 里声明的来源，不需要就删掉
account = "10001"          # 这个号自己的 QQ 号——桥靠它认出「哪句是 agent 自己说的」
admin_qqs = []             # 这些号在记录里标成 system_admin；lua.stop_command 据此放行 /stop
```

- `account` 一定要填对：agent 自己发的消息不计入未读，靠的就是它。
- **风险等级不参与鉴权**：`Read/Write/Destructive/Critical` 只是操作属性，决定默认查哪个
  capability。默认 `base:subscription` 只授 read/write，所以 destructive / critical
  默认落回票据 + 审批；要直调必须由 `[grants]` 规则显式授予对应 capability。
- **旧键 `allow_destructive` 已移除**：`true` / `false` / 方法名数组都会直接启动失败，
  报错给出可抄的 `[[grants.rules]]` 写法。
  - `allow_destructive = true` → 给对应 subject/place 的规则加 `capability = ["destructive"]`
    （见下面「访问授权」的完整示例）。
  - 方法名数组 → 每条方法一条 `scope = "platform_op"` 规则，
    `target = { kind = "platform_op", id = "member.kick", sub = "qq" }`。
  - `allow_destructive = false` → 删掉该行；默认就是票据 + 审批。
- `[connections.policy.risk]` 也已整体移除：出现即启动失败，直接删掉。
- 默认下破坏性操作直调会被拒，但 `access_request` 仍可发起：生成票据、私信审批后由
  core 执行。规则显式授予 `critical` 后 critical 也可直调，但强烈不建议。
- OneBot 连接会带 `options`；`options.snowluma_min_version` 可覆盖适配器默认的 SnowLuma
  兼容下限（默认 `2.4.0`）。低于下限只在启动日志警告，不拒绝连接：
  在 `[[connections]]` 里写 `options = { snowluma_min_version = "2.4.0" }`。
- 可选 `options.http_listen`：额外监听一个 HTTP 端点接收 SnowLuma 的 `httpClients`
  事件（动作仍走 WS）。必须同时配置 `token`（HMAC-SHA1 密钥），请求要带
  `X-Signature: sha1=...`；没有 token 会启动失败。
- `wss://` 部署前提：适配器只接受 `ws://`；需要 TLS 时用反向代理终止 TLS 后转发到
  桥监听的 `ws://` 地址。
- 连接成功后适配器会自动调 `get_login_info` / `get_version_info` / `get_status`；
  三层版本与 `compat.state` 可在 `/healthz` 的 `dependencies.items[].version` 和
  `/api/plugins.connections[].version` 看到。
- 同一个平台第二个号：再复制一段，`id` 换名（如 `qq2`）。同一个群在两个号眼里是两个房间，各记各的账。

### 接收策略：消息进不进流水

接收策略挂在**连接**上，对这一条连接的所有会话生效：

```toml
[connections.receive]
provider = "acb.receive.all"   # 默认：全收
# 插件专属参数；内置策略忽略它
# [connections.receive.options]
# blocked = ["机器人号"]
```

- 返回 `drop` 的记录**不写流水、不叫醒、不占水位**；桥就当它从没来过。
- 适合丢机器人、丢刷屏号；示例见 `examples/lua/drop_noise.lua`。
- 网关层（`receive`）是脏数据的第一道门；别拿它替代访问策略或唤醒策略。

### 消息路由：命令和链接交给代码

路由在**入库前**执行，按配置顺序一条条问；前一个返回 `pass` 才轮到下一个，返回
`drop` / `handle` 就停下。它挂在连接上：

```toml
# 管理员 /stop：打断当前房间正在跑的轮次
[[connections.routes]]
provider = "lua.stop_command"
[connections.routes.options]
commands = ["/stop"]
scope = "room"          # room / connection
admin_only = true

# 管理员 /send：直接由插件发出去，不叫醒 AI
[[connections.routes]]
provider = "lua.send_command"
[connections.routes.options]
command = "/send"

# b23.tv / bilibili 链接：后台 yt-dlp 下载，完成后发回原房间
[[connections.routes]]
provider = "lua.b23_video"
[connections.routes.options]
dir = "/tmp/acb-b23"
timeout_ms = 600000
```

- `handle` 可以指定 `store`（命令写不写流水，默认不写）和 `wake`（要不要让 AI 也看到，
  默认不看）。`store=true, wake=false` 的记录会标成 suppressed：归档备查，不算未读。
- 动作：`interrupt`（room / sessions / connection）、`send`（文本或 elements）、
  `task`（后台任务，目前内置 `exec`，跑一条命令并把产物发回来）。
- 路由 provider 只在进程内跑（Lua / native）；路由回调必须快（Lua 上限 50ms），
  下载/上传这类慢活放进 `task`，由后台 worker 执行。
- 路由插件能发消息、打断轮次、跑任务，属于高权限代码；只挂你信任的脚本。

## 插件：适配器与策略放到 core 外

```toml
# 动态库平台适配器
[[plugins]]
path = "/绝对路径/libdemo_adapter.so"
kind = "demo"        # 可选；必须和插件 manifest 的 kind 一致
timeout_ms = 5000    # 可选；单次发送/调用超时（默认 5000，最小 100，最大 60000）

# 动态库策略 provider（扩展点由插件 manifest 声明）
[[plugins]]
path = "/绝对路径/libacb_example_wake.so"
```

- `timeout_ms` 只对平台适配器生效。超时后该连接句柄标死：后续发送/调用立即失败，
  catalog 里显示 `Unhealthy`；重新创建该连接的适配器实例（新句柄）后自动恢复，
  不需要重启进程。卡住的插件线程无法强制中断，会残留到进程退出。

- Lua 脚本不走 `[[plugins]]`：把环境变量 `ACB_LUA_DIR` 指向脚本目录，每个 `.lua` 文件
  自动成为一个 provider（文件名是 `lua.<文件名>`）。
- 策略的专属参数写在对应的 `.options`（Lua 里是 `options.extra`）；适配器的参数写在
  连接的 `options`。扩展点、manifest、生命周期和 wire 形状见[插件开发](../08-plugin-development.md)。

### 外置进程策略 provider（WASM 载体）

core 不链接任何 wasm 运行时。WASM 插件由一个独立宿主进程加载，桥只按命令启动它
并通过 stdio 行 JSON 转发 `decide`：

```toml
[[process_providers]]
id = "demo.wasm_access"          # provider id；namespace 不能是 acb
extension = "access"             # 当前只支持 access
command = ["/绝对路径/acb-wasm-host", "--module", "/绝对路径/access.wasm", "--provider", "demo.wasm_access"]
startup_timeout_secs = 10        # 等第一行 manifest 的上限
timeout_ms = 5000                # 单次 decide 的上限
# env = { RUST_BACKTRACE = "0" }
# cwd = "/绝对路径"
```

会话通过 `[sessions.access] provider = "demo.wasm_access"` 使用它。列表为空 =
不启动任何宿主；宿主起不来或 manifest 不合法会在启动期直接失败（退出 78），
不会带病运行。运行期 trap / 超时会把 provider 标成 `Unhealthy` 并对后续请求
返回 `deny`，不会自动重启。设计与失败映射见 [wasm-carriers](../plan/wasm-carriers.md)。

参考 `.so` 载体走现有 `[[plugins]]`，要求 ABI v2 + `UNLOAD_SAFE`；它与 WASM 载体
共享同一份 access 决策契约。构建与测试命令：

```bash
cargo build -p acb-wasm-host --features wasm-host --bin acb-wasm-host
cargo test -p acb-wasm-host --features native-so
```

## 会话与订阅：谁在哪个群、怎么醒

```toml
[[sessions]]
id = "main"
harness = "dsh"              # 必须是上面 [[harnesses]] 里定义过的 id
persona = "default"          # 必须是定义过的人格卡
memories = ["main-memory"]   # 可空；多个就是多个 git 仓库
model = "provider/model"     # 可空 = harness 侧自己决定；写成 provider/model（第一个 / 分隔）
token = { secret = "main-session-token" }  # harness 连桥时报这个令牌绑定会话
on_turn_error = []           # 一轮失败时做什么（log / send:<房间>）；空 = 回退全局默认

[[sessions.subscriptions]]
connection = "qq"
room = "group/123456789"     # 私聊是 private/对方QQ号

[sessions.subscriptions.wake]
provider = "acb.wake.default"  # 唤醒策略实现；不写就是内置（ADR-0026）
at_mention = true      # 被 @ 醒
reply = true           # 被回复（引用我发的消息）醒
name_mention = true    # 被叫名字醒
names = ["小助手"]      # name_mention 开了才看这个列表
accumulate = 5         # 都没触发时，攒满 5 条新消息也醒
event_tags = []        # 跨平台语义标签，命中就醒（如 ["reaction"]）
events = ["qq.poke"]   # 平台事件名；带平台前缀的必须属于这个房间的连接平台
ignore_event_tags = [] # 命中这些标签的事件不叫醒（旧 poke = false 等价于 ["poke"]）
poke = true            # 旧语法糖：true 等同默认（冲我来的事件醒），false 转成上面忽略 poke
ignore_speakers = []   # 这些人的发言不叫醒（但照样记账）
```

要点：

- **上面这些是内置唤醒策略的字段**。如果 `provider` 换成 Lua / 动态库，参数写在
  `[sessions.subscriptions.wake.options]`，形状由插件自己定义（Lua 里是 `options.extra`）。
  例如 `provider = "lua.wake_policy"` 时写 `at_mention` / `poke` /
  `empty_body_hold_ms` / `accumulate` / `idle_hours`；`reply` 是例外，示例脚本读的
  是标准字段 `wake.reply`，和内置策略一致（表情回应也走这个开关），见
  [`examples/lua/wake_policy.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/wake_policy.lua) 和
  [插件开发](../08-plugin-development.md)。
- **唤醒配置是按房间存的**：同一个会话订阅 QQ 房间和 Discord 房间时，QQ 房间写
  `events = ["qq.poke"]`，Discord 房间写 `events = ["discord.reaction_add"]`；带前缀的事件
  配错平台会在启动时直接报错，不会「配了却永远不生效」。
- **`event_tags` 是跨平台维度**：适配器把平台事件翻译成语义标签（`poke`、`reaction`、
  `notice` …），换个平台只要标签名一样，策略照旧生效。`events` 里也可以写 `qq.*` 匹配该平台全部事件。
- Lua 示例的 `empty_body_hold_ms`（默认 5000）：裸 @ / 裸引用 / poke 没有正文时，
  先等这么久再开轮；等待期间到达的新消息会一起投递。带正文的 @ 不受影响。
- **私聊建议只留 `at_mention`/`reply`**（示例配置就是这么写的），不然对方每句话都叫醒；
- `accumulate` 调小反应快但费 token，调大省钱但「攒话」；
- 两个会话订同一个群完全合法（比如一个主会话 + 一个备用），各自的水位互不干扰。

### 时钟：没人说话时也要检查

时钟按**订阅**配置；默认关闭：

```toml
[sessions.subscriptions.clock]
provider = "acb.clock.off"   # 默认：不开例行检查

# 内置固定间隔 / 房间空闲：任一命中就开一轮
# provider = "acb.clock.interval_idle"
# interval_secs = 3600       # 距上次时钟轮满 1 小时
# idle_secs = 7200           # 或者房间 2 小时没新记录
```

- 时钟轮也走正常投递；没有未读时 `records` 为空，模型可以主动说话（也可以选择不说）。
- 做「多久没醒就醒一次」时，Lua 的 clock 能拿到 `last_wake_at` / `last_clock_at` /
  `last_record_at`；参考 [`examples/lua/three_hour_clock.lua`](https://github.com/DataEraserC/AgentChatBridge/blob/dev_20261025/examples/lua/three_hour_clock.lua)。
- 时钟只决定「开不开轮」，不决定叫不叫醒具体哪条消息。

### 访问策略：能不能去订阅外的房间

默认只能读写自己订阅的房间。要放宽，用 `[grants]` 规则（设计见 [access-grants](../plan/access-grants.md)）：

```toml
[grants.system_admin]
connection = "qq"
account = "10002"

[[grants.rules]]
subject = "system_admin"
place = "private"
scope = "subscribed"
capability = ["room_read", "room_write", "connection_read"]
```

- `scope = "subscribed"` 只覆盖本会话可达的 kind / 目标，不是无条件全开；跨目标要显式写
  `scope = "room"` / `"connection"` / `"platform_op"` 并给 typed target：
  `target = { kind = "room", id = "qq:group/123" }`、
  `target = { kind = "connection", id = "qq" }` 或
  `target = { kind = "platform_op", id = "group.mute", sub = "qq" }`。
  旧字符串 target 会在启动期报错。
- 能力词表：`room_read` / `room_write` / `connection_read` / `platform` / `destructive` /
  `critical` / `access_request` / `account_read` / `plugin`。`account_secret` 不可常驻授予，
  只能走票据；`critical` 可被规则显式授予（写了就直调），但强烈不建议。
- 想让 agent 直调破坏性方法，必须显式授予 `destructive`。完整示例（群内普通用户）：

```toml
[[grants.rules]]
subject = "user"
place = "group"
scope = "connection"
target = { kind = "connection", id = "qq" }
capability = ["room_write", "platform", "destructive"]
```

- 只放行单条方法（等价于旧的 `allow_destructive = ["group.mute"]`）：

```toml
[[grants.rules]]
subject = "user"
place = "group"
scope = "platform_op"
target = { kind = "platform_op", id = "group.mute", sub = "qq" }
capability = ["room_write", "platform", "destructive"]
```

- 规则命中但没列对应 capability 时按 deny 处理（fail closed）；默认没有规则时
  destructive / critical 不直调，改走票据。
- `[sessions.access]` 只用于接入外部 access provider；不写就由内置 `[grants]` 评估器判定。
- 旧键 `all_rooms` / `elevated_in_rooms` / `privileged_origins`（以及 `[sessions.access]`
  的 `rooms` / `origins`）已移除；配置里再出现会**直接启动失败**，错误会明确指向
  `[grants]`，不会静默忽略。
- 授予只改变**工具能碰到哪里**，不改变唤醒：没订阅的房间不会因为它来消息就开轮。
- 群内临时越权用 `access_request { items }`：一条 = 单张，多条 = 批量（每张独立票据、共享 batch_id）；
  审批消息发到 system admin 私信，管理员用 `/approve <id> <nonce>`、`/deny <id> <nonce>`、
  `/approve <batch_id>` 或 `/revoke <id>` 处理。票据 2 分钟内单次可用，重启后全部失效。
  批准后 core 按申请时保存的 `params` 快照执行 `send_message` / `send_message_to` / `platform_call`，
  只读 op 的票据保持 `active` 由会话在有效期内重试。执行结果以系统事件 `ticket_result`
  写回原房间，完整结果用 `access_ticket_get` 查。
  `[tickets]` 可配申请冷却与管理员消息冷却（默认 60s / 30s）。`access_ticket_get` 只能查本会话的票据。
- 未验证（unverified）操作是硬不变量：不论规则怎么授予，直调一律拒绝，只能票据 + 审批。
- `scope = "any"` 已移除（与 `subscribed` 语义相同）：配置里再写会在启动解析期报错，
  提示改用 `subscribed`，不静默映射。
- 外部 provider 只能落在**配置里存在的连接**上——连接本身没配登录账号，发不出去。

### 投递、发送钩子与失败通知

这三类策略都可以换成插件（内置 / Lua / 动态库，ADR-0026 / ADR-0027）：

```toml
# 一轮投哪些记录：只决定编号，不碰水位和 ack
[sessions.delivery]
provider = "acb.delivery.default"
# 插件专属参数；内置 composer 忽略它
# [sessions.delivery.options]
# keep = 20

# 发送前/后钩子：before 可以改写正文，after 只观察回执
[sessions.outbound]
before = []
after = []

# 一轮失败后的通知动作：log / send:<房间>
[sessions.turn_error]
provider = "acb.turn_error.default"
# [sessions.turn_error.options]
# only = "log"
```

- delivery 返回的未知编号会被 core 忽略；中间跳过的段用 `omitted` 告诉模型「还有 N 条」。
  插件报错时回退内置「最旧一小段 + 最新一批」，一轮不会空手开。
- `before` 按列表顺序执行，任一钩子报错就让这次发送失败；`after` 不影响已经发出去的消息。
- turn_error 的 provider 只挑动作，不直接发消息；空列表 = 什么都不做。报错时回退
  会话 `on_turn_error` / 全局 `default_on_turn_error`。
- `.options` 是自由表：wake / access / delivery / turn_error / clock / receive 都生效。
  Lua 的 `turn_error` 写法是 `decide(request, options)`；旧的 `decide(request)` 仍然可用。

可直接照抄的 Lua 示例：`examples/lua/recent_only.lua`（delivery）、
`examples/lua/signature.lua`（send）、`examples/lua/timeout_log_only.lua`（turn_error）。

## 人格与记忆

```toml
[[personas]]
id = "default"
name = "默认人格"
text = """
只写「怎么说话」和「信息边界」。
不要写「你没有 bash」这类能力声明——能力由会话和 harness 决定。
"""

[[memories]]
id = "main-memory"
name = "主记忆"
kind = "git"                                  # 目前只有 git
location = "/home/you/qqab-memories/main"     # 一个 git 仓库路径
```

记忆是给 harness 用的 git 仓库：桥只负责把每个「会话 × 记忆」的 worktree 建到
`state/worktrees/<记忆id>/<会话id>` 下，之后的文件读写、提交全是 agent 自己的事。

## 文件中继：发图发文件的关键

agent 发图时，桥把本机文件收下来、换成 URL 给平台下载。**`public_base` 必须填「平台那边」
能访问到桥的地址**，这是新装最容易翻车的配置。

```toml
[file_relay]
mode = "file-server"                                  # 目前只有这一种
listen = "0.0.0.0:3131"                               # 平台要能连到这个地址
public_base = "http://host.containers.internal:3131"  # 平台那边访问桥用的地址
dir = "state/relay"                                   # 不写就用 state/relay
```

按你的 OneBot 部署方式选 `public_base`：

| OneBot 跑在哪 | `public_base` |
|---|---|
| podman 容器（同机） | `http://host.containers.internal:3131` |
| docker 容器（同机，原生 Linux） | `http://host.docker.internal:3131`（compose 里要加 `extra_hosts: ["host.docker.internal:host-gateway"]`） |
| Docker Desktop | `http://host.docker.internal:3131` |
| 本机裸跑 | `http://127.0.0.1:3131` |
| 另一台机器 | `http://<桥机IP>:3131`（listen 要 `0.0.0.0`，防火墙放行 3131） |

完整说明（含每个场景的坑和验证命令）见 `deploy/relay-examples.md`。

## 其他全局参数

**注意：下面这几项是顶层键，不要写在 `[storage]` 表头下面。** `[storage]` 只装 `path`；
`worktrees_root` / `delivery_limit` / `default_on_turn_error` / `turn_timeout_secs`
都写在表外。

```toml
worktrees_root = "state/worktrees"   # 每个「会话 × 记忆」的 worktree 放哪
delivery_limit = 30                  # 一轮最多投多少条；0 = 不截断
default_on_turn_error = []           # 会话没配 on_turn_error 时的默认动作：log / send:<房间>
turn_timeout_secs = 0                # 默认关闭；写 n > 0 才启用 n 秒超时

[retention]
max_records_per_room = 5000   # 每个房间最多留多少条已读记录
max_age_days = 90             # 已读记录最多留多少天
```

只清理**已读**的记录，任何人还没读到的消息一条不删。两项写 0 = 永不清理。

`turn_timeout_secs` 默认**不自动中断**，模型想跑多久跑多久。只有显式写了正数时，
一轮对话超过这个时长才会被 agent 侧主动打断（dsh 支持打断），打断算这一轮失败、
照常走失败通知。需要限制长任务时再打开，例如 `turn_timeout_secs = 3600`。

## 记忆管家：后台同步

`[memory_steward]` 把「会话 × 记忆」的 git worktree 同步从对话轮次里拆出去：
后台线程按调度器做 `fetch → merge/rebase → push`，失败只记状态并通知会话。
默认关闭；详细设计见 [memory-steward.md](../plan/memory-steward.md)。

```toml
[memory_steward]
enabled = true
interval_secs = 300       # 时间触发；0 = 关闭
commit_threshold = 5      # 本地新增提交数触发；0 = 关闭（两者至少开一个）
poll_secs = 30            # 后台检查间隔，不是同步周期
merge_strategy = "merge"  # merge | rebase
auto_resolve = "none"     # none：冲突停下并列文件；local / remote 会按文件取一侧、丢弃另一侧
state_dir = "state/memory-steward"   # 合并演练写在 state_dir/memory-merge/ 下
git_timeout_secs = 120
```

- 时间与提交量两个触发器可以组合，**先到者触发**。
- 默认 `auto_resolve = "none"`：冲突时 abort 合并、列出冲突文件、通知会话，
  不会静默取一侧。`local` / `remote` 是按文件整体取一侧，会丢弃另一侧在冲突文件
  里的改动，默认关闭就是因为会丢数据。
- 合并演练在 `state_dir/memory-merge/<记忆>/<会话>` 的临时 worktree 里做，主工作区
  只在最后「检查干净 + 落地」时持锁；演练期间外部写主工作区会被检测为
  `concurrent_write` 并放弃本次落地，不覆盖别人刚写的内容。落地 `reset --hard`
  的前提是主工作区无人写；锁内二次比对只缩小窗口，不是绝对保证。
- 连续 5 次 `dirty` / `concurrent_write` 会在 `status.json` 里标 `stuck=true`，
  控制台「概览」顶部显示「该记忆长期未同步：工作区持续脏」告警。
- 状态写在 `state_dir/status.json`，控制台 `GET /api/memory-steward` 只读暴露；
  字段包括最近一次同步时间、结果、冲突文件数与失败原因。
- 远端就是普通的 `origin`；没有远端、worktree 尚不存在都只记状态/跳过，不影响轮次。

## dsh 侧的环境变量

`deploy/start-drivers.sh` 里给驱动设了两个环境变量，控制 dsh 网页里的工作目录和分组：

- `ACB_CWD`：会话的工作目录（默认仓库下的 `state/agents`）。改了它，dsh 里旧的
  同名会话会报「belongs to」冲突——驱动会自动换个带代次的新 id 建会话，旧的作废；
- `ACB_WORKSPACE_TITLE`：dsh 里新建工作区的显示名（默认「ACB」）。

::: warning dsh 工具清单是启动快照
给 agent 用的工具定义改了之后，要重启 dsh 才会进模型上下文。
:::
