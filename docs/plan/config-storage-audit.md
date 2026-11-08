# ACB 配置存储审计（调研与过渡记录）

状态：2026-11-06。**已被 [docs/design/acb-policy-store.md](../design/acb-policy-store.md) 取代**；
本页保留配置清点、RBAC、资源维度与委托的调研过程，不作为实现依据。实现以
`acb-policy-store.md` 为准。

范围：只读审计。没有改代码、没有改生产配置。

---

## 0. 结论先行

- 不能把所有 `config.toml` 都塞进 SQLite。先按“开库前能不能确定”拆成四类：
  **部署声明**（端口/路径/unit/profile/command）、**策略数据**（grants/trust/关系/会话/插件策略）、
  **运行态**（消息、水位、票据、审计）、**密钥**（token/topic/API key）。
- 部署声明留 Nix/文件；密钥只留 env/agenix；运行态已经在 `state/acb.db`；策略数据进
  `state/policy.db`。
- 用户最新裁定：**会话与插件配置也进库**，因此它们从“留文件/等规模”改为“入库”；Nix 的
  `services.acb.sessions`、`modules.dsh.acbSessions` 和 provider 注册片全部降级为**空库 seed**。
- 迁移顺序不变：**先定判定语义（additive + 显式 deny），再迁存储**；否则只是把隐式重置
  搬进库。
- 会话进库有一个硬约束：dsh 的 preset 和驱动端口是 Nix 构建时产物，运行中新增会话没有
  现成 preset。最终方案是 `acb-policy-store.md` §8 的 preset reconciler：库是源，
  preset 目录是派生物；Nix preset 只做初始 seed。
- 插件 `command` 进库意味着放弃 Nix 对该项的复现；Nix store 路径还会随重建/GC 失效。
  这是明确的取舍，不是顺手迁一下。

---

## 1. 判定标准

每条配置按以下五问打分：

| 代号 | 问题 | 含义 |
|---|---|---|
| `N` | 是否需要 Nix 声明与 code review？ | 端口、unit、路径、provider command、supervisor 参数属于此类 |
| `S` | 是否随房间/主体/会话数量线性增长？ | grants、trust、approval_route、sessions、plugins |
| `Q` | 是否需要 UNIQUE / 外键 / 按维度查询？ | 主体、房间、关系、授权、插件策略 |
| `W` | 是否会被程序高频写入并依赖事务？ | 消息、水位、票据、审计、策略写入 |
| `R` | 回滚是否需要能精确到“删一行 / 删一片”？ | 策略行、override、assignment、provider |

迁移到库会失去什么：

- `git diff` / PR review：库是二进制，必须靠 `acbctl export` 生成可 review 的快照。
- 删片回滚：库没有“片”的概念，必须按行/版本回滚。
- Nix 复现：库是可变状态，`nixos-rebuild` 不能保证得到同一份策略。
- 人工开库排查：TOML 一眼能读，SQLite 需要 `sqlite3`/`acbctl` 和 schema 文档。
- 迁移本身：schema 版本、WAL、并发写、备份、租户/环境隔离都要补。

反过来，入库得到什么：

- 唯一约束（同一 principal×scope×capability 不会重复授权）。
- 行级撤销（删一个 assignment/override，而不是改一整片 TOML）。
- 动态会话/插件调整不再要求重建 Nix。
- 判定缓存可以按 `resource-version` 失效，而不是每条消息重读文件。

---

## 2. 实际生效来源清单

### 2.1 文件与派生

- `~/.local/share/acb/config/config.toml`：手写基准。包含连接、personas、memories、
  harnesses、6 个 sessions、subscriptions、grants、file_relay。
- `~/.local/share/acb/config/config.active.toml`：`acb-config-merge` 在 bridge 启动前生成；
  内容 = `config.toml` + Nix 生成的 `[notifications]`（若 base 没有该表）。
- `~/.local/share/acb/config/config.d/`：
  - `70-grants-issue.toml`：9 个群的 system_admin 规则（当前临时四能力）。
  - `80-issue-tools.toml`：`issue.tools` provider 注册，command 指向 Nix store。
  - `90-console.toml`：当前不存在；控制台保存时才创建。
  - `70-grants-issue.toml.pre-rooms`：不会被加载的备份。
- `config.toml.bak*`、`acb.db.bak*`：手工备份，不是数据源。

### 2.2 Nix 生成/声明

- `nix-config/vars/networking-downstream.nix` 的 `services.acb`：hosts、端口、sessions 列表。
- `nix-config/modules/nixos/base/custom/ai/acb/default.nix`：四个 systemd user unit、
  `acb-stack.target`、`acb-config-merge`、`acb-drivers`、`acb-dsh-exec`、profile seed。
- `nix-config/hosts/idols-ai/default.nix`：把 `services.acb` 映射到 `modules.acb` 与
  `modules.dsh.acbSessions`。
