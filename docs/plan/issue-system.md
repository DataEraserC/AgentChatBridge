# Issue / WorkItem 系统设计（独立系统 + ACB 接入）

状态：**设计稿（2026-11-05）**。只定模型、边界与接入方式，不写实现。

## 0. 一句话结论

Issue 系统做成**独立进程 + 独立 SQLite 库 + HTTP API**；ACB 只通过现有扩展点接入，
不在 core 里塞 issue 表、状态机或通知队列。ACB 停机时 issue 仍可被外部 agent 读写，
事件先落库，ACB 启动后补通知。

**为什么不是 ACB 内置**：

- issue 的生命周期比 ACB 进程长。ACB 重启、升级、回滚、被 supervisor 停掉时，
  issue 不能跟着消失；内置表会把 issue 的可用性绑在 ACB 的可用性上。
- 写 issue 的人/agent 不一定经过 ACB。外部 agent、脚本、未来的 Web UI 需要一条
  不依赖 ACB 的 HTTP 入口。
- issue 的检索、订阅、通知重试是独立的一类工作；塞进 core 会让 core 多出一套
  与消息流水无关的持久化和调度逻辑。
- 独立库可以单独备份/迁移/清理；ACB 的 `state_dir` 不该混进 issue 的全文索引。
- ACB 的扩展点（`Tool` / `Wake`）已经足够窄；接入层做成进程/插件，升级节奏与
  core 解耦。

**为什么 ACB 挂了不能丢 issue 事件**：

- 状态变更 + 结果 + 通知事件在 issue 服务的**同一个事务**里落库；通知只是
  outbox 里的一行，不依赖 ACB 在线。
- ACB 不在时 outbox 保持 pending；ACB 启动后按游标补投，投递成功才标记 delivered。
- 没有内存队列、没有「先通知后落库」；重复投递由幂等键挡住。

## 1. 实体模型

### 1.1 Issue

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | ULID | 全局唯一，按时间可排序 |
| `title` | text | 一行标题 |
| `body` | text | 正文（Markdown 原文，不渲染） |
| `status` | enum | `new` / `in_progress` / `pending_verification` / `done` / `closed` |
| `priority` | enum | `P0` / `P1` / `P2` / `P3`，默认 `P2` |
| `tags` | string[] | 规范化小写标签，多对多表 |
| `assignee` | string? | 负责人/agent 的 principal id；空 = 未指派 |
| `created_at` / `updated_at` | int ms | 写入时由服务端生成 |
| `source` | object | `{type, ref}`；type 见下 |
| `reporter` | string | 报告人 principal id（人/会话/agent） |
| `related` | object? | `{room_id?, session_id?, message_id?, ticket_id?, url?}` |
| `resolution` | text? | 完成/关闭时的结论 |
| `verification_owner` | string? | 验收人；空 = 按默认规则推导 |
| `verification_due_at` | int? | 进入 `pending_verification` 时写入 |
| `verification_escalated_at` | int? | 超时升级到管理员时写入 |
| `closed_at` | int? | `done` / `closed` 时写入 |
| `version` | int | 乐观并发；每次更新 +1 |

`source.type` 封闭枚举：`acb_session` / `agent` / `webhook` / `console` / `import`。
`source.ref` 是来源侧 id（会话 id、agent 运行 id、webhook 事件 id）。

### 1.2 WorkItem

WorkItem 是 Issue 的**执行切片**，不是第二套 issue：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | ULID | |
| `issue_id` | ULID | **强制非空**：WorkItem 只是 Issue 的执行切片，不能独立存在 |
| `title` | text | |
| `body` | text | 执行说明/上下文；从 issue 拆出时默认复制 issue.body |
| `priority` | enum | 默认继承 issue |
| `tags` | string[] | 默认继承 issue，可追加执行标签 |
| `status` | enum | 与 Issue 同一状态机 |
| `assignee` | string? | 执行者 |
| `source` | object | `{type, ref}`，同 Issue |
| `related` | object? | `{room_id?, session_id?, message_id?, url?}` |
| `result` | text? | 执行结果/PR/提交号 |
| `verification` | object? | `{state, by, at, note}`；state = `pending` / `passed` / `failed` |
| `attempts` | int | 重开/重试次数 |
| `created_at` / `updated_at` | int ms | |

