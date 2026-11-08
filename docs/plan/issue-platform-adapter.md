# 把 issue 做成平台适配器：提议者与处理者互联的设计

状态：设计草案（2026-11-07）。本文只写设计，不改代码。

## 0. 结论先行

1. **issue 就是一种平台**：一条 issue 是一个房间，一条讨论/修订是一条消息，状态、指派、链接、附件等变更是平台事件。适配器把 issue-service 的 HTTP API 翻译成 ACB 的 room / message / event / `platform_call`，core 不引入任何 issue 概念。
2. **不做原地编辑，改追加式修订（revision）**。评论、状态、指派、链接、附件全部是同一时间线上的新修订；"改评论"等于追加一条新修订，消息流里表现为新消息，旧修订永久保留。幂等键是修订号（客户端另带幂等键）。
3. **连接 = 账号 = 一个 agent 身份**。受 ADR-0012"一条连接对应一个账号"约束，提议者和处理者各自用一条 issue 连接、各自的 token 连到同一个 issue-service；同一个 `issue/<id>` 路径在两条连接上各自成为一个房间。平台层面的"同一房间"由 issue id 保证，桥层面是两条镜像房间。这是不增加 core"按会话变身份"能力的最小做法。
4. **投递按"参与关系"推导，不靠逐房间写订阅**。issue 房间在运行时才产生，若每条 issue 都写订阅，建单要改提议者/处理者两份配置，关单再删，订阅无限膨胀。改为：issue-service 在每条 issue 上维护参与者（创建者、指派者、@ 提及、旁听），适配器把参与关系同步给 core；会话只声明"连接 + 参与谓词"（如"指派给我且未关闭"），投递判定 = 该会话账号是否为该房间的活跃参与者。显式房间订阅降级为"旁听某单"的特例。
5. **core 为此只加两类通用能力**：`sessions.bindings`（连接级参与绑定 + 角色谓词）和 `room_members`（平台上报的参与关系索引）。core 不理解 issue 状态，也不解析"指派给我"这类谓词；谓词由适配器按 manifest 声明的 `membership` 能力解释。
6. **历史以 issue-service 为准，桥的流水只是投递层**。revision 表 append-only、永不原地覆盖；参与关系的加入/退出也作为修订留痕。桥侧 records 按既有保留策略清理，清理不影响 issue 的审计与两修订比对。
7. **上线审批点是唯一未决项**，设计成 `human_click` 与 `auto_with_audit` 两种模式，靠配置切换；两种模式下 LLM 都不直接写升级请求。
8. **issue 不是自迭代的唯一形态，只服务"外源需求链"**。人类或 agent 提出的需求/缺陷，需要澄清、处理、验收，才走工单；探针失败、插件崩溃、行为回归、能力退化等内源触发走短链、只产出事件。两条链只在尾声（建代 → 验证 → 切换 → 观察 → 回滚/LKG）共用，不要为了统一把内源塞进工单。详见 §1.4。

---

## 1. 现状与依据

### 1.1 平台边界

- [ADR-0020](../adr/0020-platform-capabilities.md)：core 平台无关、harness 无关；平台能力只经 `platform_methods` / `platform_call` 暴露；没有对应能力的平台失败关闭。
- [ADR-0027](../adr/0027-platform-adapter-registration.md)：适配器即插件，manifest 静态声明 `platform` / `kind` / `transport` / `capabilities` / `events` / `methods`；`RoomId` 首段是 connection 名，其余是平台路径；`platform_call` 不能改目标房间，也不能补出静态声明里没有的方法。
- [ADR-0002](../adr/0002-subscription-model.md)：会话可见范围 = 自己订阅的 `connection + room`，按房间记录边界而不是按消息内容过滤。
- [ADR-0005](../adr/0005-platform-events.md)：平台事件是 `RecordKind::Event`，不消耗消息 id，不等于未读消息。
- [ADR-0011](../adr/0011-clock-policy.md)：除了收件时唤醒，还有时钟驱动的唤醒判定，可用于超时升级，不改变唤醒架构。
- [issue-system.md](issue-system.md)：issue-service 已是独立子系统；阶段 2 计划用 outbox + wake provider 打通桥。本文改为"issue 适配器"路线：适配器轮询 issue-service，事件仍走 core 的 ingest → unread → wake 一条路，不再让 wake provider 直接读 issue 库。

### 1.2 现有 issue-service 能提供什么（代码依据）

- 数据库：`issues`、`issue_events`（追加流水）、`api_tokens`，见 `crates/issue-service/src/lib.rs` 的 schema 段。
- 状态常量：`new` / `in_progress` / `pending_verification` / `done` / `closed`，见 `lib.rs` 第 17–21 行；`allowed_transition` 第 727 行。
- token scope：`issue:read` / `issue:write` / `issue:report` / `issue:subscribe` / `issue:verify` / `issue:admin`，见第 23–28 行。
- `update_issue` 已把状态/指派/优先级变更写成 `issue_events`（`kind=issue.status_changed` 等），`comment` 参数也只是写一条 `issue.comment_added` 事件，没有评论实体、没有评论 id、没有 diff、没有附件/迭代/链接表。见 `update_issue` 约第 941–1098 行。
- 事件写入是 `emit_event`（约第 772–817 行），只做审计和订阅，不是可重放的 revision 流；没有 revision 号，也没有"任意两修订比较"的接口。

**结论**：把 issue 当平台，issue-service 需要补的是"平台侧数据形状"（revision、评论 id、链接、附件、迭代、diff 接口），不是桥 core。现有 `issue_events` 可作为迁移起点，但不能直接当 revision 用。

### 1.3 桥侧的硬约束（代码依据）