- `nix-config/hosts/idols-ai/ai/acb-issue.nix`：issue 服务、`ACB_ISSUE_URL`、DB 路径。
- `nix-config/hosts/idols-ai/ai/acb-notifications.nix`：ntfy topic 的 agenix 路径。
- `nix-config/modules/nixos/base/custom/ai/dsh/default.nix`：dsh profile/preset、web-acb bundle。

### 2.3 环境与密钥

- `/run/agenix/acb.env`：`ACB_CONSOLE_TOKEN`、`ACB_SNOWLUMA_TOKEN`、`ACB_MODEL_KEY`、
  `NEW_API_..._API_KEY`、`ACB_ISSUE_TOKEN`、`ACB_OPENCODE_PASSWORD`。
- `/run/agenix/ntfy-acb-topic.env`：`NTFY_ACB_TOPIC`。
- `~/.local/share/acb/config/dsh.env`：模型 API key（仍落在配置目录）。
- `~/.local/share/acb/opencode/env`：注释说明密钥由 agenix 提供；`opencode.jsonc` 里仍有
  `token` 选项（opencode 两个会话共用的接入令牌）。
- unit `Environment`：端口、`ACB_ISSUE_URL`、`ACB_LUA_DIR`、`ACB_OPENCODE_*` 等。

### 2.4 写死在 unit/脚本里的值

- `acb-dsh-exec`：`--profile nix-web-acb --host 127.0.0.1 --port 3080`。
- `acb-drivers` / `driver-session.sh`：`ACB_ENDPOINT=127.0.0.1:7700`、`DSH_BASE_URL`、
  `ACB_MCP_PORT`、`ACB_PRESET`、`ACB_CWD`；**token 由脚本从 `config.toml` 直接 awk 提取**。
- `acb-opencode-serve`：`ACB_OPENCODE_DIR/PORT/HOST/BIN`。
- `acb-bridge`：`--config config.active.toml`，`ACB_ENDPOINT_LISTEN`、`ACB_CONSOLE_LISTEN`、
  `ACB_RELAY_LISTEN`、`ACB_LUA_DIR`。

### 2.5 运行时存储

- `state/acb.db`：`meta`、`records`、`watermarks`、`acks`、`deliveries`、`notes`、
  `session_maps`、`feedback`。`SCHEMA_VERSION = 11`；WAL；**单进程使用**（storage 代码明确
  多进程不在支持范围内）。
- `/var/lib/acb-issue/issues.db`：issue 服务独立 SQLite。
- `~/.local/state/acb/*.log`：bridge/drivers/dsh/opencode 日志，append 写入。

---

## 3. 归属表（按最新口径）

评分：`N`=Nix/review，`S`=随规模增长，`Q`=UNIQUE/查询，`W`=程序写入，`R`=删一行/删一片。
`—` 表示该维度不是主要决定因素。

