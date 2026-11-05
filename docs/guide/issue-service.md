# Issue 服务：API、token、备份与运维

独立进程 + 独立 SQLite 库，ACB 只通过 `acb-issue-tools`（process-tools provider）
接入。设计见 [`docs/plan/issue-system.md`](../plan/issue-system.md)。

## 启动

```bash
ACB_ISSUE_DB=/var/lib/acb-issue/issues.db \
ACB_ISSUE_LISTEN=127.0.0.1:7800 \
ACB_ISSUE_ADMIN_PRINCIPAL=admin \
acb-issue-service serve
```

- 首次启动自动建表（WAL）。
- `GET /healthz` 公开，不需要 token。
- 其它路径都要 `Authorization: Bearer <token>`。

CLI：

```bash
acb-issue-service migrate
acb-issue-service token-create <principal> <name> <scopes> <days>
acb-issue-service token-rotate <token-id>
acb-issue-service token-revoke <token-id>
acb-issue-service backup <target-path>
```

`token-create` 的 `scopes` 用逗号分隔；省略时给全部 scope。明文 token 只在
这一行输出，库中只存 SHA-256。

## scope

| scope | 用途 |
|---|---|
| `issue:read` | 读 issue/work item/订阅/未读 |
| `issue:write` | 建/改 issue、拆 work item |
| `issue:report` | 回报 work item 结果、ack 事件 |
| `issue:subscribe` | 建/停订阅 |
| `issue:verify` | 验收 work item |
| `issue:admin` | 签发/轮换 token、手动跑维护 |

无 token / 无效 token / 过期 / 撤销 = `401`，并带 `WWW-Authenticate: Bearer`；
token 有效但 scope 不够 = `403`（不泄露资源是否存在）。

- 默认 90 天过期；轮换后旧 token 保留 **7 天**重叠期，新旧同时在有效期内。
- ACB service token 的 principal 固定为 `acb-bridge`，**不能带 `issue:admin`**
  （`token-create` 会直接拒绝）。
- 只有 `acb-bridge` token 能带 `on_behalf_of`；服务端把它写进
  `issue_events.actor`，权限仍按 service token 判定。

## HTTP API

所有请求/响应都是 JSON。错误形状：

```json
{"error": {"code": "stale_version", "message": "..."}}
```

| 方法 | 路径 | 说明 |
|---|---|---|
| `GET` | `/healthz` | 健康检查 |
| `POST` | `/v1/tokens` | 签发 token（admin） |
| `POST` | `/v1/tokens/{id}/rotate` | 轮换（admin） |
| `POST` | `/v1/tokens/{id}/revoke` | 撤销（admin） |
| `POST` | `/v1/issues` | 建 issue |
| `GET` | `/v1/issues` | 搜索/游标分页 |
| `GET` | `/v1/issues/{id}` | 读 issue（含 work items） |
| `GET` | `/v1/issues/{id}/events` | 事件链 |
| `GET` | `/v1/issues/{id}/work_items` | work items |
| `PATCH` | `/v1/issues/{id}` | 更新/状态迁移（必须带 `expected_version`） |
| `POST` | `/v1/issues/{id}/work_items` | 拆 work item |
| `GET` | `/v1/work_items/{id}` | 读 work item |
| `POST` | `/v1/work_items/{id}/report` | 回报结果（带 `idempotency_key`） |
| `POST` | `/v1/work_items/{id}/verify` | 验收（`{"passed":true/false,"note":"..."}`） |
| `POST` | `/v1/subscriptions` | 建订阅 |
| `GET` | `/v1/subscriptions?subscriber=...` | 列订阅 |
| `DELETE` | `/v1/subscriptions/{id}` | 停订阅 |
| `GET` | `/v1/my_work?assignee=&subscriber=&channel=&status=&ack_event_ids=` | 当前待办 + 未读 |
| `POST` | `/v1/events/ack` | 显式确认（`subscriber_id/channel/event_ids`） |
| `POST` | `/v1/maintenance/run_due` | 手动跑超时升级/自动关闭（admin，可带 `now_ms`） |

### 建 issue

```bash
curl -sS http://127.0.0.1:7800/v1/issues \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"title":"登录失败","body":"复现步骤…","tags":["bug"],"assignee":"agent:a"}'
```

`source.type` 封闭枚举：`acb_session` / `agent` / `webhook` / `console` / `import`。

### 搜索与游标

```bash
curl -sS "http://127.0.0.1:7800/v1/issues?status=in_progress&tags=bug&limit=20" \
  -H "Authorization: Bearer $TOKEN"
```

支持 `status`、`tags`（逗号分隔，AND）、`assignee`（`unassigned` 查空）、
`since`/`until`（毫秒，按 `updated_at`）、`q`（标题+正文 LIKE）、`limit`
（默认 20，上限 100）、`cursor`。`next_cursor` 形如 `updated_at:id`，下一页原样带上。

### 状态机

```text
new -> in_progress -> pending_verification -> done
new|in_progress|pending_verification -> closed
done|closed -> in_progress（reopen）
pending_verification -> in_progress（验收失败）
```

