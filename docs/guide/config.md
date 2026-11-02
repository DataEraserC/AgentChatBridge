# 配置手册

配置文件是 `config.toml`（模板见 `config.example.toml`），每份配置在示例里都有带注释的完整写法。
本页按「改得多的 → 改得少的」排序讲。

::: tip 不想手改 TOML？
绝大多数配置（连接/会话/人格/记忆/订阅/唤醒策略）都可以在[网页控制台](./console)里用表单增删改，
保存即热重载，注释和排版不会丢。TOML 手改只是兜底。
:::

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
kind = "onebot11"
transport = "ws-client"    # 桥主动连平台的 WS 服务端
url = "ws://127.0.0.1:3001"
token = ""                 # OneBot 的 access token，没设就空着
account = "10001"          # 这个号自己的 QQ 号——桥靠它认出「哪句是 agent 自己说的」
admin_qqs = []             # 这些号在记录里标成 system_admin；只影响 sender.role
allow_destructive = false  # 见下
```

- `account` 一定要填对：agent 自己发的消息不计入未读，靠的就是它。
- `allow_destructive` 默认 `false`（踢人/禁言/撤回别人消息这些破坏性操作模型调不到）。
  想让 agent 真能管理群就写 `true`，或者只放行几个：`allow_destructive = ["group.kick"]`。
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

## 插件：适配器与策略放到 core 外

```toml
# 动态库平台适配器
[[plugins]]
path = "/绝对路径/libdemo_adapter.so"
kind = "demo"        # 可选；必须和插件 manifest 的 kind 一致

# 动态库策略 provider（扩展点由插件 manifest 声明）
[[plugins]]
path = "/绝对路径/libacb_example_wake.so"
```

- Lua 脚本不走 `[[plugins]]`：把环境变量 `ACB_LUA_DIR` 指向脚本目录，每个 `.lua` 文件
  自动成为一个 provider（文件名是 `lua.<文件名>`）。
- 策略的专属参数写在对应的 `.options`（Lua 里是 `options.extra`）；适配器的参数写在
  连接的 `options`。扩展点、manifest、生命周期和 wire 形状见[插件开发](../08-plugin-development.md)。

## 会话与订阅：谁在哪个群、怎么醒

```toml
[[sessions]]
id = "main"
harness = "dsh"              # 必须是上面 [[harnesses]] 里定义过的 id
persona = "default"          # 必须是定义过的人格卡
memories = ["main-memory"]   # 可空；多个就是多个 git 仓库
model = ""                   # 可空 = harness 侧自己决定用哪个模型
token = "……"                 # harness 连桥时报这个令牌绑定会话
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
  例如 `provider = "lua.wake_policy"` 时写 `at_mention` / `poke` / `reply` /
  `accumulate` / `idle_hours`，见
  [`examples/lua/wake_policy.lua`](../../examples/lua/wake_policy.lua) 和
  [插件开发](../08-plugin-development.md)。
- **唤醒配置是按房间存的**：同一个会话订阅 QQ 房间和 Discord 房间时，QQ 房间写
  `events = ["qq.poke"]`，Discord 房间写 `events = ["discord.reaction_add"]`；带前缀的事件
  配错平台会在启动时直接报错，不会「配了却永远不生效」。
- **`event_tags` 是跨平台维度**：适配器把平台事件翻译成语义标签（`poke`、`reaction`、
  `notice` …），换个平台只要标签名一样，策略照旧生效。`events` 里也可以写 `qq.*` 匹配该平台全部事件。
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
  `last_record_at`；参考 [`examples/lua/three_hour_clock.lua`](../../examples/lua/three_hour_clock.lua)。
- 时钟只决定「开不开轮」，不决定叫不叫醒具体哪条消息。

### 访问策略：能不能去订阅外的房间

默认只能读写自己订阅的房间。要放宽，给会话加一段（ADR-0026）：

```toml
[sessions.access]
provider = "acb.access.subscribed"   # 默认：只认订阅
# provider = "acb.access.all_rooms"        # 整个会话豁免：任何已配连接的房间都能碰
# provider = "acb.access.elevated_in_rooms" # 指定房间豁免（用下面的 rooms）
# rooms = ["qq:group/123"]                  # 只有 elevated 用；这些房间不用订阅也能读写
# provider = "acb.access.privileged_origins" # 按来源房间提权（用下面的 origins）
# origins = ["qq:private/123"]               # 从这些房间触发的轮次可访问任意房间；其它来源按订阅
```

- 豁免只改变**工具能碰到哪里**，不改变唤醒：没订阅的房间不会因为它来消息就开轮。
- `room` 是这次要访问的**目标**房间；策略还能看到本轮是从哪个房间触发的
  （Lua / 动态库里的 `origin_room`）。内置的 `privileged_origins` 用 `origins` 列出来源；
  其它内置策略只看目标房间，不区分来源。
- 破坏性操作仍由连接的 `allow_destructive` 单独把关，访问策略不能顶替它。
- `all_rooms` 也只能落在**配置里存在的连接**上——连接本身没配登录账号，发不出去。

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

## dsh 侧的环境变量

`deploy/start-drivers.sh` 里给驱动设了两个环境变量，控制 dsh 网页里的工作目录和分组：

- `ACB_CWD`：会话的工作目录（默认仓库下的 `state/agents`）。改了它，dsh 里旧的
  同名会话会报「belongs to」冲突——驱动会自动换个带代次的新 id 建会话，旧的作废；
- `ACB_WORKSPACE_TITLE`：dsh 里新建工作区的显示名（默认「ACB」）。

::: warning dsh 工具清单是启动快照
给 agent 用的工具定义改了之后，要重启 dsh 才会进模型上下文。
:::