| 内容 | 当前位置 | 谁改 | 频率 | 程序写入 | 回滚 | N/S/Q/W/R | 结论 | 理由 |
|---|---|---|---|---|---|---|---|---|
| 端口/监听地址（3080/7700/3100/3131/4097/53001/3101-3104） | `services.acb`、unit Environment | Nix | 低 | 否 | git revert | N | 留文件（Nix） | 部署声明，必须可 review |
| systemd unit/依赖/重启/超时 | `modules/.../acb/default.nix` | Nix | 低 | 否 | git revert | N | 留文件（Nix） | supervisor 参数属于部署 |
| issue 服务 listen/DB 路径/静态用户 | `hosts/idols-ai/ai/acb-issue.nix` | Nix | 低 | 否 | git revert | N | 留文件（Nix） | 部署声明 |
| ntfy URL/min interval | `acb-notifications.nix` + fragment | Nix | 低 | 否 | git revert | N | 留文件（Nix） | 部署声明；topic 走 env |
| `file_relay` listen/public_base/dir | `config.toml` `[file_relay]` | 手工 | 低 | 否 | git revert | N | 移到 Nix/env | 部署参数，不该由业务配置维护 |
| 连接拓扑（id/platform/kind/url/account） | `config.toml` `[[connections]]` | 手工 | 低 | 否 | git revert | N | 留文件 | 部署声明；token 走 env |
| personas/memories/harnesses | `config.toml` 数组 | 手工 | 低 | 否 | git revert | — | 留文件 | 内容/路径定义，不是动态会话字段 |
| **会话身份（id/harness/persona/model/启用）** | `config.toml` `[[sessions]]`；Nix sessions seed | 手工/控制台 | 中 | 是（库） | 删行/版本 | S/Q/W/R | **入库** `sessions` | 需要运行时动态调整；Nix 只做空库 seed |
| **会话 token** | `config.toml` 的 `token = "..."` | 手工 | 低 | 否 | env reload | — | **入库只存 `token_ref`；真值 env** | 密钥不落库；轮换 = 换 env + reload |
| **订阅/wake/clock（10 块）** | `config.toml` | 手工/控制台 | 中 | 是（库） | 删行 | S/Q/W/R | **入库** `session_rooms` | 会话动态调整的连带字段 |
| **会话绑定群/端口/preset** | `config.toml` + Nix sessions | 手工/控制台 | 中 | 是（库） | 删行 | S/Q/R | **入库** `sessions`；Nix seed | 端口进库；preset 由 reconciler 派生物化 |
| **会话工具集** | 当前由 tool surface 计算 | 控制台 | 中 | 是（库） | 删行 | S/Q/R | **入库** `session_tools`（新策略） | 当前没有持久化字段，属于新增能力 |
| **插件注册（command/args/timeout）** | `config.d/80-issue-tools.toml` | 手工/控制台 | 中 | 是（库） | 删行 | S/Q/W/R | **入库** `plugin_registry`；Nix 只做 seed | 放弃 Nix 对 command 的复现；store 路径会失效 |
| **插件策略（启用/作用域/能力/限流/生命周期）** | 当前无；只有 timeout | 控制台 | 中 | 是（库） | 删行 | S/Q/W/R | **入库** `plugin_policy` | 需要运行时动态调整 |
| **插件密钥** | `dsh.env`/`opencode.env`/agenix | 手工 | 低 | 否 | env reload | — | **入库只存 `token_ref`；真值 env** | 密钥不落库 |
| `grants.system_admin` 身份映射 | `config.toml` | 手工/控制台 | 中 | 是（库） | 删行 | S/Q/W/R | **入库** `principals/assignments` | 主体关系，需唯一约束 |
| base `[[grants.rules]]` | `config.toml` | 手工/控制台 | 中 | 是（库） | 删行 | S/Q/W/R | **入库** `roles/assignments` | 策略数据 |
| `config.d/70` 9 条群规则 | `config.d/70-grants-issue.toml` | 手工/控制台 | 中 | 是（库） | 删行/版本 | S/Q/W/R | **入库** `assignments` | 随房间增长；当前 9×4 能力值 |
| `[grants.rooms]` trust/members/approval_route | `config.toml` | 手工/控制台 | 中 | 是（库） | 删行 | S/Q/W/R | **入库** `rooms/relations` | 房间策略，需约束/查询 |
| `default_on_turn_error` | `config.toml` | 手工/控制台 | 低 | 是（库） | 删行 | S | **入库** `settings`（策略） | 会话/房间策略的一部分 |
| delivery_limit/turn_timeout_secs | `config.toml` | 手工/控制台 | 低 | 是（库） | 删行 | — | 留文件（运行调参） | 不是权限；改动可走配置/reload |
| `config.active.toml` | 派生文件 | `acb-config-merge` | 每次启动 | 派生 | 删后重建 | — | 留作派生，不当源 | 只追加 notifications |
| `90-console.toml` | 当前不存在；控制台保存时创建 | 控制台 | 中 | 是（库） | 删行/版本 | S/Q/W/R | **控制台写库；文件只做 export/seed** | `[fragment]` 覆盖方案退场 |
| `config.toml.bak*` / `acb.db.bak*` | 配置/状态目录 | 手工 | 低 | 否 | 恢复 | — | 留作备份，移入 backups | 不是数据源；DB 文件拷贝不可靠 |
| records/watermarks/acks/deliveries | `state/acb.db` | 程序 | 高 | 是 | DB 事务/备份 | S/Q/W | 已进库 | 运行态 |
| notes/feedback/session_maps/meta | `state/acb.db` | 程序 | 中 | 是 | DB | S/Q/W | 已进库 | 运行态 |
| issue 库 `/var/lib/acb-issue/issues.db` | issue 服务 | 程序 | 高 | 是 | DB 备份 | S/Q/W | 已进库 | 独立运行态 |
| 审计/日志（access、bridge、drivers、dsh） | `~/.local/state/acb/*.log` | 程序 append | 高 | 是 | rotate/截断 | S/W | 留文件/journald；判定审计进 `audit` 表 | 高吞吐 append，不等于配置库 |
| access tickets | 进程内存 | 程序 | 高 | 是 | 重启即失效 | — | 留内存 | 单轮/短 TTL |
| dsh 3080/profile、driver MCP 端口、token 提取 | `acb-dsh-exec`、`acb-drivers` 脚本 | Nix 生成 | 低 | 否 | git revert | N | 留 Nix；token 提取改 `token_ref` | 脚本从 `config.toml` 读 token 与 active/DB 可能漂移 |

---

## 4. 推荐分组

### 4.1 现在就做（与入库并行）