- 订阅是配置期写死的精确房间列表：`Subscription` 只有 `connection + room`（`crates/kernel/src/config.rs` 第 638 行、解析约 1855–1885 行），`subscription_for` / `subscribers_of` 只按 `subscription.room == room` 查（`config.rs` 约 1975–2011、`engine.rs` 第 5029–5046 行）。动态 issue 房间默认不会被投递，且每建一单都要改两份会话配置。
- 订阅集不只用于投递：未读/列表/读历史工具都直接遍历 `session.subscriptions`（`engine.rs` 第 8149、8423、8698 行），平台动作鉴权的 `subscriptions` 也来自它（`engine.rs` 第 4541–4545 行），`grants.rs` 的 `target_is_reachable` 再拿它判断目标可达（第 770、816 行）。因此"参与关系推导"不能只改 ingest 一处，必须统一到一个新的"会话可见房间集"来源。
- 入站记录带 `suppressed` 标志：只进流水、不投递、不唤醒、不计未读（`crates/adapter-api/src/lib.rs` 第 331–362 行）。可用于"修订审计事件"。
- `platform_call` 的 `MethodSpec` 有 `scope`（room/connection）、`risk`、可选 `record`，调用结果可以带回一条 `WireOutgoingRecord`（ADR-0027；wire 定义见 `adapter-api` 第 400–473 行）。状态变更事件可借此写进房间流水，不需要 core 特判。
- `records` 表只有 `(connection, room_key, platform_id)` 普通索引，没有唯一约束，也没有按 platform_id 去重（`crates/kernel/src/storage/sqlite.rs` 第 221–233、517–553 行）。重投只能在适配器侧控。
- 回环防护：自己发出的消息按待回显表抑制（ADR-0002）。issue 适配器发出 comment 后，轮询会读到同一条 revision，必须靠"自己写入的 platform_id 已在待回显/已发集合"过滤。

### 1.4 适用范围：外源需求链 vs 内源短链

issue 平台只定义**外源需求**这条链。自迭代不以工单为唯一单位，内源触发不进工单。

| 维度 | 外源需求链（使用 issue 平台） | 内源短链（只产出事件，不用 issue） |
|---|---|---|
| 入口 | 人类或 agent 在公开群/私聊提出需求或缺陷 | 探针失败、插件崩溃、行为回归、能力退化等运行时/回归信号 |
| 参与方 | 提议者、处理者、验收人（提议者）、旁听者 | 信号源、修复会话（必要时 on-call 人）；没有"提议者"角色 |
| 关卡 | 合理性判定（驳回/重复）→ 信息完整性（待补充）→ 澄清 → 处理 → 验证+上线 → 提议者验收 → 关闭 | 检测确认 → 修复 + 回归测试 → 升级链的建代/验证/切换/观察/回滚 |
| 产物 | issue、修订、讨论、参与关系、链接、附件、验收记录 | 事件（`incident.*` / `repair.*` / `deploy.*`）、日志、升级请求；不建 issue、不做讨论区/指派/验收 |
| 存储 | issue-service 的 revision 表（长期审计） | 事件流水 + 运行日志；按事件保留策略清理 |
| 生命周期 | 以天计，可以挂起等待澄清/验收 | 以分钟/小时计，目标是尽快闭环 |
| 适合 | 需要澄清、需要多方可见、需要人类验收、需要长期审计 | 高频、明确、无需澄清、无需人类验收的故障与退化 |

**共用尾声**：两条链在"要上线"之后走同一条升级链——建代 → 验证 → 切换 → 观察窗口 → 提升 LKG，失败回滚（§6.1）。这是唯一的共用段，前面不共享状态机、存储和界面。

**边界规则**：
- 内源事件只有在暴露出"需要产品决策 / 需求澄清"时，才**显式**开一条外源 issue 并链接 incident 事件；不是自动把每个 incident 变成工单。
- 外源 issue 处理中发现是已知内源故障，可追加 `incident` 链接，但仍以 issue 的澄清与验收为准。
- 高频探针抖动、纯运维重启、无需求语义的告警、需要秒级闭环的故障，**明确不适用 issue**：走短链，避免工单成为瓶颈。

---

## 2. 适配器接口映射

### 2.1 连接、身份与房间

新增一个平台适配器（建议 crate `acb-adapter-issue`，manifest 里 `platform = "issue"`，transport 由实现选 HTTP client；core 不解释 transport）。两条连接：

```toml
[[connections]]
id = "issue-proposer"
platform = "issue"
kind = "issue"
transport = "http-client"
account = "agent:proposer"        # 平台侧 principal
token = "${ACB_ISSUE_PROPOSER_TOKEN}"   # 密钥经 env 注入，不写进仓库
options = { base_url = "http://127.0.0.1:8080", role = "participant", poll_interval_ms = 5000 }

[[connections]]
id = "issue-handler"
platform = "issue"
kind = "issue"
transport = "http-client"
account = "agent:handler"
token = "${ACB_ISSUE_HANDLER_TOKEN}"
options = { base_url = "http://127.0.0.1:8080", role = "handler", poll_interval_ms = 5000 }
```

> 说明：配置本身是否支持 `${ENV}` 插值待验证；若不支持，按现有密钥注入方式改成启动时读取 env 后写入配置/进程环境，原则是 token 不落仓库、不落 issue 文本。

房间：`RoomId` 首段是 connection，路径是 `issue/<issue_id>`。因此：

- `issue-proposer:issue/ISSUE-123`
- `issue-handler:issue/ISSUE-123`

它们是同一个平台对象、两条连接的镜像。**不要**把"同一房间"理解成同一个 `RoomId`；ADR-0012 下连接即账号，两个 agent 不可能共用一条连接写评论且各自署名。

`room_scope = multi_party`，`room_labels = ["issue", "state:<category>"]`。房间不靠逐房间订阅进入会话：会话通过 §4 的参与绑定自动看见 `issue/<id>`，显式订阅只用于旁听。私密澄清不需要新房间：revision 上带 `visibility = participants`，适配器只对参与者的连接 emit 这条记录；观察者连接读不到。

### 2.2 概念映射表

| issue 概念 | 平台映射 | 说明 |
|---|---|---|
| 一条 issue | 一个 room `issue/<id>` | 房间路径就是 issue id；跨连接用同一 id 对齐 |
| 讨论区一条评论 | `RecordKind::Message` | `sender` = principal；`platform_id = rev:<issue_id>:<rev_no>` |
| 评论的新版本 | 新的一条 Message | 不覆盖旧消息；`labels` 带 `comment_revised` 与 `revises:<comment_id>` |
| 状态/指派/迭代/链接/附件变更 | `RecordKind::Event` | `event = "issue.revision"`，tags 细分；payload 是字段 diff |
| 修订审计（含评论被改） | `RecordKind::Event` + `suppressed=true` | 只进桥流水，不投递、不唤醒；保证桥侧也有一条可查的系统事件 |
| issue 标题/正文/富字段 | 不单独成消息 | 作为 event payload 透传；正文快照随 revision |
| 新建 issue | event `issue.created` | 由适配器路由给 handler（队列/指派规则） |
| 参与者加入/退出 | event `platform.membership` + `suppressed=true` | 更新桥的成员索引，不投递、不唤醒；issue 侧同时写 `participant` 修订 |
| 附件 | 链接型记录或 event | v1 只放元数据（名字、大小、sha256、URL），不搬运二进制 |
| 提醒/点名 | `at_self` | 适配器根据 comment 的 mention/visibility 置位 |

