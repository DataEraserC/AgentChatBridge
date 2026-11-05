# 术语表

> 状态：草稿，等用户过目。目标：**名字本身就无歧义**——看到名字就知道是哪一侧、什么粒度。
> 第一原则：平台侧的聊天场所一律叫「房间」，agent 侧的一次处理叫「轮次」——旧文档里
> 两侧混用的「会话」已拆开。**会话（session）**这个词现在专指 acb 侧的 agent 实体（ADR-0017）。

## 核心名词

| 术语 | 英文键 | 定义 | 粒度 |
|------|--------|------|------|
| **连接** | connection | 一条通到平台的管道：适配器种类 + 连接方向 + 地址 + 令牌 + 登录账号。**一个账号一条连接**，同一平台可以有多条。标识 = 连接名（`qq`、`qq2`） | 平台侧 |
| **房间** | room | 平台上一个具体的聊天场所：QQ 群、QQ 私聊、Discord 频道、KOOK 频道。标识 = （**连接**，路径）；路径是「段/段」，第一段说场所类型，如 `group/158296748`、`guild/9/channel/7/post/8`。段由适配器决定，核心只当一串字符串 | 平台侧 |
| **账号** | account | 平台侧登录的机器人身份（QQ 号、Discord bot）。**账号是连接的属性**：一个号一条连接，订阅里不再重复写 | 平台侧 |
| **会话** | session | acb 侧的一个 agent 实体（ADR-0017）：订阅若干「连接 + 房间」，引用一个人格、若干份记忆，一把接入令牌。**串行的单位**——同一会话同一时刻只有一轮。与硬件无关，也不是平台上的任何东西 | 两侧之间 |
| **订阅** | subscription | 会话对某个房间的一条长期关注关系。一条订阅 = 一条连接 + 一个房间 + 唤醒策略（什么时候叫醒）。会话级还有访问、投递、发送钩子、失败通知、时钟等策略，都可以换成用户自己的逻辑 | 两侧之间 |
| **人格** | persona | 会话说话的性格设定文本。独立配置，被会话引用 | 会话 |
| **记忆** | memory | 独立的长期记忆仓库（git，ADR-0017），由 harness 侧持有；桥只按名字引用。**不再是串行锁**——一个会话可引用多份记忆，一份记忆可被多个会话引用；串行按会话走 | 记忆 |
| **轮次** | turn | 会话被叫醒后的一次完整处理：收未读 → agent 干活 → 回复 → 收工。**同一会话的轮次严格串行**（串行键 = 会话 id，配置里全局唯一；串行单位 = harness × 会话，ADR-0017） | 会话 |

## 消息与记录

| 术语 | 英文键 | 定义 |
|------|--------|------|
| **内容元素** | element | 消息的最小组成单位。核心定义一套**标准元素表**：text / at / quote（引用）/ image / audio / video / file / 表情。适配器对每个标准元素做最大程度适配；平台不支持时**降级**（如引用降成纯文本）或明确报错，**绝不悄悄丢弃**。平台特有内容装进不透明载荷原样透传（参考 Koishi 的 element 体系与 NoneBot 的 MessageSegment，不是 OneBot 专属概念；来源：用户裁定要跨平台中立） |
| **策略** | policy | 一段决定行为的逻辑，有内置默认实现，也可由用户替换成自己的（Lua 脚本或动态库，见 [08](08-plugin-development.md)）。接收、唤醒、访问、投递、发送钩子、失败通知、时钟都是策略 |
| **消息路由** | route | 入库前决定这条消息交给代码还是 AI：`pass` / `drop` / `handle`；`handle` 可以发消息、打断轮次或跑后台任务。挂在连接上、可挂多条，见 [ADR-0028](adr/0028-message-routing.md) |
| **流水** | transcript | 房间的完整消息日志，投没投过都在。投递不改流水，只推水位 |
| **未读** | unread | 某个**订阅**（会话 × 房间）水位之后还没确认的消息，按订阅各自计——同一房间两个会话各看各的未读，互不干扰。不单独存，是流水减水位的差，不会出现两套账对不上 |
| **水位** | watermark | 「某个会话在某个房间里处理到哪条了」的游标，粒度是**会话 × 房间**；只经 AI 显式确认（`ack_messages`）前推，未确认的留在未读、下轮重投（见 ADR-0010）。自己发的消息永不算未读 |
| **笔记** | note | 会话在某个房间里的速记事实（术语 / 成员 / 话题），按房间隔离，跨平台不串 |
| **记录标签** | labels | 桥或适配器给一条记录追加的通用标记（如 `recall`）。core 不解释含义，只负责存储、查询和透传；harness 按标签决定展示 |
| **只进流水** | suppressed | 适配器声明「这条记录不投递、不唤醒、不计未读」。标记落库，对所有订阅者生效（包括之后才加的会话） |
| **平台事件** | platform event | 房间里发生的非消息事情：拍一拍、成员变动、频道创建。默认记录进流水，只对订阅的房间记 |
| **状态库** | state store | 桥自己记得的事落在哪：一个 SQLite 文件，装流水、水位、笔记、投递次数。重启后接着用，票和排队不进这里（见 ADR-0014） |
| **保留策略** | retention | 旧流水什么时候可以删：按条数、按天数、或都不清。只削**已经读过**的那一段，未读一条不动（见 ADR-0015） |

