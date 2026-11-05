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
| `closed_at` | int? | `done` / `closed` 时写入 |
| `version` | int | 乐观并发；每次更新 +1 |

`source.type` 封闭枚举：`acb_session` / `agent` / `webhook` / `console` / `import`。
`source.ref` 是来源侧 id（会话 id、agent 运行 id、webhook 事件 id）。

### 1.2 WorkItem

WorkItem 是 Issue 的**执行切片**，不是第二套 issue：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | ULID | |
| `issue_id` | ULID? | 空 = 独立工作项；非空 = 从该 issue 拆出 |
| `title` | text | |
| `body` | text | 执行说明/上下文；从 issue 拆出时默认复制 issue.body |
| `priority` | enum | 默认继承 issue；独立工作项默认 `P2` |
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

WorkItem 不重复 Issue 的全部字段：`issue_id` 非空时，`body/priority/tags/source/related`
默认从 Issue 复制；独立工作项（`issue_id` 为空）必须自带 `body` 与 `source`。

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

### 1.4 状态机

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
- `issue_my_work(assignee?, status?)`（当前 principal 的待办）

工具不直接读 SQLite；全部走 issue 服务 HTTP API。ACB 侧只做身份映射
（session/principal）与参数校验，不做状态机判定。

### 5.2 通知接入

- 第一阶段：ACB 启动时由接入进程拉一次 pending outbox，按订阅投递；外部
  webhook 独立直投。
- 第二阶段：增加 `Wake` provider（外部进程/WASM 载体），按游标轮询 outbox，
  有 pending 就唤醒对应 ACB 会话；outbox 仍是唯一真相，provider 不保存状态。
- 明确不做「issue 服务直接往 ACB 内部写记录」：那会把 issue 绑回 ACB 进程，
  也绕过 ACB 的投递/权限管线。

## 6. 演进路线

### 阶段 0：设计（当前）

- 只写本文件；不改 ACB，不建库，不写代码。

### 阶段 1：独立服务 MVP

- SQLite（WAL）+ HTTP API + CLI；Issue/WorkItem/状态机/事件/outbox；
- LIKE + 结构化索引；webhook 订阅；无 ACB 接入；
- 外部 agent 已经可以创建、查询、回报结果。

### 阶段 2：ACB 工具面

- `acb-plugin-process-tools` 接入层，暴露第 5.1 节工具；
- ACB 启动补投 pending outbox；会话订阅 + 外部 webhook；
- 控制台只读列表（不做完整 Web UI）。

### 阶段 3：检索与唤醒增强

- FTS5（trigram）或等价中文全文索引；
- `Wake` provider 按游标唤醒 ACB 会话；
- 保存查询、批量操作、dead-letter 重放工具。

### 明确不做（除非单独立项）

- 完整 Web UI（只做控制台只读列表）；
- 多租户/细粒度 RBAC（先单实例、单信任域，用 token + principal 白名单）；
- GitHub 双向同步（issue 只存 `source.ref`，不同步评论/状态）；
- 把 issue 表塞进 ACB core 或 ACB 的 `state_dir`；
- 用 ACB task 表代替 issue 状态机。

## 7. 风险与开放问题

- **身份映射**：ACB session/principal 与 issue principal 的映射表放哪？第一版
  由接入层配置静态映射，后续再做绑定流程。
- **outbox 重试上限**：dead-letter 的 N 与保留期需要按实际量定；先定 8 次/
  7 天，可配置。
- **webhook 签名**：与现有 `X-Signature` 规范对齐还是新起 `X-Issue-Signature`，
  阶段 2 定。
- **中文全文**：如果 FTS5 trigram 不可用，阶段 3 可能改用外部索引；
  阶段 1 的 LIKE 行为必须保持稳定。
- **删除**：第一版不做物理删除，只 `closed` + 归档；避免 issue 事件链断裂。