1. **会话与插件进 `policy.db`**：先建 schema、导入器、双读校验，再切写路径。
2. **会话 token 改成 `token_ref`**：库内只出现 `env:NAME`；真值由 agenix → systemd → 进程。
3. **Nix 降级为 seed**：`services.acb.sessions`、`modules.dsh.acbSessions`、provider 注册片
   在空库时写入一次，非空跳过并打日志；Nix 不再覆盖运行中的定义。
4. **插件 command/args 进库**：显式接受 Nix 复现损失；启动/`doctor` 检查 command 是否
   可执行，store 路径 GC 后要报 drift。
5. **控制台改为写库**：`90-console.toml` 只保留 export/seed；`[fragment]` 方案不再实现。
6. **审计行升级**：判定审计写 `audit` 表，日志行保留人读版本。

### 4.2 先不动等规模/等二期

1. **grants/rooms/approval_route/RBAC/委托**：语义（additive + 显式 deny）先定；存储迁移
   在二期。当前 9 条规则还能用文件，但迁移前不要再扩。
2. **personas/memories/harnesses**：内容/路径定义，继续文件/Nix；等它们也需要动态编辑时
   再评估。
3. **console/ntfy 配置**：暂留 Nix/文件；token/topic 走 env。若之后也要动态改，再进
   `settings` 并同样只存 `token_ref`。

### 4.3 永远别进库

- 端口、监听地址、unit 模板、supervisor 参数、dsh profile/preset 构建产物、provider
  command 的**构建来源**（可以进库，但 Nix 复现必须由 export/seed 补回）。
- 密钥真值：session token、plugin secret、console token、ntfy topic、connection token。
- `config.active.toml` 作为“可编辑源”；它只能是派生文件。
- access tickets 的持久化（当前语义是单轮/短 TTL；要持久化是另一个设计）。
- 审计日志的高吞吐原始流（可进 `audit` 表的是判定事件，不是全部日志）。

---

## 5. 迁移代价

### 5.1 Schema 版本与迁移

- `policy.db` 单独维护 `schema_version`；不要复用 `acb.db` 的 `SCHEMA_VERSION = 11`。
- 迁移必须“先补列/表，再 bump 版本”；旧程序遇到新版本直接拒绝，不猜测结构。
- 每次迁移前 `acbctl export` + `sqlite3 .backup`；迁移失败用备份恢复，不做半迁移。

### 5.2 WAL 与多进程并发

- 当前 `SqliteStore` 是**单进程**契约；`policy.db` 会有 bridge、console、`acbctl` 多个写者，
  必须重新定义并发模型：
  - WAL + `busy_timeout=5000`；
  - 写事务用 `BEGIN IMMEDIATE`，短事务，不在事务里做网络/进程启动；
  - 每次写 bump `policy_version`；行级 `version` 做乐观锁，冲突返回可读错误；
  - bridge 用读连接/快照 + 版本轮询（`meta.policy_version` 或 `PRAGMA data_version`）失效缓存。
- 不建议让 `acbctl` 和控制台各自长时间持有写事务；插件启动/停止放在事务提交之后，
  失败按上一版本回滚策略。

### 5.3 备份

- `acb.db` 的 `.bak` 文件拷贝在 WAL 下可能不一致；改用 `sqlite3 .backup` 或
  `VACUUM INTO`。
- `policy.db` 每次迁移/批量导入前做一致性备份；`acbctl export` 生成可读快照进 git/审查。

### 5.4 Nix 可复现性

- 会话/插件进库后，`nixos-rebuild` 不再决定运行中的会话/插件；必须靠 `acbctl export`
  把库状态导出成快照，或靠 seed 文件记录初始状态。
- 建议在仓库/运维目录保留 `policy-export/`（脱敏）用于 review；不把密钥导出。

### 5.5 审计与回滚

- 回滚从“删片/恢复 .bak”变成“删/恢复一行 + 版本回滚”；`assignments`/`overrides` 需要
  `id`、`version`、`revoked_at`，否则无法精确撤销。
- 每次写入 `audit`：actor、目标、能力集、scope、parent_grant、reason、request_id。
- `acbctl grants tree` 要能显示祖先/后代、过期、撤销、`needs_review`。

### 5.6 人工排查

- 库不是“打开就能读”；必须提供 `acbctl explain/tree/matrix` 和 schema 文档。
- `sqlite3` 只能作为最后手段；直接改库会绕过校验和审计，必须禁止。

---

## 6. 顺序约束

1. **先定语义**：`effective = (floor ∪ grants) − deny`；`evaluate_rules` 改成“匹配但缺
   能力不否决”；70 片的临时四能力删回 `["plugin"]`；9 个群验证。
2. **再定能力/资源名**：RoomRead → room_read/room_history；RoomWrite → room_write；
   Plugin → tool_call；AccessRequest 保留为 meta；AccountRead/AccountSecret/Destructive/
   Critical 的处置先写清。