**只作为载荷透传、core 不理解的 issue 语义**：标题、正文、状态分类与 reason、指派、迭代、area、优先级、标签、链接类型与目标、附件元数据、修订 diff、参与人、visibility、合并/重复关系。core 只看到 elements/event/tags/labels。

### 2.3 方法表（`platform_methods`）

全部是 issue 适配器静态声明、`platform_call` 调用；**不新增 core 内建工具**。

| 方法 | scope | risk | 作用 / 记录 |
|---|---|---|---|
| `issue.create` | connection | write | 建 issue，返回新房间路径；result 里带 `WireOutgoingRecord`(event `issue.created`) |
| `issue.comment` | room | write | 追加讨论修订；`idempotency_key` 必填；返回 `platform_id=rev:...` |
| `issue.transition` | room | write / 关闭类为 destructive | 追加状态修订；必须带 `expected_revision`；result 可带事件记录 |
| `issue.assign` | room | write | 追加指派修订 |
| `issue.link_add` / `issue.link_remove` | room | write | 追加链接修订；类型：commit / generation / room / parent / child / related / hyperlink |
| `issue.attachment_add` / `attachment_remove` | room | write | 追加附件修订（元数据） |
| `issue.iteration_set` | room | write | 追加迭代修订 |
| `issue.get` | room | read | 当前快照 |
| `issue.history` | room | read | 按 revision_no 升序列修订 |
| `issue.diff` | room | read | 任意两修订字段级 diff |
| `issue.search` | connection | read | 按状态/指派/迭代/标签检索（结果含 issue id） |
| `issue.participants` | room | read | 参与者与 visibility 规则 |

不在表里的能力一律不可用（ADR-0020 失败关闭）。`issue.comment` 是 issue 房间的**唯一规范写入路径**；`send_message` 不适配到 issue 连接，原因是 core 的 Send 线协议不携带幂等键，超时重试会产生重复评论（见 §3.3）。如果 agent 误用 `send_message`，适配器按普通消息落成一条无幂等键评论并打 `degraded` 标签，作为兜底。

### 2.4 core 需要新增什么（越少越好）

**必加（两类通用能力，均与 issue 无关）**：

1. **连接级参与绑定 `[[sessions.bindings]]`**：替代"逐房间订阅"作为动态平台的主要来源。字段：`connection`、`roles`（可选，如 `["assignee", "creator", "mention"]`；空 = 任意参与者）、`include_closed`（默认 false）、`wake`。语义是"此会话接收该连接上、我作为活跃参与者出现的房间"。
2. **参与关系索引 `room_members`**：适配器通过 manifest 新增 capability `membership`（字符串能力，现有 manifest 的 `capabilities` 就是字符串列表，见 `adapter-api` 第 104–113 行），并按房间上报成员快照/变更事件；core 落 `room_members(connection, room_key, principal, roles, state, since, until, rev)`。投递判定 = 绑定存在 + 账号在该房间有匹配 `roles` 的活跃成员行。

```toml
[[sessions.bindings]]
connection = "issue-handler"
roles = ["assignee"]          # 谓词：指派给我
include_closed = false        # 谓词：未关闭；关单时适配器把成员置 left/archived
wake = { events = ["issue.created", "issue.revision"], tags = ["comment", "verification"] }
```

配置校验：`bindings` 只能用于声明了 `membership` 能力的连接；没有该能力的平台（QQ/OneBot）仍走原有 `subscriptions`，行为不变。显式 `[[sessions.subscriptions]] connection + room` 保留，语义是"旁听/固定观察某单"，不参与推导。

统一的"会话可见房间集"必须从两处合并：显式订阅（精确房间）+ 绑定查询 `room_members`。`subscribers_of`、`list_rooms`、未读、`platform_call` 的鉴权 `subscriptions`、`grants.rs::target_is_reachable` 都改为调用这一个来源（详见 §4.4）。

**可选（后置）**：入站按 `(connection, room_key, platform_id)` 去重。没有它，适配器重启/重投窄窗口内可能重复一条评论。v1 先靠适配器水位 + 待回显集合把窗口压到最小，并在流水里标 `redelivery=true`；是否加唯一索引留作后续评估（见 §9 待验证）。

**不加**：issue 表、评论逻辑、状态机、附件、diff 都不进 core；不新增 `issue_*` 内建工具；不让 core 解析 issue 文本。适配器只上报"谁在哪个房间、什么角色、是否活跃"，不把 `state=closed` 这类业务状态塞进 core。

---

## 3. 修订制、幂等与水位

### 3.1 修订制（不做编辑）

- 每 issue 一条单调递增 `revision_no`；每次变更追加一行 `issue_revisions`，不 UPDATE 旧行。
- 修订内容三类：
  - `discussion`：讨论区消息；带 `comment_id`（同一条评论的新版本沿用同一 `comment_id`）、`reply_to_comment_id`、`visibility`、正文。
  - `field`：状态/指派/迭代/优先级/标签等字段变更；`patch` 是字段 diff。
  - `relation`：链接/附件/父子关系增删；`patch` 指向 link/attachment id。
- 修订号由服务端在事务内分配；客户端只传 `expected_revision` 和 `idempotency_key`。
- "评论被改"= 追加一条 `discussion` 修订（同 `comment_id`，更高 `revision_no`），外加一条 `suppressed` 的系统事件。桥的消息流看到的是**新消息**，不是编辑旧消息；历史页看到的是同一 `comment_id` 的修订序列。这与 Azure Work Item 的 Discussion/History 一致。

### 3.2 稳定消息 id

- 桥侧 `platform_id = rev:<issue_id>:<revision_no>`。
- 理由：修订号是 issue-service 保证的唯一、单调、不可变键；评论 id 不足以区分版本；用时间戳或内容哈希会在重试/并发下漂移。
- 引用回复：`quote_of` 存被引用消息的同一个 `rev:` id；core 的 `located()` 能按 `(connection, room, platform_id)` 反查到自己的镜像记录。跨连接的引用由适配器把目标 `rev:` 转成 issue-service 的 `comment_id` 查询，再落成本连接的 `rev:`。
- 不把 `comment_id` 当消息 id：同一条评论有多个修订，每个修订都是一条独立消息；用 `comment_id` 会让"新修订=新消息"无法表达。

### 3.3 幂等

