# Feedback 演进为工作项（设计稿）

状态：设计，未实现。只讨论 ACB 的 feedback，不改代码，不动 #6 / #7 的既有事项。

一句话：**core 只记「发生了什么」，工作项语义由挂载层解释。**

## 1. 现状

- 存储：SQLite `feedback` 表，schema 11。现有 8 行，全部 `open`，`replies` 全空。
- 写入口：agent 工具 `report_feedback`（用户口中的 `feedback_submit` 在本仓库就是它）。读入口 `feedback_list` / `feedback_get`，回复 `feedback_reply`。
- 字段：`id / fingerprint / title / body / suggestion / kind / severity / refs / occurrences / repeats / tool / session / harness / room / context / status / note / replies / first_at / last_at`。
- 状态：`open / acked / resolved / wontfix`。回复会把 `open` 自动推进到 `acked`；控制台可以任意改状态并写 `note`。
- 去重：`fingerprint = FNV-1a(小写标题去掉空白)`，全局唯一。同指纹合并 `repeats + 1`、`occurrences` 相加、`last_at` 刷新；`resolved` / `wontfix` 被再次命中会**静默退回 `open`**。
- 线程：`replies` 是扁平 JSON 数组，每条只有 `at / author / text / via`，没有 id、没有回复关系。
- 通知：回复会生成 `kind=feedback` 的合成事件（tag `feedback.reply`），直接唤醒原会话；带外 `acb-notifier` 目前只认依赖状态和 turn 抑制，没有 feedback 事件。
- 控制台：`/api/feedback` 列表，`POST /api/feedback/:id/status` 改状态，`POST /api/feedback/:id/reply` 回复；页面是「反馈」标签页。

三个关键缺陷：

1. 关闭态的复发没有事件、没有原因、没有操作者，只是状态被改回去。
2. 全局指纹把 A 房间和 B 房间合并后，B 房间收不到 A 房间的答复；回复还只通知当初那条记录的 `session`。
3. 扁平 `replies` 没有身份和父子关系，无法表达「提问者追问 → 处理者再答」的多轮线程。

## 2. 目标模型

### 2.1 分层：core 只有三个原语

| 原语 | core 负责 | core 不负责 |
|---|---|---|
| Record | id、内容、出处、来源、计数、去重键、当前状态指针、版本号 | 字段的业务含义 |
| Transition | 原子地改当前状态、记录操作者/入口/原因、追加事件 | 哪些状态合法、谁有权转移 |
| Timeline | 追加事件与评论，按时间可查 | 事件类型背后的工作流语义 |

工作项语义放在**挂载层**：`workflow` 定义状态集合、合法转移、属性 schema、去重终态集合、通知策略。默认实现是内置 `acb.feedback.default`；之后可以换成可配置 provider，core 不知道 `component` / `assignee` / `area` / `iteration` 这些词。

表结构：

- `feedback`：主记录。保留现有内容/出处/计数列，新增：
  - `scope_key`：去重与可见性边界，默认 `room:<room>`；没有房间时 `session:<session>`。
  - `workflow`：挂载层 id，默认 `acb.feedback.default`。
  - `revision`：乐观并发版本，转移以 `WHERE id=? AND revision=?` 更新。
  - 现有 `status` 列保留列名，但降级为**不透明状态 token**；合法集合由 workflow 解释。`note` 保留为最近一次转移的备注。
- `feedback_events`：`id / feedback_id / seq / kind / at / actor_kind / actor_id / via / from_state / to_state / reason / data`。`kind` 是挂载层解释的字符串（默认：`reported / transitioned / commented / linked / imported`）。
- `feedback_comments`：`id / feedback_id / parent_id / author / actor_kind / via / body / created_at`。评论本身同时写入 timeline，保证详情页只查一张事件流也能看全。
- `feedback_sources`：`feedback_id / room / session / harness / first_at / last_at`，主键 `(feedback_id, room, session)`。合并时保留所有来源；通知和可见性都按它算。
- `feedback_attributes`：`feedback_id / namespace / key / value_json / updated_at / updated_by`，主键 `(feedback_id, namespace, key)`。`component / assignee / area / iteration / duplicate_of / recurrence_of` 全部是挂载层写进这里的键，core 不校验值的业务含义；`duplicate_of` 的「必须指向另一条」由默认 workflow 校验。