## 机制

| 术语 | 英文键 | 定义 |
|------|--------|------|
| **唤醒策略** | wake policy | 每条订阅各自配置的「什么时候叫醒会话」。默认策略：**@ 我就醒 + 引用我的话就醒 + 叫昵称就醒 + 攒够 N 条就醒 + 冲我来的平台事件（如 poke）就醒**（来源：用户裁定）；其它事件按 `event_tags` / `events` 可选开启，`events` 带平台前缀时按房间校验归属。只管叫不叫醒，不管记不记录 |
| **访问策略** | access policy | 会话能读写哪些房间/连接。判定输入分「目标房间 `room`」和「本轮来源房间 `origin_room`」；内置 `[grants]` 评估器按主体 + 场所 + 范围 + 能力判定，旧的 `privileged_origins` / `all_rooms` 等策略已移除 |
| **来源房间** | origin_room | 当前这一轮是被哪个房间叫醒的。`send_message_to` 的目标可以不同；访问策略用它区分「私聊触发」和「群触发」 |
| **时钟策略** | clock policy | 按时间触发的评估：固定间隔或房间空闲超时后检查「有没有未处理完的、要不要主动说话」，需要就开一轮。默认关闭、按订阅开启（见 ADR-0011） |
| **适配器** | adapter | 一种平台协议的实现（OneBot SnowLuma，协议 OneBot 11；还有 Satori、将来的 Discord / KOOK 直连）。职责：把平台帧解成事件、把动作编成平台帧 |
| **harness 适配器** | harness adapter | 和上面那个不是一回事：桥**外面**那一半，活在 harness 里。干两件事——把桥的工具挂进 harness（工具面），把推来的一轮变成 harness 里的一次对话并知道什么时候结束（轮次驱动）。这两件事可以分开缺席（见 [06](06-harness-adapters.md)） |
| **harness** | harness | 承载 agent 的运行环境（DSH、WorkBuddy、Trae、Codex……）。桥不实现 agent，只接 harness |
| **接入点** | endpoint | harness 连接桥的通道（本机套接字 / TCP + 接入令牌） |
| **反馈** | feedback | agent 写给做桥的人的一条建议：正文 + 出处编号 + 自称撞了几次。桥自己补现场（会话/房间/模型/票），同标题合并累加（ADR-0021） |
| **接入令牌** | token | harness 握手时出示。配置里写同一个 token 的那几个会话由这条 harness 连接代表（Ready 帧会带回名单和已落盘的映射）；令牌对不上任何会话直接断开 |

## 工具面

来源：用户裁定——不止有 send，还要有 get 等读取类工具。下表是**现状**（ADR-0017 之后、ADR-0020～0023 之后），不再是提案。共 **29 个工具**（`TOOL_NAMES`）。