一个 Issue 可以有多个 WorkItem；Issue 的 `done` 需要至少一个 WorkItem
`verification.state=passed`，或 Issue 本身被人工确认（`resolution` 写明理由）。

WorkItem 不重复 Issue 的全部字段：`body/priority/tags/source/related` 默认从 Issue
复制。`issue_id` **不可为空**：需要独立存在的东西先建 Issue，再拆 WorkItem；
这样 `issue_search` / 订阅永远只有 Issue 一个主对象，不会出现「半套 issue」。

### 1.3 与 ACB `task` 的区别

| | ACB task | Issue / WorkItem |
|---|---|---|
| 归属 | ACB 进程内运行时 | 独立 issue 服务 |
| 生命周期 | 轮次/进程级，记录 queued/running/succeeded/failed | 持久，跨 ACB 重启 |
| 用途 | 异步执行记录、`wake_on` 合成事件 | 问题跟踪、验收、订阅、对外可见 |
| 谁写 | 桥内部 task handler | 人/agent/外部系统 |
| 失败处理 | 任务状态 + 唤醒 | 状态机 + 验证 + 通知 outbox |

**不合并**：task 是「这次执行成功没有」，workitem 是「这件事做完没有、谁验收」。
一个 workitem 可以产生多个 task（重试/分步），但 task 不反向生成 issue。

### 1.4 验收者规则（`pending_verification`）

- `verification_owner` 可显式指定；空时按默认顺序：
  1. **创建者**（`reporter`）——如果它是人/可验收 principal；
  2. **指派人**（`assignee`）——但不能与执行者自验收：如果 `assignee == reporter`，
     跳过；
  3. **管理员**（`admin` principal 组）。
- 执行者不能默认验收自己的 WorkItem；如果默认候选就是执行者，直接升级管理员。
- 进入 `pending_verification` 时写 `verification_due_at = now + 72h`。
- **超时行为**（不让 issue 永久卡住）：
  - T+72h 未验收 → 事件 `verification_escalated`，改派/通知管理员，状态仍
    `pending_verification`；写 `verification_escalated_at`；
  - T+168h（再 96h）仍未验收 → 自动 `closed`，`resolution="verification_timeout"`，
    可 reopen；不自动标 `done`，避免把没验收的工作算完成。
- WorkItem 验收用同一规则；Issue `done` 需要至少一个 WorkItem
  `verification.state=passed`，或管理员显式确认并写 `resolution`。

### 1.5 状态机

```text
new ──assign/triage──> in_progress ──提交结果──> pending_verification
  │                         │                         │
  │                         │                    验收通过
  │                         │                         v
  │                         │                       done
  │                         │                         │
  └────────close────────────┴────close───────────────┘
                                                    closed
done / closed ──reopen──> in_progress
```

允许的迁移：

- `new -> in_progress`（指派或开始）
- `in_progress -> pending_verification`（执行者提交结果，必须带 `result`）
- `pending_verification -> done`（验收通过；写 `verification`）
- `pending_verification -> in_progress`（验收失败，写原因，`attempts + 1`）
- `new|in_progress|pending_verification -> closed`（重复/不修/无效，必须写 `resolution`）
- `done|closed -> in_progress`（重开；`reopened` 是事件，不是单独状态）
- 禁止 `new -> done`、`closed -> done`；所有迁移写 `issue_events` 一行。

状态变更必须带 `expected_version`；版本不匹配返回 `409 stale_version`，不静默覆盖。

## 2. 检索

### 2.1 查询维度