替代方案与否决：

- 直接给 `feedback` 加 `component / assignee / area / iteration` 列：每次工作流变化都要改 kernel schema，core 变成 issue tracker，否决。
- 只加一个 `metadata JSON` 列：实现最省，但无法按 field 查询、无法约束 `duplicate_of`、并发更新整块 JSON 会互相覆盖；只适合展示，不适合作工作项存储，否决。
- 只存事件、状态靠重放：状态查询每次都要扫全时间线，列表页代价不可控，否决；当前状态缓存 + 追加事件是折中。

### 2.2 状态机

默认 workflow 的状态：

`open → triaged → in_progress → blocked → resolved / wontfix / duplicate`

合法转移（默认策略）：

| 从 | 可以到 |
|---|---|
| open | triaged、in_progress、wontfix、duplicate |
| triaged | in_progress、blocked、wontfix、duplicate |
| in_progress | blocked、resolved、wontfix、duplicate |
| blocked | in_progress、resolved、wontfix、duplicate |
| resolved / wontfix | open（复发/重新打开） |
| duplicate | open（解除误判） |

规则：

- **每次转移都必须有操作者、入口、原因**。`actor_kind = agent / console / admin`，`via = tool / console / notification`。原因不允许空字符串。
- 默认策略：agent 只能提交和评论，不能改状态；`console` 和 `admin` 可以转移。这样 agent 不会自己把问题标成 resolved。
- `duplicate` 必须带 `duplicate_of`：目标必须存在、不能指向自己、目标不能已经是 `duplicate`。由 workflow 在转移事务里校验，core 只负责把属性与转移事件一次性写入。
- `revision` 防止两个控制台窗口同时改状态；版本不匹配返回冲突，不静默覆盖。

**关闭后复发**：选择「新建一条关联记录」，不选「重开旧记录」。

- 复发时旧条目保持 `resolved / wontfix`，新建一条同 `scope_key + fingerprint` 的记录，状态从 `open` 开始，并写 `linked` 事件 + `recurrence_of = 旧 id`。
- 理由：保留「上一轮修复已经关闭」的事实，复发可以重新分派、重新定级，不会被一次静默状态回退掩盖；旧记录的关闭时间和处理人也不会被覆盖。
- 替代方案「重开旧记录 + 事件」：实现更省、计数连续，但会抹掉「它曾经被修好过」的时间点，且重新打开后无法区分原处理和本次复发；默认不采用。workflow 以后可以选择重开模式，但 core 不写死。

### 2.3 线程

采用「评论表 + 时间线」，评论支持**一级父评论**，不做任意深度树。

- 每条评论有 id、作者、入口、正文、`parent_id`（可空）。
- 处理者回复某条追问时把 `parent_id` 指过去；提问者可以再回复处理者的评论。UI 按时间线展示，并在有父评论时显示「回复 #12」。
- 状态转移、评论、关联都进入同一条 timeline，详情页按时间顺序展示。
- 现有 `replies` 保留为只读兼容视图：由 comments 合成，旧客户端仍能读到 `replies`。

替代方案与否决：

- 继续用 `replies` JSON：没有评论 id，无法回复某一条、无法做 read/通知、并发追加要重写整块数组，否决。
- 任意深度父子树：多轮对话用不到，缩进和折叠会把最小 UI 拖成论坛，否决。

### 2.4 去重与可见性

**默认按房间隔离去重**，不做跨房间合并。

- `scope_key = room:<room>`；同一房间内同指纹仍合并：`repeats + 1`、`occurrences` 相加，来源写入 `feedback_sources`（同房间不同 session 都记）。
- 同房间的回复/状态事件广播给该房间所有来源 session（它们本来就能看到同一房间内容）。
- 跨房间同指纹：**不合并**，各建一条；只在控制台写一条 `duplicate_candidate` 关联事件（指向另一条），房间侧看不到对方的存在、标题、正文、回复或次数。
- 隐私边界：房间是边界。群成员本来能看到群消息，所以群内合并不会额外泄露；私聊与群聊之间绝不合并、绝不广播。控制台/管理员能看到跨房间候选关系，但不会把 A 房间内容推进 B 房间。
- 关闭态不参与去重：同一 `scope_key + fingerprint` 可以有「已关闭的旧条目 + 新开条目」；去重只命中最新的未关闭条目。