| 工具 | 参数 | 行为 |
|------|------|------|
| `send_message` | elements（或 text） | 发到**当前轮次**的房间。回执 `ok` / `platform_id` / `at` / `room`，失败直接报错；正文里夹着调用标记被截断时多一个 `trimmed` + 说明；本机文件路径被换成中继 URL 时多一个 `relayed`（ADR-0025） |
| `send_message_to` | elements, to | 发到别的房间。`to` 的默认可选目标 = **本会话订阅的房间**，越界拒绝；会话可用 `[sessions.access]` 放宽边界（ADR-0026）。跨会话发送已砍（ADR-0017） |
| `access_request` | items（target / ops / params） | 申请一次性票据。op 用具体名字（`send_message_to`、`notes:list`、`platform_call:<op>`）；批准后 core 执行写类动作，只读 op 留 active 由会话重试 |
| `access_ticket_get` | id / batch_id | 只读查询本会话发起的票据：状态、能力、执行结果与失败原因 |
| `capabilities` | scope(session/room/tool), room, name | 只读自检：工具可用性、平台 op 风险与 allow_destructive、权限边界、票据额度与可申请 op；不改变任何判定 |
| `ack_messages` | ids | 确认消息处理完毕，水位前推。**幂等**；未确认的下轮重投（ADR-0010 修订） |
| `get_history` | room, before, after, limit, columns | 查房间的流水。带 `total` / `has_more` / `next_before` / `next_after`；给了 `after` 就从旧往新翻（补看新消息），`columns` 里有 `quote_ref`（引用对到本房间编号，ADR-0024） |
| `fetch_records` | room, ids, columns | 按水位编号精确查一批（补投递被截断的部分）；这轮读到的编号可以 `ack_messages` |
| `wait_message` | room, timeout_ms, limit, columns | 在当前轮次房间里等新消息，最多 60 秒；返回并记为本轮读过，可 `ack_messages`；超时返回空。等待在 core 锁外，不挡别的会话 |
| `list_rooms` | — | 列出本会话订阅了哪些房间、各自未读多少 |
| `notes` | op(list/set/del), room, kind, key, value, label, status, limit, offset | 房间级笔记（term / member / topic）。list 不传 kind=全部，set/del 不传 kind=term；list 默认 20 条、上限 100，超 4 KiB 索引降级，可用 key 查回单条 |
| `get_platform_events` | room, limit, offset | 查平台事件，带 `total` / `has_more` |
| `platform_call` | op, params, path, tags, query, all, room, limit, offset, where, fields, order_by, group_by | 平台能力单入口：给层级 `op`（如 `msg.send`、`member.list`）执行；不给 `op` 按 `path` / 封闭 `tags` / `query` 列目录，命中的 op 带参数表与 `example`（ADR-0024）。分页与查询在桥这层做（ADR-0022 / 0023）；未知 op 直接拒绝 |
| `describe_plugins` | scope, extension | 插件/策略目录：默认本会话实际生效的 provider 和适配器，`scope=global` 看整个 Root；只读（ADR-0026 Phase 6） |
| `extensions_list` | namespace | 列动态扩展目录（仅管理员会话） |
| `extensions_read` | path, namespace | 读模块目录下的一个 `.toml` |
| `extensions_write` | path, text, namespace | 原子写模块目录下的一个 `.toml` |
| `extensions_reload` | namespace | 排队重载动态扩展 |
| `tasks` | op(list/get), status, limit, offset, id | 后台任务记录；list 带分页，超 4 KiB 索引降级 |
| `report_feedback` | text, refs, count, kind, severity, suggestion | 写一条反馈（ADR-0021）：哪一步卡住、出处编号、撞了几次 |
| `feedback` | op(list/get), status, limit, offset, id | 查本会话写过的反馈（T7c） |
| `feedback_reply` | id, text | 给一条反馈写回复 |
| `plugin_tools` | name, limit, offset | 插件工具索引：不给 name 只给摘要，给 name 返回完整 schema |
| `plugin_tool_call` | tool, arguments, scope | 调插件工具；`scope` 必须显式声明，不能默认成当前房间 |
| `memory_read` | memory, path | 读记忆 worktree 里的文件 |
| `memory_write` | memory, path, content | 写记忆 worktree 里的文件 |
| `memory_list` | memory, path, limit, offset | 列记忆目录，带分页 |
| `memory_search` | memory, path, needle, limit, offset | 搜记忆文件，带分页 |
| `memory_git` | op(status/log/diff/blame/add/commit/push/branch), memory, path, limit, message | 记忆仓库的 git 操作；`log` 默认 10 条、上限 100 |

## 旧词 → 新词对照

| 旧词 | 新词 | 为什么改 |
|------|------|----------|
| 会话（平台侧） | 房间 | 「会话」两侧混用，歧义之源 |
| 会话 / agent 侧会话 | 轮次（一次处理）/ 会话（持续实体） | 拆开两个不同粒度 |
| 设备（device）/ 设备组 | 会话（session，ADR-0017） | 「设备」像硬件；ADR-0017 把它升级成显式的会话实体，人格/记忆/订阅都独立出去被会话引用，「设备组」并入「同一令牌的一组会话」 |
| 归档 | 流水 | 「归档」像动作，「流水」像东西本身 |
| 范围（scope） | 订阅（subscription） | 「范围」看不出是有主物的；订阅一眼看出属于哪个会话 |
| ConversationKey | 房间标识（RoomId） | 跟着「房间」走 |
| 平台（当身份用） | 连接 | 平台是一类，连接是一个具体的号；房间、订阅、出站都该按连接说话 |
| 群（group 字段） | 房间 | 不再预设 QQ 语义 |

## 命名时严格遵守

1. 平台侧的东西一律不带 agent 色彩；agent 侧的东西一律不带平台色彩。
2. 每个名词定义里写清粒度（哪一侧、属于谁）。
3. 新概念入库前先过一遍本表：能不能用已有名词表达？不能才新增，并补进本表。