- 状态：单个或集合；
- 标签：AND/OR（默认 AND）；
- 指派人：principal id 或 `unassigned`；
- 时间范围：`created_at` / `updated_at` 的 `since` / `until`；
- 关联对象：`room_id` / `session_id` / `message_id`；
- 全文：标题 + 正文；
- 排序：`updated_at desc`（默认）、`priority`、`created_at`；
- 分页：游标 `(updated_at, id)`，`limit` 默认 20、上限 100。

### 2.2 索引选择：先 LIKE，后 FTS5

**判断：第一阶段用结构化索引 + `LIKE`，不直接上 FTS5。**

理由：

- 预期量级小（先按千级 issue 估算），结构化过滤（状态/标签/指派人/时间）能覆盖
  大多数查询；全文只是补充。
- 中文分词是 FTS5 的坑：默认 `unicode61` 不切中文，`trigram` 需要编译时启用且
  对短查询/标点不稳定；先上 FTS5 会把「搜不到」变成索引问题。
- `LIKE` 可以先把 `title/body` 规范化成小写 `search_text`，行为可预测，迁移成本低。
- FTS5 不是永远不做：当 issue 超过约 1 万条、或全文查询成为主要入口时，再建
  `issue_fts`（trigram）并用触发器同步；届时保留 `LIKE` 作为 fallback。

第一阶段索引：

```sql
CREATE INDEX issue_status_updated ON issues(status, updated_at DESC, id);
CREATE INDEX issue_assignee_updated ON issues(assignee, updated_at DESC, id);
CREATE INDEX issue_updated ON issues(updated_at DESC, id);
CREATE TABLE issue_tags(issue_id, tag, PRIMARY KEY(issue_id, tag));
CREATE INDEX issue_tags_tag ON issue_tags(tag, issue_id);
-- search_text = lower(title || '\n' || body)，查询用 LIKE '%kw%'
```

### 2.3 鉴权

- **token 模型**：按 principal/agent 签发 bearer token；表
  `api_tokens(id, principal_id, name, scopes, created_at, expires_at, revoked_at,
  token_hash, last_used_at)`。token 明文只在创建时返回一次，库里存 SHA-256 哈希。
- **scope**：`issue:read` / `issue:write` / `issue:report` / `issue:subscribe` /
  `issue:verify` / `issue:admin`。外部 agent 默认只拿 `issue:read` +
  `issue:report`（只能回报自己名下的 WorkItem）；控制台只读 token 只有
  `issue:read`。
- **过期与轮换**：默认 90 天过期；轮换 = 先签发新 token，保留旧 token 7 天重叠期，
  再 `revoked_at`。所有事件记录 `token_id`，便于审计。过期/撤销返回 401。
- **无 token / 无效 token** 一律 401（`WWW-Authenticate: Bearer`）；token 有效但
  scope 不够返回 403，不泄露资源是否存在。
- **ACB 自身身份**：ACB 接入进程使用专用 service token
  `principal=acb-bridge`，scope = `issue:read` / `issue:write` /
  `issue:report` / `issue:subscribe`（不含 `issue:admin`）。它可以在请求体里带
  `on_behalf_of`（`session:<id>` / `principal:<id>`）；只有 `acb-bridge` token
  允许这个字段，服务端会把它写进 `issue_events.actor`，但权限仍按 service token
  判定。控制台/Webhook 各自独立 token/secret，不复用 ACB service token。

## 3. 订阅

### 3.1 订阅者

- `session:<acb-session-id>`：ACB 会话；
- `principal:<id>`：人/agent；
- `webhook:<id>`：外部 HTTP 端点。

### 3.2 订阅粒度

- 单 issue：`filter = {issue_id}`；
- 按标签：`filter = {tags:[...], tag_mode:"and"}`；
- 按查询：保存一条查询表达式（状态/标签/指派人/时间/全文），创建/更新时求值；
- 订阅有 `active`、`created_at`、`last_delivered_event_id`（游标）。

### 3.3 通知渠道