- 服务端：`issue_revisions.idempotency_key` 唯一。重试同 key 返回同一修订，不新增行。
- key 约定：`<actor>|<issue_id>|<op>|<turn_or_client_id>`。适配器在 `platform_call` 里透传；agent 侧由 core 的 turn/operation id 生成，避免模型自由发挥。
- `issue.transition` / `assign` / `link` 还必须带 `expected_revision`；版本落后返回 409，由调用方重新读快照再决定，不做 last-write-wins。
- 适配器 Send 兜底路径没有幂等键：发送超时后不自动重试，返回 `ambiguity` 让上层读历史确认，避免重复评论。

### 3.4 重投与水位

两个水位，不能混：

1. **平台水位**：适配器按 issue 持久化 `last_emitted_revision`（在自己的 state dir，随连接重启保留）。轮询 `GET /v1/issues/{id}/revisions?after=<rev>`；只 emit 大于水位的修订。
2. **桥水位**：core 的每房间 `record id` + 会话 ack 水位，负责"投递到会话"的至少一次；未读/保留策略不变。

重投窗口与取舍：

- 顺序：先 emit 进 core 的 EventQueue，成功后推进本地水位。进程内适配器崩溃时，已入队事件仍由 core 落库，不会丢。
- 崩溃发生在"HTTP POST/platform_call 成功"与"水位推进"之间：重启后会重发同一修订。v1 接受这个窄窗口，靠 `idempotency_key` 保证 **issue 侧** 不重复，桥侧可能出现一条重复消息并标 `redelivery=true`。若要彻底消除，需 core 按 `platform_id` 去重（可选变更，见 §2.4）。
- 水位不能"先推进后发送"：那会在崩溃时永久丢事件；issue 历史仍在，但要人工回填，不可取。
- 长时间停机：按水位回放全部未投递修订；若积压超过阈值，适配器改为"合并回放"：状态类只发最终值 + 一条 `issue.catchup` 事件，讨论类逐条补发，避免 agent 被几百条旧状态变更唤醒。

---

## 4. 两个 agent 的连接与参与关系

### 4.1 从"逐房间订阅"改为"参与关系推导"

**现状问题**：`[[sessions.subscriptions]]` 是配置期写死的房间列表。每建一条 issue，提议者会话和处理者会话各要加一条 `[sessions.subscriptions]`；关单后还要删。订阅随 issue 数量无限膨胀，且改配置需要重载，无法支撑"生命周期以天计、随时新建"的流程。

**新模型**：投递由 issue 的参与关系推导，而不是由订阅表逐条声明。

- `issue` 字段里维护 `participants`：`creator`（创建者，建单自动加入）、`assignee`（指派时加入、改派时退出）、`mention`（评论里 @ 时加入）、`watcher`（显式旁听）。
- 适配器把参与关系作为**平台成员事件**上报（`event = "platform.membership"`，`suppressed = true`：只更新桥的成员索引，不投递、不唤醒、不计未读），同时 issue-service 追加一条 `relation` 修订留痕。成员事件与修订共用 `(issue_id, revision_no)`，可回查。
- core 用 `room_members` 保存"哪个账号在哪个房间、什么角色、是否活跃"，投递判定 = `该会话的账号是否为该 room 的活跃成员`。core 不解读 issue 状态，也不知道"未关闭"是什么意思。
- 会话用 `[[sessions.bindings]]` 声明谓词，绑定一个连接，而不是列出房间：

```toml
[[sessions.bindings]]
connection = "issue-proposer"
roles = ["creator", "assignee", "mention"]   # 我以这些身份参与的单
include_closed = false
wake = { events = ["issue.revision"], tags = ["verification_requested", "comment_mention"] }

[[sessions.bindings]]
connection = "issue-handler"
roles = ["triage", "assignee"]               # 谓词：待我初审 + 指派给我
include_closed = false                       # 谓词：未关闭
wake = { events = ["issue.created", "issue.revision"], tags = ["comment", "status"] }
```

**谓词由适配器解释**，不是 core 的查询语言：issue 适配器在每轮轮询时按 `roles + include_closed` 计算"该账号现在参与哪些未关单的房间"，并通过成员事件/快照同步。core 只按成员索引匹配。这样换一个平台（例如 Jira）只需换适配器的成员语义，core 不变。

| 会话 | 连接 | 绑定（谓词） | 唤醒 | 职责 |
|---|---|---|---|---|
| 提议者 | `issue-proposer` | `roles = creator/assignee/mention`，`include_closed = false` | `issue.revision` + `verification_requested` / `comment_mention` | 建 issue、补充信息、验收 |
| 提议者（原群） | 原有 QQ 连接 | 原静态订阅，不变 | 原策略 | 公开群讨论 |
| 处理者 | `issue-handler` | `roles = triage/assignee`，`include_closed = false` | `issue.created`、`issue.revision` + `comment` / `status` | 判合理性、追问、澄清、处理、提交验证 |
| 旁听者（可选） | 任一 issue 连接 | `[[sessions.subscriptions]]` 精确房间 | 静态订阅的 wake | 旁听某单；这是特例，不是主路径 |

与静态订阅的差异：

| 维度 | 静态 `[[sessions.subscriptions]]` | 参与推导 `[[sessions.bindings]]` |
|---|---|---|
| 来源 | 配置里的房间 id | 平台成员的实时索引 |
| 建单/关单 | 改配置、重载 | 配置不变，成员事件驱动 |
| 谓词 | 无，只有精确房间 | 角色、是否含已关单；由适配器解释 |
| 增长 | 随房间线性增长 | 配置恒定；成员行随 issue 增长（可归档） |
| 语义 | 会话可见性 = 订阅边界 | 会话可见性 = 参与关系 + 显式旁听 |
| 适配平台 | 所有平台（保留） | 声明 `membership` 能力的平台 |

### 4.2 参与关系的加入/退出时机

| 时机 | 角色变化 | 触发与留痕 |
|---|---|---|
| issue 创建 | `creator` 加入；按队列规则给指定处理者 `triage` 角色 | `issue.created` + `relation` 修订 `participant:join`；`triage` 让处理者在未指派前也能看到新单 |
| 指派 | 新 `assignee` 加入，旧 `assignee` 退出 | `issue.assign` + 两条 `participant` 修订（leave/join） |
| 评论 @ | 被 @ 的 principal 加入 `mention` | `issue.comment` + `participant:join`（同一 `revision_no` 的附属关系修订） |
| 手动旁听 | `watcher` 加入 | `issue.watch`（可选方法）或显式 `subscriptions`；前者留修订，后者只影响桥侧 |
| 取消指派 | `assignee` 退出 | `issue.assign` + `participant:leave(reason=unassigned)` |
| 关闭/驳回 | 所有角色从"活跃"转为 `archived` | `issue.transition` + `participant:archive(reason=closed)`；不物理删成员行 |
| 重开 | 默认恢复 `creator` + 最后一个 `assignee`；`mention` 不自动恢复 | `issue.transition` + `participant:join`；需要谁再 @ 或指派 |