3. **再建 `policy.db` 与导入器**：双读（文件 vs 库）跑 `grants matrix/diff`，对齐为零差异。
4. **再切会话/插件**：Nix seed → 库；`acbctl reload` 优雅重载；失败回滚版本。
5. **最后退文件**：`config.d` 变只读历史；`90-console` 只做 export/seed。

不要在 1 之前把 grants 搬进库；否则只是把“匹配即独占”的隐式重置换了个存储位置。

---

## 7. RBAC 评估与草案

### 7.1 结论

RBAC 可行，但只作为 **grants 的存储/表达层**，不另起判定器。它能显著减少
“9 房间 × 4 能力”的重复，但不能解决 trust/关系/资源维度；这些要由 `rooms`、
`relations` 和 `principals` 补上。

### 7.2 表结构草案

```sql
CREATE TABLE roles (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE role_caps (
  role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  capability TEXT NOT NULL,             -- 固定枚举，写入时校验
  can_delegate INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (role_id, capability)
);
CREATE TABLE subject_roles (
  id INTEGER PRIMARY KEY,
  subject TEXT NOT NULL,                -- user:<id> / system / bot_admin
  role_id TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  place TEXT NOT NULL,                  -- private / group / room:<id>
  created_at INTEGER NOT NULL,
  expires_at INTEGER,
  revoked_at INTEGER,
  UNIQUE(subject, role_id, place)
);
CREATE TABLE rooms (
  id TEXT PRIMARY KEY,
  trust TEXT NOT NULL DEFAULT 'untrusted',
  approval_route TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE room_overrides (
  id INTEGER PRIMARY KEY,
  room_id TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
  subject_or_role TEXT NOT NULL,        -- subject:<...> / role:<id>
  capability TEXT NOT NULL,
  effect TEXT NOT NULL CHECK (effect = 'deny'),
  reason TEXT,
  expires_at INTEGER,
  revoked_at INTEGER
);
```

补充：`rooms.members` 当前是审批白名单，不是一般成员关系；RBAC 草案需要
`room_members(room_id, subject)` 或并入 `relations`。`approval_route` 建议规范化成
`room_approval_routes(room_id, seq, target_room)`，但也允许 JSON + 写入校验。

### 7.3 判定公式

必须与既有模型一致：

```text
floor = subscription_reachability ∩ trust_cap(room) + admin_private/console 基线
grants = role_caps(subject, place) ∩ trust_cap(room)  // 角色只做加法
deny = room_overrides(subject_or_role, capability)    // 房间例外只做减法
effective = (floor ∪ grants) − deny
```

`trust_cap` 只限制高风险能力（`destructive`/`critical`/`account_secret` 等），不是读写
地板；否则 9 个 untrusted 群会重新失去读写。角色不得绕过 trust_cap。

### 7.4 冗余对照

| 现状 | 行数/能力值 | RBAC 表达 | 结果 |
|---|---|---|---|
| 70 片 9 房间 × 4 能力 | 9 条规则，36 个能力值 | 1 个 `group-plugin` 角色 + 1 个 `plugin` role_cap + 1 条 group 级 assignment；若必须精确 9 房间，则 9 条 `place=room:<id>` assignment | group 级 3 行；精确范围 11 行；0 override |
| base `system_admin` 私信规则 | 1 条规则，3 个能力值 | 1 个 `admin-private` 角色 + 3 个 role_cap + 1 条 private assignment | 5 行；只有 1 个管理员时比原规则多，2+ 管理员时开始省重复 |
| 80 片 provider 注册 | 1 条 `[[tool_plugins]]` | RBAC 不表达 provider | 仍是 1 条部署行（Nix 生成/seed）；RBAC 0 角色 |
| `rule:6` 事故 | `plugin` 规则压掉 `room_read` | 角色只加 `plugin`；floor 给读写；0 override | 事故消失；显式 deny 才会减 |

结论：RBAC 省的是**能力值重复**，不是所有行数。9 个房间的精确 scope 仍需 9 条
assignment；如果接受“所有群”的 group 级角色，才是 1 条 assignment。

### 7.5 代价与风险

- **角色爆炸**：一房间一角色就是 ABAC 失败信号。启发式：一个角色 >5 条 room override，
  或 override 超过 role-room 对的 20%，就退回 ABAC/显式规则，不再加角色。
- **审计**：必须记角色 id、override id、floor 来源；否则“为什么允许”又变成猜。
- **trust**：角色只能加，不能绕过 trust_cap；高危能力仍走审批。
- **精确回收**：`subject_roles` 和 `room_overrides` 必须有 id；删一行，不改整角色。

### 7.6 不采纳