- ACB 会话：由 ACB 接入层的 `Wake` provider 轮询 outbox，命中后开一轮；
- 控制台：控制台只读 API，轮询未读；
- 外部 webhook：issue 服务直接 POST，失败重试，签名 `X-Issue-Signature`。

通知事件类型：`issue.created` / `issue.updated` / `issue.status_changed` /
`issue.comment_added` / `workitem.result_reported` / `workitem.verification_changed`。

### 3.4 阶段 1 的补投路径：工具拉取 + 显式确认

阶段 1 没有 `Wake` provider，因此**不采用**「ACB 启动时把未投递事件作为
`system` 记录注入会话流水」：

- 那需要 core 新增一个「插件写系统记录」的 API，等于把 issue 又绑回 ACB；
- 阶段 1 的 ACB 接入只有 Tool provider，没有权限/通道写流水；
- 一旦 ACB 注入失败，issue 侧的 pending 状态和 ACB 侧记录会分叉。

阶段 1 选 **(b) 工具拉取 + 显式确认**：

1. `issue_my_work` / `issue_get` 的返回里带
   `unread_events: { count, event_ids, summaries, redelivery }`；
2. 第一次读到 pending 事件时，服务端用一条原子
   `UPDATE event_deliveries SET state='claimed', claim_id=?... WHERE state='pending'`
   抢占；同一条事件只有一个 claim 能成功；
3. agent 必须显式调用 `issue_my_work({ ack_event_ids: [...] })` 确认；确认是
   幂等的，重复 ack 返回 `already`；
4. 未 ack 的事件下次调用会再次返回（`redelivery` + 1），这是 at-least-once，
   事件本身是只读通知，重复看到不会产生副作用。

**幂等账本**：

```sql
CREATE TABLE event_deliveries(
  event_id     TEXT NOT NULL,
  subscriber_id TEXT NOT NULL,
  channel      TEXT NOT NULL,          -- acb_session / console / webhook
  state        TEXT NOT NULL,          -- pending / claimed / delivered / dead
  claim_id     TEXT,
  attempts     INTEGER NOT NULL DEFAULT 0,
  claimed_at   INTEGER,
  delivered_at INTEGER,
  ack_at       INTEGER,
  PRIMARY KEY(event_id, subscriber_id, channel)
);
```

- 启动补投（未来阶段 2）与正常投递都只能走同一条 claim 路径：唯一主键 +
  `pending -> claimed` 的原子更新保证同一条事件只有一个投递者；
- claim 超过 5 分钟未 ack 会回到 pending，允许重投；
- `delivered_at` 只在 agent/通道显式确认后写入；写成功即不会再出现在 pending；
- 事件 id 随通知正文一起返回，ACB 侧按 `event_id` 去重。

阶段 2 若增加「ACB 启动注入 system 记录」，也必须复用这张账本；不能另起一套
「启动投递」状态，否则同一条事件会在启动补投和正常投递之间重复。

## 4. 外部 agent 完成回报（ACB 不在也不丢）

外部 agent 调用：

```http
POST /v1/issues/{id}/result
{
  "work_item_id": "...",
  "status": "pending_verification",
  "result": "...",
  "idempotency_key": "agent-run-123:issue-...:result"
}
```

一个事务里写：

1. `work_items.result` + `status`；
2. `issue_events`（谁、从什么状态到什么状态、结果摘要）；
3. `outbox` 每个匹配订阅一行，`delivery_state=pending`。

ACB 不在时：

- 事务照常提交，outbox pending；
- 外部 webhook 仍可直接投（不经过 ACB）；
- ACB 启动后，接入层按 `subscriber + last_delivered_event_id` 拉取 pending；
- 只有 ACB 确认接收后才把 outbox 标 `delivered`；失败按指数退避重试，
  超过 N 次进 dead-letter，保留原事件。

**幂等**：