- 成员行带 `since` / `until` / `state`（`active` / `left` / `archived`），append-only + 关闭时间，不 UPDATE 覆盖历史。
- "加入/退出"本身是 issue 时间线上的修订；成员事件只是桥侧的同步信号。两边用同一 `revision_no` 对齐，避免"桥认为在房间、issue 认为已退出"。
- 适配器每轮轮询还发一次**成员快照**（当前 participants），用于桥重启/漏事件时校正；快照也带 revision 号，幂等。

### 4.3 关单后参与关系如何处理

1. **停止投递**：`Closed` / `Removed` 是终态，issue 不可再追加讨论（除非受权重开）。适配器把该 issue 的所有成员置 `archived`，`include_closed = false` 的绑定不再匹配，因此不会再产生未读/唤醒。
2. **保留只读**：`archived` 成员仍可读该房间的历史（直到桥保留期清理），用于审计、引用和验收复查；读不产生未读、不唤醒。需要继续接收的审计 agent 可显式 `include_closed = true`。
3. **不删成员行**：删掉会丢失"谁参与过、何时退出"的审计；保留行不会导致配置膨胀，只增加数据库行。issue 归档/保留策略（§5.6）触发时再一起归档。
4. **重开不自动复活所有人**：只恢复创建者和最后指派者；原 `mention`、`watcher` 需要重新加入。这样避免关单很久后被一条重开唤醒一批早已不相关的会话。
5. **旁听订阅是唯一可能增长的部分**：显式 `subscriptions` 不由参与关系管理，建议给 console 的"watch"加 TTL（例如 30 天），或在关单时提示清理；不要把旁听写进 `config.toml`。

### 4.4 需要改的代码位置

新增一个统一入口（建议 `Config::visible_rooms(session, store/room_members)`）：

- **配置**：`Subscription` 定义（`config.rs` 第 638 行）与 raw 解析（约 1855–1885 行）增加 `bindings`；`subscription_for` / `subscribers_of`（`config.rs` 约 1990–2011 行）改成"显式订阅 + 成员索引"合并；配置校验（约 2040 行）要求 `bindings` 只用于声明 `membership` 能力的连接。
- **投递/ingest**：`engine.rs` 第 2017、2099 行（`subscribers_of`）、第 7930 行（事件投递）。注意第 2099–2103 行：没有订阅者时 `ingest` 直接返回 `NotSubscribed`，根本不落库。**成员事件必须先过这道门**，否则新 issue 的第一条记录永远进不来。通用规则：对声明 `membership` 能力的连接，`platform.membership` 控制记录先更新 `room_members` 再投递，且不要求房间已有订阅者；适配器按"先成员快照、后内容修订"的顺序发。这不是 issue 特例，而是成员能力的通用语义。
- **未读/列表/读历史**：`engine.rs` 第 8149（`list_rooms`）、8423、8698、10204 行，不再直接遍历 `session.subscriptions`。
- **鉴权**：`engine.rs` 第 4541–4545 行把 `subscriptions` 传给 `GrantQuery`；`grants.rs` 第 770、816 行 `target_is_reachable` 依赖它。这里必须注入推导出的可见房间，否则从动态房间发起的 `platform_call` 会被 `base:subscription` 拒绝。
- **唤醒**：`engine.rs` 第 2905、3012、7930 行的 `subscribers_of` 改为同一入口；wake 策略从 binding 继承。
- **存储**：新增 `room_members` 表与索引（见 §5.5）；`records` 表结构不变，`room_members` 不参与消息保留，按 issue 保留策略归档。
- **适配器协议**：`adapter-api` manifest 的 `capabilities` 增加字符串 `"membership"`；约定通用事件名 `platform.membership`，payload 为 `{principals:[{account, roles, state, rev}]}`；core 只认这个通用形状，不认 issue 字段。
- **issue 适配器/issue-service**：issue-service 增加 participants 字段与 `participant` 修订；适配器轮询成员快照与变更，emit 成员事件。

### 4.5 "生命周期以天计"的会话

- 会话是配置对象，不要求 harness 进程常驻。一次 turn 结束后可以断开；未读、游标、issue 快照都在服务端/桥库里。
- 澄清可能挂起数天：issue 停在 `Active / clarifying`，修订与参与者都在 issue-service；任何一方回复都追加修订，适配器照常 emit，唤醒对方。
- 桥重启：适配器从持久化水位回放；core 的未读记录只要没被清理就仍在。清理规则保护未读（ADR-0002），已读旧消息被清理不影响 issue 历史（issue-service 是审计真源）。
- 超时：用 ADR-0011 的时钟策略做"待补充 N 天未回复 → 提醒/降级"，不新造调度器。

### 4.6 事件驱动唤醒如何触发

沿用现有链路：适配器 emit → `ingest` 写 records → 可见房间/成员匹配 → 未读 → wake policy。

- `issue.created` / `issue.revision` 是平台事件；wake 策略用 `events` 和 `tags` 选择。state/assign 变更默认**只记账不唤醒**，只有显式 `verification_requested`、`mention`、`comment` 才唤醒对应会话，避免每次字段变更都拉起一次 turn。
- 适配器可置 `at_self`（评论里点名该 principal、或 visibility 指定参与者）；`reply_to_self` 由适配器按引用关系置位，core 不猜。
- 不使用 issue-service 的 outbox 直接把事件塞给 wake provider：那会绕开参与边界和回环防护。outbox 可作为适配器的补洞来源（水位对不上时重建），但投递仍走同一条 ingest 路径。

### 4.7 一对一澄清

不需要额外房间：澄清修订 `visibility = participants`，参与者 = 提议者 + 处理者；适配器只对这两个 principal 的连接 emit，其他连接（观察者/审计）不产生记录。桥侧访问由 `room_members` + 绑定限定；两边都是 `issue/<id>` 房间，私密性由平台 payload + 适配器路由保证。**注意**：这是平台侧过滤，不是 core 的按消息内容过滤；core 不做内容级 ACL，符合 ADR-0002/0026 的边界。