- `ops` 通用 CRUD 抽象：能力保持固定枚举（`GrantCapability`），加载/写入期硬校验；
  `ops` 会引入未知动作和无法审计的自由度。
- 角色继承层级：继承会产生隐式授权，审计和回收都变复杂；用多条扁平 assignment 表达
  组合，必要时上 ABAC。
- 通用权限市场/无限转授：见 §9。

### 7.7 迁移映射

| 现规则 | RBAC 映射 |
|---|---|
| `[grants.system_admin]` | `subject_roles(user:<id>, admin-private, private)` |
| base 私信三能力 | `roles(admin-private)` + 3 `role_caps` + 上面 assignment |
| 70 片 9 条 `plugin` | `roles(group-plugin)` + `role_caps(plugin)` + 9 条 room 级 assignment（或 1 条 group 级） |
| 70 片临时四能力 | 不回填四项；floor 给读写/票据，角色只给 plugin |
| `[grants.rooms]` trust/members/approval_route | `rooms` + `room_members`/`relations` + `room_approval_routes` |
| 80 provider | 不进 RBAC；进 `plugin_registry` |
| `default_on_turn_error` | 不进 RBAC；进 `settings` |

---

## 8. 资源维度与账号

### 8.1 代码里的能力全集与鉴权入口

`GrantCapability`（`crates/kernel/src/grants.rs`）实际有 10 个：

| 能力 | 当前用途 | 鉴权入口 |
|---|---|---|
| `room_read` | `get_history`/`fetch_records`/`wait_message`/`notes:*`/`get_platform_events`；房间平台读 | `ensure_readable` / `check_access(AccessAction::Read)` |
| `room_write` | `send_message`/`send_message_to`；房间平台写 | `ensure_can_send` / `check_access(Send)` |
| `connection_read` | 连接级平台读 | `ensure_connection_readable` / `check_access(Connection)` |
| `platform` | 普通平台方法（非账号、非高危） | `platform_capability` → `evaluate_platform` |
| `destructive` | 群管理/破坏性平台方法（kick/mute/mute_all/member.admin/group.name 等） | `Risk::Destructive` → `platform_capability` → `evaluate_platform` |
| `critical` | 未验证/最高危方法 | `Risk::Critical` / `verification` → 票据 |
| `account_read` | `account.*`/`file.*`/`stream.*` 账号侧读 | `platform_capability`（`access_kind == Account`） |
| `account_secret` | 取账号凭证 | 配置禁止常驻授予；`ticket_capabilities` 只允许票据 |
| `access_request` | 申请票据 | `evaluate_access_request` |
| `plugin` | `plugin_tool_call` | `evaluate_tool` |

`AccessTargetKind` 实际有 8 个：`room/message/member/file/account/connection/plugin_tool/`
`platform_op`。资源维度提案只保留 `room/account/tool`，其余需要 `sub` 或 operation 字段
表达；不能假装已统一。

没有独立鉴权入口的操作：

- `member.kick`/`group.mute`/`group.mute_all`/`member.admin`/`group.approve`：走
  `platform_call` 的 `destructive`/`write` 风险，不检查人类主体是否群主/管理员；
  `SenderRole::Owner/Admin` 只记录在 `records.sender_role`，不参与授权。
- 私信发送/读取：当前是 `room_write`/`room_read`，target 是 `qq:private/<id>`；
  `account_message` 没有独立入口。
- `account_admin` 没有独立能力；群管理只是 `destructive` 风险。
- `plugin_tool_call` 的 `plugin` 能力永远按**当前房间**判定；声明 connection scope 时
  还额外查 `connection_read`，但 `plugin` 仍不是按 connection/tool 资源判定。
- `capabilities` 是只读探测：可对任意 room 返回 readable/sendable/ops，不做读取鉴权；
  不泄露内容，但泄露能力元数据。
- `ack_messages`/迟到发送：靠轮次票/TTL，不是 grants。

### 8.2 trust、approval_route、approver 的现状

- `rooms.trust`：目前只用于 `approval_allowed`（群内审批身份）；不是读写能力的 trust cap。
- `members`：审批白名单；空 = 只按 trust。
- `approval_route`：房间级的审批目标房间列表；不是“谁能批准”的人名单。
- approver：真实批准人是发送者 id，记在 `Ticket.approver`；`approval_allowed` 用
  trust + members 判定。没有 `owner/approver` 关系表。
- `SenderRole::Owner/Admin`：记录层身份，存在但未接入 grants。
- `system_admin`/`bot_admin`：由 `[grants.system_admin]`/`bot_admin.accounts` 映射；
  `admin_qqs` 只标 sender_role，不是授权主体。
- 结论：owner/member/approver 关系**部分建模、未接入授权**；新表要留位置，但先不要
  假装它们已经在控制判定。

### 8.3 统一资源维度

```text
resource = kind + id
kind ∈ { room, account, tool }
```