替代方案与否决：

- 全局合并 + 向所有来源房间广播：B 房间会收到 A 房间的答复，私聊内容可能被推进群，否决。
- 完全按 session 隔离：同一房间的两个 agent 会把同一个坑各提一条，房间里出现两份重复；房间已经是可见性边界，按 session 隔离过细，否决。

### 2.5 通知

复用两层：房间内的 `feedback.*` 合成事件，以及 `acb-notifier` 的带外 ntfy。**带外只推摘要，不推正文/建议/context**。

| 事件 | 房间内合成事件 | ntfy 带外 | 只进控制台 |
|---|---|---|---|
| 新提交 / 同指纹合并 | 无 | blocker 可选 | 默认 |
| 处理者新回复 | 是（`feedback.reply`） | 否 | 是 |
| agent 新回复/追问 | 否（避免自唤醒） | 是（提醒处理者） | 是 |
| 状态转移到 blocked | 是（`feedback.status`） | 是 | 是 |
| 状态转移到 resolved / wontfix / duplicate | 是（`feedback.status`） | 否 | 是 |
| 被指派 | 若 assignee 是 agent | 是 | 是 |
| 超时未处理（SLA） | 否 | 是，按摘要汇总 | 是 |
| 跨房间 duplicate_candidate | 否 | 否 | 是 |

- 配置放在 `[feedback.notifications]`：`push = ["assigned", "blocked", "overdue", "agent_reply"]` 默认只开这四类；`min_interval_secs` 复用同一条工作项+事件类型的去重窗口；`room_limit` 限制单位时间推送条数；`digest_secs` 把多条 overdue 汇总成一条。
- 实现上给 `NoticeEvent` 增加 `feedback` 变体，`acb-notifier` 增加对应渲染；kernel 仍然只看到 `NoticeSink`，不引入 HTTP。
- 房间内事件沿用现有 `feedback.reply` 路径，新增 `feedback.status` / `feedback.assigned`；都是合成事件，走 store + wake，不绕过 wake 策略以外的人工直通（回复仍按现有语义直接排唤醒）。

替代方案与否决：

- 所有变化都推 ntfy：一次 triage 就能推三条，人会关掉通知，否决。
- 只进控制台：blocked 和 overdue 没人看，工作项又变回垃圾场，否决。

### 2.6 控制台

最小两视图，不抄 GitHub：

- 列表：id、状态、kind/severity、标题、来源房间/session、assignee（若挂载层有）、回复数、最近时间、blocked/overdue 标记；筛选状态、类型、来源、assignee。
- 详情：正文、建议、出处（tool/session/room/harness/context）、挂载属性（component/area/iteration 等，按 workflow schema 渲染）、评论线程、时间线（提交/转移/评论/关联/通知），操作区：转移（必须填原因）、评论、指派（若策略允许）、标 duplicate（必须选目标）。
- 正文是纯文本；不引入富文本编辑器。
- 兼容：`GET /api/feedback` 保留，新增 `GET /api/feedback/:id` 详情、`POST /api/feedback/:id/transition`、`POST /api/feedback/:id/comments`；旧 `POST /api/feedback/:id/status` 作为 transition 的包装（原因写 `console legacy status`），旧 `POST /api/feedback/:id/reply` 写评论。
- 工具面：`report_feedback` 参数不变；`feedback_list` / `feedback_get` 原有字段不变，新增 `comments / events / attributes / revision`；`feedback_reply` 语义不变。`feedback_submit` 若以后确实要作为工具名，只做别名，不改参数。

### 2.7 责任归属的挂载位置

`assignee` 不是 core 字段，放在 `feedback_attributes` 的挂载层 namespace 下（建议 `workflow.default`，键 `assignee`，值保存稳定 principal id）。core 只保证「可以挂、可以查、可以随转移一起写」，不实现分配逻辑。具体给谁、是否给具名呈现身份，见第 4 节。