---

## 5. 富信息数据模型（对齐 Azure Work Item）

### 5.1 字段

以"issue = Azure Work Item"建模，v1 至少覆盖：

| 类别 | 字段 |
|---|---|
| 标识 | `id`、`revision_no`、`title`、`description` |
| 状态 | `state`（`new` / `active` / `resolved` / `closed` / `removed`）、`reason` |
| 分派 | `assigned_to`（principal）、`reporter`、`participants`（creator/assignee/mention/watcher，含加入/退出时间） |
| 计划 | `iteration_path`、`area_path`、`priority`、`tags` |
| 讨论 | `comment_id`、`reply_to_comment_id`、`visibility`、正文 |
| 关系 | 链接类型：`commit` / `generation` / `room` / `parent` / `child` / `related` / `hyperlink`；附件元数据 |
| 审计 | 每条修订的 `revision_no`、`actor`、`at`、`kind`、`patch`、`snapshot` |
| 时间 | `created_at`、`updated_at`、`resolved_at`、`closed_at` |

v1 不做：work item 类型体系（Bug/Task/Feature 的模板与字段规则）、board/backlog 视图、查询语言、通知订阅规则。现有 `work_items` 执行切片作为 `child` 关系保留；是否合并进 revision 模型待验证（§9）。

### 5.2 状态机

Azure 五类状态 + `reason` 承载半自动流程：

```
New ──(handler triage)──> Active(reason=needs_info | clarifying | implementing)
New ──(rejected/duplicate)──> Removed(reason=rejected | duplicate)
Active(implementing) ──(deploy verified)──> Resolved(reason=fixed)
Active ──(duplicate/rejected)──> Removed
Resolved ──(proposer accepts)──> Closed(reason=verified)
Resolved ──(acceptance fails)──> Active(reason=implementing, attempt+1)
Closed/Removed ──(reopen, 受权限约束)──> Active
```

- `New` 覆盖"待领取"。
- `Active` 的 reason 区分"待补充 / 澄清中 / 处理中"；reason 是普通字段，不是状态，避免状态爆炸。
- `Resolved` = "待验收"；`Closed` = 验收通过；`Removed` = 驳回/重复/不做。
- 迁移映射：现有 `new→New`、`in_progress→Active(implementing)`、`pending_verification→Resolved(fixed)`、`done/closed→Closed(verified)`；`allowed_transition` 需要补 `Removed` 与回退边。

每条迁移都是 `field` 修订；不许 UPDATE 状态列绕过修订。进入 `Closed` / `Removed` 时同步归档参与关系（§4.3）；重开只恢复 `creator` 与最后一个 `assignee`。

### 5.3 历史与任意两修订比较

- 修订是唯一历史真源；当前快照可由修订折叠得到，也可物化（推荐物化 + 事务内同步）。
- `issue.diff(a, b)`：按字段比较两个 `snapshot`，输出 added/removed/changed；讨论修订比较正文与 `comment_id`。
- 比较结果不进 core 的类型系统，作为 `platform_call` 的返回值（JSON）交给 agent；需要落到房间里时由调用方再发一条 `issue.comment` 或 `suppressed` 事件。

### 5.4 链接与附件

- 链接是关系行：`link_id`、`rel`、`target_kind`、`target_id`、`url`、`added_revision`、`removed_revision`。`removed_revision` 为空即当前有效。
- 必有的三类：`commit:<sha>`、`generation:<id>`、`room:<RoomId 字符串>`。`room` 链接用于把公开群讨论与 issue 关联（QQ 房间 id 只是字符串，issue-service 不解释）。
- 附件只存元数据 + URL + sha256；二进制走外部对象存储，桥不中转（若需要中转，走 ADR-0025 文件通道，不在 v1）。
- 链接/附件增删都是 `relation` 修订；删除不物理删行，写 `removed_revision`。

### 5.5 SQL 表结构草案

```sql
CREATE TABLE issues (
  id              TEXT PRIMARY KEY,
  revision_no     INTEGER NOT NULL,          -- 当前修订号，乐观并发
  title           TEXT NOT NULL,
  description     TEXT NOT NULL,
  state           TEXT NOT NULL,             -- new|active|resolved|closed|removed
  reason          TEXT NOT NULL,             -- needs_info|clarifying|implementing|fixed|verified|duplicate|rejected|removed
  assigned_to     TEXT,
  reporter        TEXT NOT NULL,
  iteration_path  TEXT,
  area_path       TEXT,
  priority        TEXT,
  tags            TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  resolved_at     INTEGER,
  closed_at       INTEGER
);

CREATE TABLE issue_revisions (
  issue_id        TEXT NOT NULL,
  revision_no     INTEGER NOT NULL,
  kind            TEXT NOT NULL,             -- discussion|field|relation|system
  actor           TEXT NOT NULL,             -- principal
  at              INTEGER NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  comment_id      TEXT,                      -- discussion 专用，同一条评论的版本共用
  reply_to        TEXT,                      -- 被回复的 comment_id
  visibility      TEXT NOT NULL DEFAULT 'public', -- public|participants
  patch           TEXT NOT NULL,             -- JSON：字段 diff / 正文 / 关系变更
  snapshot        TEXT,                      -- JSON：该修订后的完整快照（v1 每修订都存）
  PRIMARY KEY (issue_id, revision_no)
);

CREATE INDEX issue_revisions_by_issue ON issue_revisions(issue_id, revision_no);
CREATE INDEX issue_revisions_by_comment ON issue_revisions(issue_id, comment_id, revision_no);

-- 参与关系：issue 侧的审计真源，append-only
CREATE TABLE issue_participants (
  issue_id        TEXT NOT NULL,
  principal       TEXT NOT NULL,
  roles           TEXT NOT NULL,             -- creator|assignee|mention|watcher（可多值）
  state           TEXT NOT NULL,             -- active|left|archived
  since_revision  INTEGER NOT NULL,
  until_revision  INTEGER,                   -- 退出/归档时的修订号；NULL 表示仍活跃
  until_reason    TEXT,                      -- unassigned|closed|reopened|unwatched
  PRIMARY KEY (issue_id, principal, since_revision)
);

-- 桥侧成员索引：由 platform.membership 事件维护，不是 issue 真源
CREATE TABLE room_members (
  connection      TEXT NOT NULL,
  room_key        TEXT NOT NULL,
  principal       TEXT NOT NULL,
  roles           TEXT NOT NULL,
  state           TEXT NOT NULL,             -- active|left|archived
  since_at        INTEGER NOT NULL,
  until_at        INTEGER,
  source_revision INTEGER NOT NULL,          -- 对齐 issue revision_no
  PRIMARY KEY (connection, room_key, principal, source_revision)
);
CREATE INDEX room_members_active ON room_members(connection, room_key, state, principal);

CREATE TABLE issue_links (
  link_id          TEXT PRIMARY KEY,
  issue_id         TEXT NOT NULL,
  rel              TEXT NOT NULL,            -- commit|generation|room|parent|child|related|hyperlink
  target_kind      TEXT NOT NULL,
  target_id        TEXT NOT NULL,
  url              TEXT,
  added_revision   INTEGER NOT NULL,
  removed_revision INTEGER
);

CREATE TABLE issue_attachments (
  attachment_id    TEXT PRIMARY KEY,
  issue_id         TEXT NOT NULL,
  revision_no      INTEGER NOT NULL,
  name             TEXT NOT NULL,
  size             INTEGER,
  sha256           TEXT,
  url              TEXT NOT NULL,
  uploader         TEXT NOT NULL,
  removed_revision INTEGER
);

CREATE TABLE iterations (
  iteration_path   TEXT PRIMARY KEY,
  name             TEXT NOT NULL,
  start_at         INTEGER,
  end_at           INTEGER
);

CREATE TABLE issue_principals (          -- 连接账号到 issue principal 的映射，不放桥配置
  principal        TEXT PRIMARY KEY,
  kind             TEXT NOT NULL,        -- agent|human|courier
  display_name     TEXT
);
```