- 变更必须带 `expected_version`；不匹配返回 `409 stale_version`。
- 非法迁移 `409 invalid_transition`；`closed` 必须写 `resolution`。
- `in_progress -> pending_verification` 需要至少一个已回报的 work item。
- `pending_verification -> done` 需要至少一个 work item `verification.state=passed`，
  或管理员显式确认并写 `resolution`。
- 验收者默认顺序：`verification_owner` → 创建者（`reporter`）→ 指派人 →
  管理员；执行者不能默认验收自己的 work item。
- 进入待验收写 `verification_due_at = now + 72h`；T+72h 升级管理员，
  T+168h 自动 `closed` + `resolution=verification_timeout`，可 reopen。

### 回报与验收

```bash
curl -sS -X POST http://127.0.0.1:7800/v1/work_items/$WI/report \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"result":"PR #123","idempotency_key":"agent-run-1:result"}'
```

同一 `idempotency_key` 重复调用只产生一条事件/一条 outbox，第二次返回
`{"duplicate":true,"event_id":"..."}`。

```bash
curl -sS -X POST http://127.0.0.1:7800/v1/work_items/$WI/verify \
  -H "Authorization: Bearer $VERIFY_TOKEN" -H 'Content-Type: application/json' \
  -d '{"passed":true,"note":"复现通过"}'
```

### 未读事件与 ack

```bash
curl -sS "http://127.0.0.1:7800/v1/my_work?subscriber=session:s1&channel=acb_session" \
  -H "Authorization: Bearer $TOKEN"
```

返回 `unread_events: {count, event_ids, events, redelivery}`。第一次读取会用
`event_deliveries` 的原子 `pending -> claimed` 抢占；带着 `ack_event_ids`
再调一次会先确认再返回剩余未读。ack 是幂等的，重复确认返回 `already`；
未确认的 claim 超过 5 分钟回到 pending，允许重投。

## ACB 接入

`config.toml`：

```toml
[[tool_plugins]]
id = "issue.tools"
command = "/usr/local/bin/acb-issue-tools"
args = []
timeout_ms = 10000
```

provider 进程只读两个环境变量：

```bash
ACB_ISSUE_URL=http://127.0.0.1:7800
ACB_ISSUE_TOKEN=<acb-bridge service token>
```

它暴露 8 个工具：`issue_create`、`issue_get`、`issue_search`、`issue_update`、
`issue_subscribe`、`issue_unsubscribe`、`issue_report_result`、`issue_my_work`。
这些工具**只**通过 `plugin_tools` / `plugin_tool_call` 出现，不进 core 的
`TOOL_NAMES`（core 仍是 29 个）。

阶段 1 没有 Wake。harness（dsh / opencode）在**会话首轮前** internal 调一次
`issue_my_work`（`internal: true`，不计入 `turnCalls`）：

- 有未读时，把这轮注入成一行系统事件：
  `[系统事件 issue_unread] N 条未读：<issue_id> <标题>；…；用 issue_my_work 查看并确认。`
  正文只给前 3 条摘要、计数和 issue id，不给 event_id。
- 注入后立刻用同一个 `event_deliveries` claim 账本 ack，保证同一 `event_id`
  在同一会话只注入一次；不另建去重表。
- 拉取失败静默降级：不注入、不报错，且不记录“已查”，下一轮再试。

## 备份与恢复

SQLite 用 WAL，**不要**直接 `cp issues.db`（可能漏 WAL）。用一致性快照：

```bash
acb-issue-service backup /backup/issues-$(date +%F-%H%M).db
```

`backup` 走 `VACUUM INTO`，目标已存在时拒绝覆盖。

恢复演练：

1. 停服务（或停 ACB 的 issue 接入，避免写入）。
2. 保留现场：`mv /var/lib/acb-issue/issues.db /var/lib/acb-issue/issues.db.old`
   （同时移走 `-wal` / `-shm`）。
3. 把备份文件放到 `ACB_ISSUE_DB` 指向的位置，属主/权限与服务一致。
4. 启动服务，确认 `GET /healthz` 200，再抽查一条 issue 和它的 events。
5. 确认无误后再删 `.old`。

恢复后 event_deliveries 也一起回到快照时刻；ACB 未确认的 pending 事件会重新
出现在 `issue_my_work` 里，这是预期的 at-least-once。

## 运维

- systemd 模板：[`deploy/systemd/acb-issue-service.service`](../../deploy/systemd/acb-issue-service.service)。
- 可选 NixOS module：[`deploy/nix/issue-service.nix`](../../deploy/nix/issue-service.nix)，
  默认不启用。
- 服务端每次请求都会跑一次 `run_due`；也可以由 cron/systemd timer 调
  `POST /v1/maintenance/run_due`（admin），或带 `now_ms` 做测试。
- 日志看 journal（systemd）或 stderr（前台）。常见错误：
  - `401 unauthorized`：token 错/过期/撤销，或没带 Bearer。
  - `403 forbidden`：scope 不够；验收还需要 `issue:verify`。
  - `409 stale_version`：先 `GET` 拿最新 `version` 再带 `expected_version`。
  - `409 invalid_transition`：按状态机合法路径走。
- 阶段 1 的“webhook”只有订阅与 outbox 记录，没有服务端 HTTP POST 投递；
  外部系统通过 `my_work` / `events` 拉取。dead-letter 与 Webhook 直投留给阶段 2。