- `idempotency_key` 在 `issue_events` 上唯一；重复请求返回原结果，不重复写事件；
- outbox 唯一键 `(event_id, subscriber_id, channel)`；重复投递由接入层游标 +
  唯一键双重挡住；
- 通知内容带 `event_id`，ACB 侧按 `event_id` 去重。

## 5. ACB 接入方式

**选择：外部进程 Tool provider（`acb-plugin-process-tools` 载体）+ 后续 Wake provider。**

不选 core 内置、不选 access provider、不选 in-process Lua/WASM 工具：

- core 内置会把 issue 生命周期绑在 ACB 上，违背第 0 节；
- access provider 只决定放行/拒绝，不能提供 issue 工具；
- in-process 插件随 ACB 进程死，无法在 ACB 停机时继续接外部 agent 的 HTTP 写入；
- process-tools 载体已有 stdio JSON-RPC、超时、崩溃退避、Unhealthy 标记，
  接入层只是一个薄 HTTP 客户端，升级/重启不影响 issue 库。

### 5.1 工具面（通过 `plugin_tools` / `plugin_tool_call` 暴露）

- `issue_create(title, body, priority?, tags?, related?)`
- `issue_get(id)`
- `issue_search(query?, status?, tags?, assignee?, since?, until?, cursor?, limit?)`
- `issue_update(id, expected_version, status?, assignee?, comment?, resolution?)`
- `issue_subscribe(filter, channel)` / `issue_unsubscribe(id)`
- `issue_report_result(issue_id, work_item_id, status, result, idempotency_key)`
- `issue_my_work(assignee?, status?, ack_event_ids?)`（当前 principal 的待办；
  带 `ack_event_ids` 时显式确认未读事件）

固定 **8 个工具**，不新增 `issue_ack_events`；确认折叠进 `issue_my_work`。
工具不直接读 SQLite；全部走 issue 服务 HTTP API。ACB 侧只做身份映射
（session/principal）与参数校验，不做状态机判定。

### 5.2 工具位预算

- 这 8 个 issue 工具**只通过 `plugin_tools` 索引 + `plugin_tool_call` 调用**，
  不进入 `TOOL_NAMES` / `RESIDENT_TOOLS` / `TOOL_CATALOG`，也不占常驻层或目录层
  的任何位置。
- 当前 core 工具数是 29（8 常驻 + 21 目录）；issue 工具走插件面，**不会变成 37**，
  不与「工具数上限 30」冲突。
- `capabilities scope=session/catalog` 不会列出它们；agent 先 `plugin_tools`
  发现，再 `plugin_tool_call` 调用。
- 如果以后有人想把它们提升进 core 工具表：29 + 8 = 37，直接超过 30 的上限；
  处理方式是**拒绝提升**，保持插件面，或把它们压成两个多动作工具
  （`issue_search` 一个入口、`issue_update` 一个入口）。当前设计不这么做。

### 5.3 通知接入

- 阶段 1：没有 Wake；`issue_my_work` / `issue_get` 拉取 `unread_events`，
  agent 用 `ack_event_ids` 显式确认（第 3.4 节）；外部 webhook 独立直投。
- 阶段 2：增加 `Wake` provider（外部进程/WASM 载体），按游标轮询 outbox，
  有 pending 就唤醒对应 ACB 会话；启动补投也可升级为 system 记录注入，但必须
  复用 `event_deliveries` 账本。outbox 仍是唯一真相，provider 不保存状态。
- 明确不做「issue 服务直接往 ACB 内部写记录」：阶段 1 那会把 issue 绑回 ACB
  进程，也绕过 ACB 的投递/权限管线。

## 6. 阶段 1 验收标准

阶段 1 的目标是「独立服务 + ACB 工具面可用，但没有 Wake」。**不写代码也要能
按下面逐条判断做没做完**：

- [ ] 服务能空库启动、跑迁移、`GET /healthz` 返回 200；重启后数据还在。
- [ ] token：无 token 401、scope 不足 403、过期/撤销 401；轮换重叠期新旧都能用。
- [ ] Issue：创建/读取/更新/关闭；非法状态迁移被拒（422/409）；`expected_version`
      不匹配返回 409，不静默覆盖。