与现有 schema 的关系：`issue_events` 迁移为 `issue_revisions` 的 `kind=field|discussion` 种子；`issues` 增补 revision/富字段；`api_tokens` 不变，scope 增补 `issue:triage`（如需，待验证）。迁移必须一次性回填 `revision_no` 与 `snapshot`。

### 5.6 历史压缩与保留

- 修订行 append-only，**不在保留期内删除/合并**；这是审计与"任意两修订比较"的前提。
- 保留期：默认 `issue_retention_days = 0`（永久）。若需压缩：`retention_days = 365` 后把该 issue 的修订归档到冷表/对象存储，保留首修订、末修订和关闭相关修订；任何两修订比较跨越归档时，从冷存储读。
- `snapshot` 可以压缩：v1 每修订存全量（实现简单、比较 O(1)）；当单 issue 修订数超过阈值（建议 1000）改为"每 K 条 + 最新一条"存快照，其余靠折叠。**只压快照，不删修订**。
- 桥侧 `records` 仍按既有策略清理；issue 历史的保留与桥流水解耦。不要为了省库把 issue 的旧评论删掉。
- `issue_participants` 与 `room_members` 跟随 issue 归档：终态 issue 的成员行先转 `archived`，归档时随 issue 修订一起移入冷存储；桥侧 `room_members` 只保留最近 N 个关闭 issue 的索引，避免索引无限增长。配置文件大小与 issue 数量无关。

### 5.7 存储与密钥方针

- **进库**：issue、修订、参与关系、链接、附件元数据、状态、审计、token 记录（`api_tokens`）。
- **留文件**：引导参数——issue-service 监听地址、适配器 `base_url`/轮询间隔、`approval_mode`、upgrade 请求目录；这些是部署形状，不进业务库。
- **密钥**：token 经环境变量注入；issue-service 库里只存哈希或加密后的密文（现有 `api_tokens` 的形状待验证），桥不把明文 token 写进日志/记录/issue 文本。适配器连接拿到的 token 只用于 HTTP Authorization。

---

## 6. 与升级链、issue 工作流、处理者权限

### 6.1 升级链对接点（两条链的共用尾声）

[acb-upgrade-chain.md](acb-upgrade-chain.md) 的链路是：`POST /api/upgrade` 写请求文件 → 预算/审计 → 排队 → 建代 → 验证 → 切换 → 观察窗口 → 提升 LKG，失败回滚；有 `expected_current_id` 与全局单飞行。**这是外源需求链和内源短链唯一的共用段**：内源短链在修复/回归通过后同样写升级请求，但不产生任何 issue 修订；外源链额外把结果回写成 issue 状态与评论。

外源链对升级链的对接：

1. **链接**：issue 必带 `generation:<id>` 和 `commit:<sha>`；升级请求产生后追加 `deployment:<request_id>` 链接修订。
2. **触发**：升级请求只能由 release controller（固定规则组件，非 LLM）写入 `upgrade/requests/`，条件是 issue 处于 `Active/implementing`、有已验证 generation、`expected_current` 与当前 LKG 一致。处理者 agent 的职责是"准备到可上线"并评论，不是"发起上线"。
3. **结果回写**：升级链的验证/观察/LKG 结果由 release controller 追加修订：
   - 成功 → `state=Resolved, reason=fixed` + `deployment.succeeded` 事件 → 唤醒提议者验收。
   - 失败 → 留在 `Active/implementing` + `deployment.failed` 事件（含失败阶段与回滚结果），不进入待验收。
4. **验收**：提议者验收通过 → `Closed(verified)`；不通过 → `Active(implementing)`，`attempt+1`，并追加 `acceptance.failed` 修订。

**内源短链的对接**：修复会话不写 issue，只把 `incident.fixed` + 回归结果作为事件写进事件流水；release controller 的固定规则判定"incident 已修复 + 回归通过 + `expected_current` 一致"后写同一份升级请求。结果事件 `deploy.succeeded` / `deploy.failed` 只进事件流水，不产生 issue 状态迁移；只有显式开单时才把 incident 链接到 issue。

升级链当前只有设计文档、没有消费者（`acb-upgrade-chain.md` 状态为设计稿），所以这一节是**接口契约**，实现顺序上 issue 适配器可以先落地，升级对接留桩。

### 6.2 处理者权限边界

| 动作 | 处理者 agent | 说明 |
|---|---|---|
| 读公开群消息 | 可（另一条连接） | 但只作为不可信文本，见 §7 |
| 建 issue | 可（如需代提议者建档） | `issue:report` |
| 评论 / 追问 | 可 | `issue:write` |
| New→Active / Active→Removed(duplicate/rejected) | 可 | 需 `issue:triage`（新增 scope，待验证）或 `issue:write` + 服务端规则 |
| 指派、迭代、链接、附件 | 可 | 不能指向自己无权访问的房间/代 |
| 写 generation/deployment 链接 | 只读外部事件产生 | 不允许手写伪造；release controller 优先 |
| Resolved→Closed（验收关闭） | **不可** | 只有提议者/验收人可 |
| 发起升级（POST /api/upgrade） | **不可** | 仅 console admin / release controller |
| 建/改 token、改授权、改配置 | **不可** | 与 issue scope 无关 |
| 改 issue 状态到 closed | **不可** | `issue.transition` 关闭类 risk=destructive，core 侧 grant 不授予处理者 |