建议能力命名：

| 新名 | 含义 | 与现有映射 |
|---|---|---|
| `room_read` | 当前/实时房间读（notes/events/wait/live） | `RoomRead` 的一部分 |
| `room_write` | 房间发送 | `RoomWrite` |
| `room_history` | 历史/记录（get_history/fetch_records） | `RoomRead` 的另一部分；建议 `room_read ∧ room_history` |
| `account_read` | 账号资料/文件/stream 读 | `AccountRead` |
| `account_message` | 向账号/私聊发消息 | 当前无入口；私信仍是 `room_write` |
| `account_admin` | 群/账号管理动作（kick/mute/approve） | 当前是 `destructive` + `platform` |
| `tool_call` | 插件工具调用 | `plugin` |

保留为 meta/风险层，不强行并入 7 个资源能力：

- `access_request`：票据申请是独立 meta 能力；并入 `tool_call` 会丢失房间级票据控制。
- `account_secret`：只能票据，不可常驻授予。
- `destructive`/`critical`：风险层；`account_admin` 是能力，二者同时检查。
- `connection_read`/`platform`：legacy/internal；先保留，后续再决定是否折叠到
  `account_read`/`room_read`/`room_write`。

主体与资源同名时（账号既是主体又是资源）：

- 审计必须分开写 `subject=principal:qq:<id>` 与 `resource=account:qq:<id>`；
  不能只写一个 target。
- `principals(id, platform, external_id, kind)` 统一主体；资源 `account` 的 id 用
  `platform:external_id`，避免把 bot 账号和人类账号混为一谈。
- 当前 `AccessTargetKind::Account` 指的是**机器人账号**（`account_of_room`），不是发送者；
  新模型必须显式区分 `bot account` 与 `user account`。

### 8.4 四块表与两条机制

```sql
CREATE TABLE principals (
  id INTEGER PRIMARY KEY,
  platform TEXT NOT NULL,
  external_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('user','bot','system')),
  display_name TEXT,
  UNIQUE(platform, external_id)
);

CREATE TABLE relations (
  id INTEGER PRIMARY KEY,
  subject_id INTEGER NOT NULL REFERENCES principals(id),
  relation TEXT NOT NULL CHECK (relation IN ('owner','member','approver')),
  resource_kind TEXT NOT NULL CHECK (resource_kind IN ('room','account','tool')),
  resource_id TEXT NOT NULL,
  source TEXT NOT NULL CHECK (source IN ('platform','explicit','seed')),
  since INTEGER NOT NULL,
  until INTEGER,
  evidence TEXT
);

CREATE TABLE expires_policy (
  id INTEGER PRIMARY KEY,
  subject_id INTEGER REFERENCES principals(id),
  role_id TEXT,
  resource_kind TEXT,
  resource_id TEXT,
  expires_at INTEGER,
  revoked_at INTEGER,
  revoked_by INTEGER REFERENCES principals(id)
);

CREATE TABLE decision_audit (
  id INTEGER PRIMARY KEY,
  at INTEGER NOT NULL,
  request_id TEXT,
  actor_id INTEGER REFERENCES principals(id),
  subject_id INTEGER REFERENCES principals(id),
  role_id TEXT,
  override_id INTEGER,
  relation_id INTEGER REFERENCES relations(id),
  resource_kind TEXT NOT NULL,
  resource_id TEXT NOT NULL,
  capability TEXT NOT NULL,
  decision TEXT NOT NULL,
  floor_json TEXT NOT NULL,
  policy_version INTEGER NOT NULL,
  reason TEXT NOT NULL
);
```

为什么关系不用角色表达：

- owner/member/approver 是**事实**（来自平台或显式指定），角色是**授权**；事实会变，
  角色不应跟着变。
- 关系不应该被继承/转授；角色可以委托。
- 关系参与审批/floor，不直接授予能力，避免“成员自动获得读写”。

两条机制：

- **判定缓存**：按 `resource-version`；每次写 bump `policy_version` 和
  `resource_versions(kind,id)`；缓存键包含 principal/resource/capability/floor context；
  bridge 不每条消息查库。
- **break-glass**：本地文件/env 的一次性紧急授权（不依赖库），有 TTL、只给固定
  `system_admin` 能力、写独立审计文件；用完即删；不得成为常规运维路径。

### 8.5 不做清单

- Zanzibar 全关系图：当前只有 owner/member/approver 三种关系，做全图是过度设计。
- 角色继承层级：隐式授权、审计困难、回收不精确。
- 通用 CRUD `ops`：能力固定枚举，`ops` 会产生未知动作和不可审计的自由度。

---

## 9. 授权委托（为他人加权限）

### 9.1 能力与不越权

新增 `grant_manage`：允许写授权表。真正能授出什么由**授予者在同一 scope 的 effective**
约束：