- [ ] WorkItem：`issue_id` 必填；从 Issue 拆出；结果回报带 `idempotency_key`，
      同一 key 调两次只产生一条事件/一条 outbox。
- [ ] 检索：状态/标签/指派人/时间范围/LIKE 全文都可用；游标分页在写入过程中
      不跳条、不重条；中文关键词能命中。
- [ ] outbox：写完结果后**杀掉服务**再启动，pending 事件仍在；webhook 失败会
      重试；ACB 不在不影响落库。
- [ ] ACB 工具面：`plugin_tools` 能看到 8 个 issue 工具；`capabilities` 不列出；
      core 工具数仍 29，不是 37。
- [ ] ACB 补投：ACB 停掉 → 外部 agent 通过 HTTP 回报 → ACB 启动后第一次
      `issue_my_work` 返回 `unread_events` → `ack_event_ids` 确认后不再重复。
- [ ] 验收：`pending_verification` 只有验收者能推进到 `done`；72h 升级管理员、
      168h 自动关闭（测试可用缩短的配置）。
- [ ] 无 ACB 依赖：不启动 bridge，只用 curl/CLI 就能创建 issue、拆 workitem、
      回报结果、搜索、收到 webhook。
- [ ] 文档：HTTP API 示例、token 签发/轮换步骤、备份/恢复演练一次。

**「可用」的判定**：外部 agent 能在 ACB 停机时创建并回报一个 issue；服务重启
不丢事件；ACB 启动后 agent 能看到未读事件并显式确认；所有状态迁移和鉴权都被
测试覆盖。

## 7. 演进路线

### 阶段 0：设计（当前）

- 只写本文件；不改 ACB，不建库，不写代码。

### 阶段 1：独立服务 MVP + ACB 工具面（无 Wake）

- SQLite（WAL）+ HTTP API + CLI；Issue/WorkItem/状态机/事件/outbox；token 鉴权；
- LIKE + 结构化索引；webhook 订阅；
- `acb-plugin-process-tools` 接入层，暴露 8 个工具（第 5.1 节）；
- 通知靠 `issue_my_work` / `issue_get` 拉取 + 显式 ack（第 3.4 节）；
- 不做 Wake、不做启动 system 记录注入。

### 阶段 2：Wake 与会话订阅

- `Wake` provider 按 outbox 游标唤醒 ACB 会话；
- 启动补投可升级为「system 事件注入」（必须复用 `event_deliveries` 账本）；
- 控制台只读列表（不做完整 Web UI）。

### 阶段 3：检索增强

- FTS5（trigram）或等价中文全文索引；
- 保存查询、批量操作、dead-letter 重放工具。

### 明确不做（除非单独立项）

- 完整 Web UI（只做控制台只读列表）；
- 多租户/细粒度 RBAC（先单实例、单信任域，用 token + principal 白名单）；
- GitHub 双向同步（issue 只存 `source.ref`，不同步评论/状态）；
- 把 issue 表塞进 ACB core 或 ACB 的 `state_dir`；
- 用 ACB task 表代替 issue 状态机。

## 8. 风险与开放问题

- **身份映射**：ACB session/principal 与 issue principal 的映射表放哪？第一版
  由接入层配置静态映射，后续再做绑定流程。
- **outbox 重试上限**：dead-letter 的 N 与保留期需要按实际量定；先定 8 次/
  7 天，可配置。
- **webhook 签名**：与现有 `X-Signature` 规范对齐还是新起 `X-Issue-Signature`，
  阶段 2 定。
- **中文全文**：如果 FTS5 trigram 不可用，阶段 3 可能改用外部索引；
  阶段 1 的 LIKE 行为必须保持稳定。
- **删除**：第一版不做物理删除，只 `closed` + 归档；避免 issue 事件链断裂。