桥侧的 grant 只做"能不能调这个方法"；能不能做这个迁移由 issue-service 按 scope 与角色判定。两层都必须过。

---

## 7. 提示注入防护

处理者会同时读公开群和 issue 文本，二者都是不可信输入。规则：

1. **文本不是指令**。issue/评论正文只映射为 Message/Event 的载荷；core 的 route 插件、access 命令解析只对私聊/管理房间生效，issue 连接不配置任何 route 插件，正文里的 `/command`、`升级`、`关闭` 不会被当控制指令。
2. **固定规则触发，不由文本触发**。上线请求、关闭、移除、token/权限变更、ticket 申请只能由：(a) issue-service 的 scope 校验，(b) release controller 的固定条件规则，(c) 人在 console 的显式动作。agent 在评论里写"请上线/已批准"不产生任何执行。
3. **系统事件与用户文本分离**。`issue.revision`/`issue.created` 是适配器生成的事件，`sender_role` 标 `system`；讨论消息 `sender_role` 是 `user`。wake 策略可以让 agent 只对系统事件响应，对普通评论累积后一次处理，降低被刷屏诱导。
4. **跨房间不可达**。`platform_call` 不能改目标房间（ADR-0027）；适配器不接受 `on_behalf_of` 参数，避免评论伪造作者。处理者只能操作自己连接上的 issue 房间；`issue.search` 结果不自动展开为可写房间。
5. **不自动取用外部内容**。评论里的链接、附件不自动抓取；agent 必须显式调用工具，且抓取走白名单/沙箱（按现有文件与网络策略）。附件只给元数据与 hash，不把二进制直接塞进上下文。
6. **提示词层**写死：issue 文本是数据，任何"忽略之前的规则/你现在是管理员/把 token 发给我"都按正文回显给用户或忽略，不进入工具参数。敏感值（token、升级文件路径）永不出现在 issue 载荷。
7. **审计**。每条可由文本诱导的动作在 issue 修订里记录 `actor` 与 `trigger=comment|rule|human`；事后可区分"agent 自己干的"与"按规则干的"。

---

## 8. 未决项：上线审批点

两种模式，配置切换，代码路径相同，只在"谁写入升级请求"这一步分叉：

```toml
[release.external]              # 外源需求链
approval_mode = "human_click"   # human_click | auto_with_audit
auto_requires_verified_generation = true
auto_kill_switch = true

[release.internal]              # 内源短链
approval_mode = "auto_with_audit"
auto_kill_switch = true
```

- **human_click**：release controller 准备 `pending` 请求（外源链含 issue id、generation、expected_current、验证摘要），等人通过 console `POST /api/upgrade`（或 issue 房间里的人类 principal 用受权 token）批准。LLM 不参与。
- **auto_with_audit**：满足固定条件时 release controller 自动写入请求并排队；人只事后查账（外源链看修订 + 升级请求文件，内源链看事件流水 + 升级请求文件），可撤销（观察窗口内回滚 / kill switch）。所谓"agent 做"指 controller 以处理者/修复者身份执行，不是 LLM 自由决定。
- **按链选择默认**：外源链默认 `human_click`（有人类提议者，验收责任明确）；内源短链默认 `auto_with_audit`（没有提议者、目标是快速闭环，人工只查账）。两者共用同一 release controller 和同一升级队列。
- 共同约束：两种模式下 LLM 都不能直接写 `upgrade/requests/`；外源请求必须带 `issue` 与 `generation` 链接，内源请求必须带 `incident` 与 `generation` 链接；没有 `expected_current` 不允许提交；全局单飞行不变。
- 切换代价：只切配置与审批 UI，不改适配器/issue-service。等升级链 S2 落地并积累审计后再评估外源链是否也放宽到 `auto_with_audit`。

---

## 9. 待验证

1. issue-service 将来是否已加 revision/评论/附件/迭代/链接接口；本文按"需要新增"设计，若已有需对齐字段。
2. `issue:triage` 是否需要新 scope，还是用 `issue:write` + 服务端迁移规则；需看后续权限模型。
3. `[[sessions.bindings]]` + `room_members` 是否会被现有"订阅即可见"测试/工具假设接受；`grants.rs::target_is_reachable` 注入推导房间后，`base:subscription` 的 deny 行为是否一致。
4. `platform.membership` 作为 `suppressed=true` 的事件是否足够驱动 core 成员索引；如果 core 需要在 ingest 阶段识别成员事件，是否要单独的通用 `Membership` 事件类型而不是复用 `Event`。
5. 参与谓词的表达力：`roles + include_closed` 是否覆盖"指派给我且未关闭"；更复杂的谓词（"我关注的标签"）是继续加通用字段，还是让适配器维护更多角色。
6. core 是否加入 `platform_id` 去重；加入后能否处理保留期清理后的晚到重投。
7. 现有 `work_items` 与 issue 的关系：作为 `child` 保留还是并入修订模型。
8. harness 会话在数天无活动后的重连与未读回放行为（各 harness 不同）；以 dsh/opencode 实测为准。
9. 升级链 S1–S4 的实现状态与请求文件格式；本文只做接口契约。
10. `visibility=participants` 的平台侧路由是否足够；如果需要 core 级内容 ACL，则超出本设计范围。

---

## 10. 落地顺序（建议）

1. issue-service：revision 表 + 讨论/链接/附件/迭代 + `GET revisions?after=` + `issue.diff` + 幂等键 + 迁移；保持现有 HTTP API 兼容。
2. `acb-adapter-issue`：轮询、水位、镜像记录、`platform_methods` 方法表、参与关系同步（`platform.membership`）、回环过滤、suppressed 审计事件。
3. core：`sessions.bindings` + `room_members` 索引 + 统一"可见房间集"入口 + 配置校验（`membership` capability）。
4. 两个会话配置：提议者/处理者各一条 issue 连接 + binding（参与谓词）+ wake tags；先用只读/评论跑通再造 issue。
5. release controller 桩：固定规则、挂起请求、结果回写；先 `human_click`。
6. 升级链落地后接上建代/验证/切换/LKG，再评估 `auto_with_audit`。