## 3. 迁移步骤

现有库 8 条、全 `open`、replies 全空，迁移可以一次性完成，且**加法式、不删列**：

1. **schema 11 → 12**：新建 `feedback_events`、`feedback_comments`、`feedback_sources`、`feedback_attributes`；`feedback` 增加 `scope_key / workflow / revision`。`status` 列继续存在，但由 workflow 解释。
2. **回填**：
   - 每条旧记录 `scope_key = room:<room>`（无 room 用 `session:<session>`），`workflow = acb.feedback.default`，`revision = 1`。
   - 状态映射：现有的 `open / resolved / wontfix / duplicate` 原值保留；旧 `acked` 映射为 `triaged`（回复过就表示已有人接手分类），旧 API 里的 `status=acked` 继续接受并映射到 `triaged`，避免旧客户端报错。
   - 写一条 `imported` 事件：`at = first_at`，`actor_kind = agent`，`via = tool`，data 记录 `repeats / occurrences`。
   - 写来源：`feedback_sources` 一行 `(feedback_id, room, session, harness, first_at, last_at)`。
   - `replies` 为空，无需搬评论；以后非空时按 `at` 顺序搬成 `feedback_comments`，`parent_id = NULL`，原 `author/via/text` 原样保留。
   - 不重建 `feedback` 表，id 全部保留。
3. **去重索引**：去掉 `fingerprint` 全局唯一约束，改为普通索引 `(scope_key, fingerprint)`；是否算「同一活跃项」由 workflow 查询终态集合判断。不用 SQLite 部分唯一索引，因为终态列表属于策略，写进索引就等于写死 core。
4. **读写切换**：
   - 写：提交仍写主记录与 `reported` 事件；合并只针对同 scope 的活跃项；关闭态命中的复发走「新建 + recurrence_of」。
   - 读：`feedback_list/get` 从主记录 + comments/events 组装，旧 `replies` 字段由 comments 合成；控制台列表/详情走新表。
   - 兼容：旧 `status` 字段、旧 API、旧工具字段全部保留一个版本；确认新路径稳定后再删兼容层。
5. **通知**：先接房间内 `feedback.status/assigned` 合成事件，再扩展 `NoticeEvent` 与 `acb-notifier`；默认只开 blocked/assigned/overdue/agent_reply。
6. **验证**：用现库副本跑迁移，断言 8 个 id 不变、8 条 imported 事件、8 条 source、状态全 open；旧 API 返回字段不变；新详情能看时间线与评论。
7. **回滚**：迁移只加表/加列，旧程序仍能读原列；新程序回退时新表留着不影响启动，不反向删数据。

## 4. 待决问题

- **谁负责、如何分配、是否分给具名的呈现身份（如 trae宝）**：不拍板。可选口径包括：不分配、按 `component` 标签路由、默认 owner、每周人工巡未认领、分给具名身份。schema 只留 `assignee` 的挂载位置，不写分配逻辑。
- 默认 workflow 是否允许 agent 把条目推进到 `in_progress`：本文默认不允许，agent 只能提交/评论；需确认是否给 agent 自助认领。
- SLA 阈值与 overdue 的统计口径（按创建时间还是最近一次处理时间）。
- 事件/评论保留上限（沿用 5000 条主记录上限，还是单独按时间裁剪）。
- 跨房间 `duplicate_candidate` 是自动建立还是控制台人工确认后建立。

## 5. 明确不做

- GitHub issue 双向同步、导入/导出。
- sprint / iteration 管理、看板泳道、燃尽图。
- 工时、估点、计费。
- 富文本编辑器、附件、图片上传。
- 完整权限模型与私有评论。
- 可视化 workflow 编辑器、自动分派/SLA 自动关闭。
- 自动语义去重（正文相似度、向量聚类）；本轮只保留标题指纹 + 同房间合并 + 跨房间候选。
- #6（memory Git 拓扑）和 #7（重投丢绑定）不在本设计内，等用户点头后另行处理。