```text
candidate_caps ⊆ effective(grantor, scope)
candidate_scope ⊆ grantor_scope
scope hierarchy: room:<id> ⊆ group ⊆ global; private 是独立分支，room:<private> 最具体
```

`grant_manage` 本身也受此约束；没有 `grant_manage` 不能写授权表。

### 9.2 能力快照

授权行必须存：

- `capability_snapshot`：授予时的能力集合；
- `role_snapshot`：授予时的角色 id + role_caps 快照。

不能只存角色 id。否则角色日后扩容会把历史授权**追认**成超额授权。角色变更时：

- 新授权使用新快照；
- 旧授权保留旧快照；
- 标记 `needs_review=1`，`acbctl grants tree` 提示复审；
- 不静默扩展，不自动回收（复审后显式重新授权/撤销）。

### 9.3 授权行与撤销

```text
assignment:
  id, subject, role_id, scope_kind, scope_id,
  capability_snapshot, role_snapshot,
  can_delegate, granted_by, granted_at, expires_at,
  parent_grant, depth, reason, status, needs_review,
  revoked_at, revoked_by, revocation_reason, version
```

撤销按 `parent_grant` 级联：撤销父授权时，递归标记所有后代 `revoked_at`；拒绝环；
`depth <= 2`。到期按 `expires_at` 过滤，不物理删除以保留审计。

### 9.4 继续委托

- 只有带 `can_delegate=1` 的 `grant_manage` 才能把 `grant_manage` 再授出；
- 普通 `grant_manage` 可以授普通能力，但不能造新的委托者；
- 链深默认 2；超过时**拒绝**，不截断、不自动降级；
- 禁止自授、环、跨 scope 放大。

### 9.5 高危能力

`account_admin`、对外 `tool_call`、`account_secret`、`destructive`/`critical` 不经普通委托；
走 `approval_route`：

- `acbctl grant` 对高危能力创建 pending grant request；
- 复用现有票据审批：`approval_route` + `approval_allowed`（trust/members）+ approver；
- 批准后写 assignment，`granted_by=approver`，`request_id` 进审计；
- 无路由/路由不可达 -> pending，不落授权；
- 不在委托里另造一套审批。

### 9.6 接口

```text
acbctl grant <principal> <role|capability> --scope <place>
  [--can-delegate] [--expires <ts>] [--reason <text>]
acbctl revoke <assignment-id>
acbctl grants tree [--principal <p>] [--scope <s>]
```

每次写入 `audit`：actor、目标、能力集、scope、parent_grant、reason、request_id、结果。
`grant` 高危能力返回 pending + 路由；批准/拒绝走现有审批命令。

### 9.7 边界

- 不做通用权限市场：授权只能由有 `grant_manage` 的主体在自身 effective 范围内进行，
  不发布、不竞价、不匿名。
- 不做无限转授：depth 2，超限拒绝。
- 不做跨平台主体隐式映射：`principals(platform, external_id)` 是复合身份；QQ 号不等于
  其他平台号，不做自动合并。

---

## 10. 代码证据索引

- `crates/kernel/src/grants.rs`：`GrantCapability`、`AccessTargetKind`、
  `evaluate_access`/`evaluate_access_request`/`evaluate_tool`/`evaluate_rules`、
  `base_access`、`scope_matches`、`subject_for`、`GrantsConfig::validate`。
- `crates/kernel/src/engine.rs`：`check_access`、`ensure_readable`/`ensure_can_send`/
  `ensure_connection_readable`、`plugin_tool_call`、`platform_call`、
  `platform_capability`、`ticket_capabilities`、`approval_allowed`、`audit_grant`。
- `crates/kernel/src/endpoint.rs`：`handshake`（harness 名 + token 精确匹配）与
  `Conn`（同 token 的会话组）。
- `crates/kernel/src/config.rs`：`Config::load`、`config.d` 合并、`90-console` 覆盖、
  `RawConfig`/`GrantsConfig` 校验、session token。
- `crates/kernel/src/storage/sqlite.rs`：`SCHEMA_VERSION = 11`、WAL、单进程契约。
- `crates/plugins/process-tools/src/loader.rs` / `provider.rs`：provider 启动、常驻
  supervisor、重启/背压。
- `crates/bridge/src/console.rs` / `config_patch.rs`：控制台保存、session patch、
  `reload_impact`。
- Nix：`nix-config/modules/nixos/base/custom/ai/acb/default.nix`、
  `vars/networking-downstream.nix`、`hosts/idols-ai/ai/acb-issue.nix`、
  `hosts/idols-ai/ai/acb-notifications.nix`。
- 运行证据：`bridge.log` 的配置来源/effective rules、`dsh.log` 的启动、`drivers.log`
  的 turn/tool 记录；`state/acb.db` 表结构与 `meta.schema_version=11`。

