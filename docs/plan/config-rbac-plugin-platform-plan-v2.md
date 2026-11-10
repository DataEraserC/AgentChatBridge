# 配置源、RBAC 与插件平台计划 v2（2026-11-10）

> 状态：planning，已对齐主要决策，待你审阅后开工。
> 本文件取代 `config-rbac-plugin-platform-plan.md`（v1 只保留历史讨论）。
> 关联台账：`docs/plan/todo-ledger.md` 的 F1b / F4 / F5 / F6 / J1-J4。
> 关联设计：`docs/design/acb-policy-store.md` §3.3-3.7、§6.2-6.4；`docs/plan/plugin-api.md`。

## 0. 已锁定决策

1. 运行时 `config.toml` 只留启动必需；仓库旧 `config.toml` 移入 `config.deprecated/` 并 gitignore。
2. 会话增删必须自动起停驱动；dsh 会话使用显式 `driver_port`，不再按数组下标推导。
3. 权限采用 RBAC + ABAC：角色可定义、可指派；现有 `policy_rules` 继续作为 ABAC 条件。
4. 票据只能由「拥有对应能力且范围覆盖目标」的人批准；高危能力只允许 system_admin / break-glass。
5. 插件能力采用混合模型：core 保留 `plugin` 粗门禁；插件可选声明 `plugin:<id>:<name>` 细能力；`requires_ticket` 归 core。
6. 权限判定可插件化，但执行收口、票据、审计、fail-closed 留在 core；插件之间不互相调用。
7. 插件存储 `builtin` / `external` 可配；builtin 由 ACB 提供 API，external 由插件自己加载和维护数据库。
8. 插件配置、CRUD、自定义动作由 manifest 声明，控制台按 schema 渲染；插件自带网页允许，但加 sandbox 隔离。
9. 库内允许存密钥，但每个字段有 `encrypted` 开关；密钥本身只从 `ACB_KEY_FILE` / `ACB_KEY` 读。
10. Agent 可以调用配置类工具，但必须走能力校验、票据与审计。

## 1. 现状基线（2026-11-10 实测）

| 项 | 事实 | 证据 |
|---|---|---|
| 活配置 | `~/.local/share/acb/config/config.toml` 107 行，只有 secrets/endpoint/console/tickets/file_relay/notifications + 3 个全局键 | `acb-bridge --check` 报文件贡献 connections 0 / sessions 0 / rules 0 |
| 业务对象 | 连接 1 / 会话 6 / 人格 2 / 记忆 2 / harness 2，全在 `config_objects`（schema 15） | `sqlite3 state/acb.db` |
| 会话订阅 | 在 session 行的 TOML 原文里，随会话一起入库 | `config_objects.payload` |
| 权限 | `policy_rules` 11 / `policy_rooms` 7 / `policy_accounts` 0 / `policy_admins` 1 | 同上 |
| 主体/能力 | 4 个写死主体；10 个写死能力；角色到能力映射在代码 | `grants.rs:167,329`、`policy.rs` |
| 票据审批 | 只看「私信/控制台管理员」或「群 trusted + 成员名单」，不看审批人能力 | `engine.rs:3449-3475` |
| 策略插件 | `Policy` trait 在，只有 `CorePolicy`；无 `policy_binding`、无 provider 解析 | 全仓 0 命中 |
| 会话级外部策略 | `Session.access.provider` 可换外部 `ToolAccessProvider`，但 `plugin_tool_call` 在外部 provider 下直接放行 | `engine.rs:5836-5839` |
| 插件注册 | `[[tool_plugins]]` 在 `config.d/80-issue-tools.toml`，command 是 nix store 路径 | 活配置 |
| 插件存储/配置 | 无 `plugin_registry` / `plugin_config` / `plugin_storage` / `plugin_secret_refs` | 全仓 0 命中 |
| 控制台插件页 | 只有 reload/unload，没有配置表单 | `console.html:269-275` |
| 驱动 | 启动时读一次 `acbctl sessions`；端口 = 3101 + 下标；任一驱动退出整组退出 | `deploy/presets/drivers/drivers_acb.sh` |
| 当前 dsh 端口 | `dsh-qas 3101 / dsh-dev 3102 / dsh-snowluma-test 3103 / dsh-group2 3104` | 新 acbctl `sessions` 输出 |

注意：系统 `/run/current-system/sw/bin/acbctl` 是旧构建，读不到库里的会话；真机验证必须用
`/nix/store/kq44xkbb…-acbl-0.1.0/bin/acbctl`（supervisor.toml 里 `ACBCTL_BIN` 指的那个）。

## 2. 目标架构

### 2.1 配置边界

配置文件只保留启动前必须知道的参数：

```text
[storage]  [endpoint]  [console]  [secrets]  [file_relay]
[notifications]  # 只留 enabled/url/topic/token
```

其余全部入库：

| 数据 | 归属 |
|---|---|
| 连接 / harness / 人格 / 记忆 / 会话与订阅 | ACB DB `config_objects` |
| 规则 / 房间 trust / 审批路由 / 角色 / 指派 | ACB DB `policy_*` |
| 模型后备链 | ACB DB `model_fallbacks` |
| 插件注册 / 策略 / 存储 / 配置 / 密钥引用 | ACB DB `plugin_*` |
| 外部插件的业务数据 | 插件自己的库 |
| 解密钥 `ACB_KEY` / `ACB_KEY_FILE` | 库外（env / 0600 文件） |

### 2.2 权限模型

**模型定位**：RBAC + ABAC 是内置 `CorePolicy` 的实现，不是 core 的契约。core 只定义
固定能力枚举、`PolicyRequest` / `Decision` 形状、fail-closed、票据与审计；换一个
ReBAC / ABAC-only / OPA / Cedar 实现，只要返回同样的 `Decision`，core 和业务插件都
不需要改。

为什么默认选 RBAC + ABAC：这个域的主体是人、agent、插件，资源是房间/连接/插件工具，
天然适合「角色给能力 + 属性限范围」；票据再处理临时例外。它的边界是：没有完整关系图
（owner/member/approver 的传递关系）、没有时间/环境条件、没有委托链。需要这些时再换
provider，不把关系模型塞进 core。

```text
effective = (floor ∪ role_caps(assignments) ∪ rules) − deny
```

1. `floor`：core 内置的订阅可达性、管理员私信特权等，保持今天语义。
2. `role_caps`：角色能力集合，按 `scope_kind/scope_id` 过滤后参与并集。
3. `rules`：现有 `GrantRule`，继续按 subject + place + scope + target + capability 匹配。
4. `deny`：显式减法，优先级最高。
5. 内置角色 4 个（system/system_admin/bot_admin/user）只用于展示与迁移，求值仍走 floor，保证 36 格 matrix 零差异。
6. 自定义角色带 `capability_snapshot` / `role_snapshot`；角色扩容不追认旧指派，旧指派 `needs_review=1`。
7. 能力枚举封闭；插件只能声明 `plugin:<id>:<name>` 命名空间能力，不能 mint core 能力名。
8. 插件能力风险元数据由插件声明、core 校验并存库；未知/缺省按 high + 需要票据。

### 2.2.1 策略数据归属与替换

1. `policy_rules` / `policy_rooms` / `policy_accounts` / `policy_admins` /
   `policy_roles` / `policy_assignments` 是**内置 `CorePolicy` 的数据**，不是 core 的
   普适模型。
2. 换成插件 provider 后，这些表不再参与判定；插件用自己的存储（builtin 或 external）。
   两套数据不自动迁移，切换 provider 是显式动作，必须确认。
3. 控制台在 provider=core 时显示「规则/角色/指派」；切到插件 provider 后，这些页面
   变为只读或隐藏，改为渲染该插件声明的配置页，避免两个真源。
4. 其他插件开发者只需要知道固定能力名和 `plugin:<id>:<name>` 命名空间，不需要知道
   RBAC/ABAC 的表结构。
5. `acbctl grants` 在非 core provider 下只做只读说明；`explain` 由 provider 自己实现，
   输出字段仍按 core 约定。

### 2.3 票据审批

批准一张票据必须同时满足：

1. 审批人持有 `access_approve`，且在**当前审批房间**范围内有效。
2. 审批人持有票据申请的每一个能力，且范围覆盖票据目标（房间/连接/插件工具）。
3. `account_secret` / `destructive` / `critical` / `policy_write` / `plugin_admin` 只允许 system_admin 或 break-glass。

票据消息里直接列出「需要能力 + 目标范围」；拒绝也回话，不静默。
迁移时打印「原来靠 trusted 群批准、现在需要补 `approver` 角色」的房间清单。

### 2.4 插件能力（混合模型）

1. core 永远先判 `plugin` 粗门禁：这个会话/角色能不能用这个插件。
2. 插件声明了 `plugin:<id>:<name>`，core 在粗门禁之后再判细能力；没声明就交给插件内部。
3. `requires_ticket`、`risk`、`scopes` 存 `plugin_capabilities`，core 取 `max(core 下限, 插件声明)`。
4. 权限插件只回答 allow/deny，不读 manifest，不参与票据门槛。
5. 插件之间不互相调用；所有判定经 core 的 `Policy` facade。

### 2.4.1 权限插件载体

契约放在 `plugin-api`，三种载体共用同一套可序列化类型：

1. native：`trait PolicyProvider`，直接注册。
2. `.so`：C ABI 符号 `acb_policy_decide`，输入/输出为 JSON 字节串，语义由
   `plugin-api` 的同一组函数定义。
3. Lua：`policy` 模块的 `decide(req)`，返回 table；字段名与 JSON 契约一致。

规则：

1. 三种载体都能注册 `kind='policy'` 的 provider，注册面一样宽；不允许出现只有
   native 能用的字段或回调。
2. v1 只交付 native 实现示例；`.so` / Lua 提供 stub 与同一组 golden corpus 的
   一致性测试，不要求都写完整策略。
3. 全部在进程内调用，不引入 IPC 延迟；进程外 policy provider 不在本轮。
4. `PolicyRequest` 只含 `(subject, place, target, capability, scope, session, room,
   policy_version)`，不含 RBAC/ABAC 表结构；provider 也不能依赖这些表。
5. 返回 `Decision { effect, rule_id, reason, policy_version }`；`rule_id` 必填、可追溯；
   非法 Decision、超时、panic 一律 deny。

### 2.5 插件存储

| 事项 | builtin | external |
|---|---|---|
| 建表/迁移 | ACB | 插件 |
| 查询接口 | ACB `plugin_records` / KV API | 插件自己的 DB 驱动 |
| 备份/保留期 | ACB | 插件 |
| 库位置 | ACB 命名空间 | ACB 存 `location` + `location_kind` 并传给插件 |
| 凭据 | ACB secrets | `plugin_secret_refs` / 加密字段 |
| 控制台数据页 | ACB 通用 CRUD | 插件声明的 resources/actions，或 `ui_url` |

external 的准确定义：插件自己加载数据库、自己建表/迁移/备份/降级；ACB 不提供 StorageHandle、
不转发 SQL、不打开它的库，只存位置与凭据并传给插件。

### 2.6 密钥

1. 新增 `secrets` 表；默认 `encrypted=1`，密文 `v1:<kid>:...`。
2. 解析顺序：库内优先，找不到再回落 env/file 来源；迁移后可以删旧来源。
3. `ACB_KEY_FILE`（0600，优先）/ `ACB_KEY`（env）只提供解密钥，不进库、不进配置文件。
4. `plugin_config.encrypted=1` 的字段走同一套加密；控制台允许查看明文，但查看动作记审计，列表默认打码。
5. 迁移前导出加密备份；密钥缺失时遇到密文启动失败，不能当明文读。

### 2.7 运行时设置

`default_on_turn_error`、`delivery_limit`、`turn_timeout_secs`、`tickets.*`
（`request_cooldown_secs` / `admin_cooldown_secs` / `auto_expire` / `pending_ttl_secs`）、
`notifications.min_interval_secs`、`notifications.in_band.*` 都是运行时可变的全局默认值，
不应留在配置文件。

1. 新增 `settings` 表（schema 16）；初始值写在代码常量里，不从配置文件导入。
   缺 key 时按代码默认值填充，不做「首次从文件 seed」。
2. 配置文件里这些键直接忽略并打警告，不保留第二个来源；需要改值走控制台或 `acbctl settings set`。
3. 迁移时 `acbctl settings diff` 只打印「当前文件值 vs 代码默认值」的差异，由人显式
   `set`，不自动导入。
4. 控制台「设置」页可改；`config_write` 能力控制写入；改完下一轮生效。
5. 仍留在配置文件的只有真正启动必需：`[storage]`、`[endpoint]`、`[console]`、
   `[secrets]`、`[file_relay]` 的监听/目录、`[notifications]` 的 url/topic。

### 2.8 Agent 配置工具

新增工具：

```text
config        op=list|get|set|delete  kind=session|persona|memory|harness|connection
policy        op=list|get|set|roles|assign
plugin_config op=list|get|set
plugin_action name=<action>
```

新增能力：`config_read`（低危）、`config_write`（高危）、`policy_write`（最高危）、
`plugin_admin`（高危）。写入复用控制台同一套校验与库写入函数，不新增第二写路径；
所有写操作落 `policy_audit`。

## 3. Schema 变更

### 3.1 Schema 16：RBAC + 密钥 + 审计 + 设置

```sql
CREATE TABLE policy_roles (
  id                TEXT PRIMARY KEY,
  name              TEXT NOT NULL UNIQUE,
  description       TEXT NOT NULL DEFAULT '',
  builtin           INTEGER NOT NULL DEFAULT 0 CHECK (builtin IN (0,1)),
  capabilities_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(capabilities_json)),
  created_at        INTEGER NOT NULL,
  updated_at        INTEGER NOT NULL,
  version           INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE policy_assignments (
  id                  TEXT PRIMARY KEY,
  subject_kind        TEXT NOT NULL CHECK (subject_kind IN ('subject','account','member')),
  subject_id          TEXT NOT NULL,
  role_id             TEXT NOT NULL REFERENCES policy_roles(id) ON DELETE CASCADE,
  scope_kind          TEXT NOT NULL CHECK (scope_kind IN ('global','connection','room')),
  scope_id            TEXT,
  capability_snapshot TEXT NOT NULL CHECK (json_valid(capability_snapshot)),
  role_snapshot       TEXT NOT NULL CHECK (json_valid(role_snapshot)),
  needs_review        INTEGER NOT NULL DEFAULT 0 CHECK (needs_review IN (0,1)),
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL
);
CREATE INDEX policy_assignments_subject ON policy_assignments(subject_kind, subject_id);

CREATE TABLE secrets (
  id               INTEGER PRIMARY KEY,
  name             TEXT NOT NULL UNIQUE,
  value_ciphertext TEXT NOT NULL,
  encrypted        INTEGER NOT NULL DEFAULT 1 CHECK (encrypted IN (0,1)),
  key_id           TEXT,
  source           TEXT,
  created_at       INTEGER NOT NULL,
  updated_at       INTEGER NOT NULL
);

CREATE TABLE settings (
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL CHECK (json_valid(value_json)),
  updated_at INTEGER NOT NULL
);

CREATE TABLE policy_audit (
  id             INTEGER PRIMARY KEY,
  at             INTEGER NOT NULL,
  session        TEXT,
  subject        TEXT NOT NULL,
  place          TEXT NOT NULL,
  capability     TEXT NOT NULL,
  target_kind    TEXT NOT NULL,
  target_id      TEXT NOT NULL,
  decision       TEXT NOT NULL CHECK (decision IN ('allow','deny')),
  basis          TEXT NOT NULL,
  policy_version INTEGER NOT NULL,
  approver       TEXT,
  request_id     TEXT
);
CREATE INDEX policy_audit_at ON policy_audit(at);
```

迁移：

1. 创建四表；插入 4 个内置角色（`role:system` / `role:system_admin` / `role:bot_admin` / `role:user`）。
2. 现有 11 条规则原样保留，不改语义。
3. `policy_audit` 从空开始；不影响旧日志。

### 3.2 Schema 17：插件平台

```sql
CREATE TABLE plugin_registry (
  id                  TEXT PRIMARY KEY,
  kind                TEXT NOT NULL DEFAULT 'tool' CHECK (kind IN ('tool','policy')),
  trust_tier          TEXT NOT NULL DEFAULT 'tool' CHECK (trust_tier IN ('tool','policy')),
  command             TEXT NOT NULL,
  args_json           TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(args_json)),
  timeout_ms          INTEGER NOT NULL DEFAULT 5000 CHECK (timeout_ms BETWEEN 100 AND 60000),
  command_kind        TEXT NOT NULL DEFAULT 'absolute'
                      CHECK (command_kind IN ('absolute','profile','nix_store','relative')),
  command_fingerprint TEXT,
  manifest_json       TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(manifest_json)),
  schema_version      INTEGER NOT NULL DEFAULT 0,
  source_seed         TEXT,
  enabled             INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  version             INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE plugin_capabilities (
  plugin_id       TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  capability_id   TEXT NOT NULL,
  risk            TEXT NOT NULL DEFAULT 'high' CHECK (risk IN ('low','medium','high')),
  requires_ticket INTEGER NOT NULL DEFAULT 1 CHECK (requires_ticket IN (0,1)),
  scopes_json     TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(scopes_json)),
  description     TEXT NOT NULL DEFAULT '',
  PRIMARY KEY (plugin_id, capability_id)
);

CREATE TABLE plugin_policy (
  plugin_id         TEXT PRIMARY KEY REFERENCES plugin_registry(id) ON DELETE CASCADE,
  enabled           INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  scope_kind        TEXT NOT NULL DEFAULT 'room' CHECK (scope_kind IN ('room','account','global')),
  scope_id          TEXT,
  capabilities_json TEXT NOT NULL DEFAULT '["tool_call"]' CHECK (json_valid(capabilities_json)),
  rate_limit_json   TEXT CHECK (rate_limit_json IS NULL OR json_valid(rate_limit_json)),
  lifecycle         TEXT NOT NULL DEFAULT 'resident' CHECK (lifecycle IN ('resident','on_demand')),
  allow_unknown     INTEGER NOT NULL DEFAULT 0 CHECK (allow_unknown IN (0,1)),
  updated_at        INTEGER NOT NULL
);

CREATE TABLE plugin_storage (
  plugin_id   TEXT PRIMARY KEY REFERENCES plugin_registry(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL DEFAULT 'builtin' CHECK (kind IN ('builtin','external')),
  location_kind TEXT NOT NULL DEFAULT 'none' CHECK (location_kind IN ('none','path','dsn')),
  location    TEXT,
  data_class  TEXT NOT NULL DEFAULT 'domain' CHECK (data_class IN ('domain','policy')),
  backup      INTEGER NOT NULL DEFAULT 0 CHECK (backup IN (0,1)),
  retention   TEXT,
  updated_at  INTEGER NOT NULL,
  CHECK (kind = 'builtin' OR location IS NOT NULL)
);

CREATE TABLE plugin_config (
  plugin_id  TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  key        TEXT NOT NULL,
  value_json TEXT NOT NULL CHECK (json_valid(value_json)),
  type       TEXT NOT NULL CHECK (type IN ('string','integer','number','boolean','array','object')),
  encrypted  INTEGER NOT NULL DEFAULT 0 CHECK (encrypted IN (0,1)),
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (plugin_id, key)
);

CREATE TABLE plugin_secret_refs (
  plugin_id TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  name      TEXT NOT NULL,
  secret_id INTEGER NOT NULL REFERENCES secrets(id),
  PRIMARY KEY (plugin_id, name)
);

-- builtin 通用 CRUD：一行一条资源记录，不动态建表
CREATE TABLE plugin_records (
  plugin_id  TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  resource   TEXT NOT NULL,
  record_id  TEXT NOT NULL,
  value_json TEXT NOT NULL CHECK (json_valid(value_json)),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (plugin_id, resource, record_id)
);
```

迁移：

1. 读 `config.d/80-issue-tools.toml`，写一条 `plugin_registry`（`issue.tools`，`command_kind=nix_store`，指纹）。
2. 写 `plugin_policy`（enabled=1，scope=global，capabilities=`["tool_call"]`）。
3. 写 `plugin_storage`（builtin，data_class=domain）。
4. 原文件移到 `config.d/history/80-issue-tools.toml.archived-20261110`。

### 3.3 Schema 18：策略插件（P6，可选）

```sql
CREATE TABLE policy_binding (
  id             INTEGER PRIMARY KEY,
  plugin_id      TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  enabled        INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  priority       INTEGER NOT NULL DEFAULT 0,
  fallback       TEXT NOT NULL DEFAULT 'deny' CHECK (fallback IN ('deny','core')),
  cache_ttl_ms   INTEGER NOT NULL DEFAULT 0,
  allowlisted_by TEXT,
  allowlisted_at INTEGER NOT NULL,
  UNIQUE(plugin_id)
);
```

### 3.4 迁移与回滚

1. 迁移分两次发版：schema 16（RBAC/密钥/审计）→ schema 17（插件平台）→ schema 18（可选）。
2. 每次换代前备份 `state/acb.db`、`-wal`、`-shm`；先停桥再拷。
3. schema 是单向门：回滚只能恢复备份，旧代不认新 schema。
4. 迁移标记：`meta.policy_roles_initialized`、`meta.plugins_initialized`，避免「清空后重启被旧文件复活」。

## 4. 接口面

### 4.1 控制台 API

```text
GET  /api/policy                       # rules / rooms / accounts / admins / roles / assignments
POST /api/policy                       # 整份校验后写库，置 pending，轮次边界生效
GET  /api/plugins                      # 注册信息 + manifest + 状态 + 存储类型
GET  /api/plugins/{id}/config          # 配置项（加密字段打码）
POST /api/plugins/{id}/config          # 按 manifest schema 校验后写库
GET  /api/plugins/{id}/resources/{res} # builtin 通用 CRUD；external 由插件实现
POST /api/plugins/{id}/actions/{name}  # 自定义动作
POST /api/secrets                      # 写加密密钥（值不回显）
```

### 4.2 acbctl

```text
acbctl sessions [--config PATH]                  # 输出显式 driver_port
acbctl sessions set-port <id> <port>             # 新增/调整端口
acbctl grants explain ...                        # basis 含 role/assignment
acbctl grants roles|assign|unassign
acbctl secrets migrate [--check]
acbctl plugin list|add|remove|enable|disable|export|import
acbctl policy bind|unbind|status
```

### 4.3 插件 manifest

```json
{
  "id": "issue.tools",
  "kind": "tool",
  "config": {
    "type": "object",
    "properties": {
      "default_project": { "type": "string", "title": "默认项目" },
      "max_results": { "type": "integer", "minimum": 1, "maximum": 100, "default": 20 }
    }
  },
  "storage": { "kind": "builtin" },
  "resources": [
    {
      "name": "issues",
      "primary_key": "id",
      "crud": ["create", "read", "update", "delete"],
      "schema": {
        "type": "object",
        "properties": {
          "title": { "type": "string" },
          "state": { "type": "string", "enum": ["open", "closed"] }
        }
      }
    }
  ],
  "actions": [
    {
      "name": "sync",
      "label": "同步",
      "params_schema": { "type": "object", "properties": { "force": { "type": "boolean" } } }
    }
  ],
  "capabilities": [
    { "id": "create", "risk": "medium", "requires_ticket": false, "scopes": ["room"] }
  ],
  "ui_url": null
}
```

规则：

1. `config` / `resources` / `actions` 用 JSON Schema 子集（type、properties、required、enum、minimum、maximum、default、items）。
2. 密钥形态的 key/value 硬拒绝；需要密钥时用 `plugin_secret_refs` 或 `encrypted=1` 字段。
3. `ui_url` 默认允许，但控制台用 sandbox iframe + 独立 origin；全局开关可关。
4. 插件作者不改 ACB 代码；注册只写 manifest + 可执行文件。

## 5. 阶段计划

### P0 仓库配置清理（0.5d）

任务：

1. 新建 `config.deprecated/`，把仓库根 `config.toml` 移为 `config.deprecated/config.toml.dev-20261109`。
2. `config.deprecated/README.md` 写明「不是运行配置，仅历史参考」。
3. `.gitignore` 增加 `config.deprecated/`。
4. `deploy/acb-up.sh`、`deploy/start-drivers.sh` 默认配置改为
   `ACB_CONFIG_FILE` 优先、否则 `$ACB_DATA/config/config.toml`。
5. `deploy/verify.sh` 的「本机文件未入库」检查保留；把 `config.deprecated/` 加入禁止跟踪清单。不要改成检查 `config.example.toml`——示例是跟踪文件，会误报。
6. `config.example.toml` 首行加「业务对象以库为准，示例只描述形状」。

验收：

```bash
git check-ignore -v config.deprecated/config.toml.dev-20261109
rg -n '\$REPO/config.toml' deploy/ || true
./deploy/verify.sh --quick
```

### P1 会话驱动对账（3d）

任务：

1. `crates/kernel/src/config.rs`：`Session` 增加 `driver_port: Option<u16>`，解析 `driver_port`。
2. `crates/kernel/src/config.rs`：加载时校验 dsh 会话必须有 `driver_port`，且全局唯一。
3. `crates/acb/src/sessions.rs`：`sessions` 输出显式端口；dsh 缺端口报可操作错误。
4. 新增 `crates/acb/src/sessions.rs` 的 `set-port` 子命令；迁移 4 个 dsh 会话为
   `3101/3102/3103/3104`。
5. `deploy/presets/drivers/drivers_acb.sh` 改成常驻对账：
   - 每 5 秒读 `acbctl sessions`；
   - 指纹 = `id + token hash + port + preset + model`；
   - 新会话 spawn；删除/禁用 SIGTERM；指纹变化重启该驱动；
   - 单驱动崩溃指数退避重启，不整组退出；
   - `--once` 模式用于测试。
6. 新增 `deploy/verify-drivers-reconcile.sh`：假 acbctl + 假 driver，断言 spawn/kill/restart。

验收：

```bash
# 新增会话后 10 秒
acbctl sessions --config ~/.local/share/acb/config/config.toml
ss -ltn | grep 3105
# 删除后 10 秒
ss -ltn | grep 3105 || echo released
```

### P2 RBAC + 票据审批 + 运行时设置（6.5d）

任务：

1. `crates/kernel/src/grants.rs`：新增 `AccessApprove`，补风险映射、序列化、解析。
2. `crates/kernel/src/policy.rs`：`PolicySnapshot` 增加 roles/assignments；`CorePolicy::decide`
   加 `role_caps` 并返回 basis。
3. `crates/kernel/src/storage/sqlite.rs`：schema 16 四表；`policy_load` / `policy_replace`
   扩展；迁移插入 4 个内置角色。
4. `crates/kernel/src/engine.rs`：启动加载角色；`replace_policy` 写库；`approval_allowed`
   改为「access_approve + 每个申请能力 + 范围覆盖 + 高危限制」。
5. `crates/bridge/src/console.rs`：`/api/policy` 返回/接收 roles/assignments。
6. `crates/bridge/src/console.html`：权限页拆「规则 / 角色 / 指派」。
7. `crates/acb/src/grants.rs`：`explain` 输出 `role:<id>#<assignment>`；新增 `roles` / `assign` / `unassign`。
8. 迁移脚本：打印需要补 `approver` 角色的房间。
9. `settings` 表：代码常量作为初始值，缺 key 时填充；**不从配置文件导入**。
   `acbctl settings diff` 只打印文件值与代码默认值的差异，由人显式 set。
10. 测试：`tests/policy_roles.rs`、`tests/ticket_approval_scope.rs`、`tests/settings.rs`、
    matrix 零差异。

验收：

```bash
cargo test -p acb-kernel --test policy_roles
cargo test -p acb-kernel --test ticket_approval_scope
acbctl grants matrix --config ... --format tsv > /tmp/new.tsv
diff -u /tmp/p1-grants-baseline.tsv /tmp/new.tsv   # 应为空
```

### P2.5 密钥入库（2d，与 P2 同发版）

任务：

1. `crates/kernel/src/crypto.rs`：`ACB_KEY_FILE` / `ACB_KEY` 解析；`v1:<kid>:` AEAD 加解密。
2. `crates/kernel/src/storage/sqlite.rs`：`secrets` 表；读写接口。
3. `crates/kernel/src/config.rs`：`{ secret = "name" }` 先查库、再回落 sources。
4. `crates/acb/src/secrets.rs`：`migrate` / `--check`；只打印名字与来源。
5. 测试：往返、密钥缺失失败、迁移只读。

验收：

```bash
acbctl secrets migrate --check
acbctl secrets migrate
mv ~/.local/share/acb/secrets/session-tokens.env /tmp/
systemctl --user restart acb-bridge
acbctl component status   # 5 healthy
```

### P3 插件注册与存储入库（4d）

任务：

1. `crates/kernel/src/storage/sqlite.rs`：schema 17 表；加载/保存。
2. `crates/kernel/src/config.rs`：`tool_plugins` 从库读；文件只做 seed。
3. `crates/kernel/src/plugin.rs` / `plugin_meta.rs`：manifest 解析、能力声明校验、
   command 指纹、存储类型。
4. `crates/bridge/src/main.rs`：按库安装 `ProcessToolLoader`；command 不可执行时 degraded。
5. `crates/bridge/src/console.rs`：`/api/plugins` 带 manifest / storage / capabilities。
6. `crates/acb/src/plugin.rs`（新增）：`list/add/remove/enable/disable/export/import`。
7. 迁移 `80-issue-tools.toml` 进库并归档。
8. 测试：注册往返、指纹漂移、禁用、builtin/external 选择。

验收：

```bash
acbctl plugin list
mv ~/.local/share/acb/config/config.d/80-issue-tools.toml /tmp/
systemctl --user restart acb-bridge
acbctl component status
curl -s localhost:3100/api/plugins -H "Authorization: Bearer ..." | jq
```

### P4 插件配置页 + CRUD + 动作（6d）

任务：

1. `plugin_registry.manifest_json` 增加 `config/resources/actions` 校验。
2. `plugin_config` 写入校验：类型、枚举、范围、密钥扫描、`encrypted`。
3. 新增 `PluginAdmin` trait：`list/get/create/update/delete/action`；
   in-process 与 external process 两套实现。
4. builtin 通用 CRUD 走 `plugin_records`；external 通过插件自己的 JSON-RPC 方法。
5. `console.html` 插件 tab：配置表单、资源表格、动作按钮；按 schema 渲染。
6. 示例插件 `examples/plugin-demo`：1 个配置项、1 个资源、1 个动作；README。
7. 测试：schema 校验、CRUD 往返、动作调用、密钥拒绝。

验收：

```bash
# 注册示例插件，改配置，增删改查，执行动作
acbctl plugin add examples/plugin-demo/manifest.json --command ...
curl -s localhost:3100/api/plugins/plugin-demo/config ...
```

手动验收：控制台插件页截图；示例插件全程未改 `crates/kernel`。

### P5 Agent 配置工具（4d）

任务：

1. `crates/kernel/src/grants.rs`：新增 `config_read` / `config_write` / `policy_write` / `plugin_admin`。
2. `harnesses/shared/tools.mjs`：新增 `config` / `policy` / `plugin_config` / `plugin_action`。
3. `crates/kernel/src/tools.rs` / `engine.rs`：处理器复用控制台写库函数；能力校验；审计。
4. 写操作默认策略：system_admin 私信直接写 + 审计；其他人票据；`policy_write` 一律票据。
5. 测试：普通成员 deny、管理员 allow、写操作票据、审计行。

验收：

```bash
# 群里普通成员
# config set session ... -> tool_not_allowed
# 管理员私信
# config get session dsh-dev -> ok
# config set ... -> 票据；批准后下一轮生效
```

### P6 策略插件（4d，可选）

任务：

1. schema 18 `policy_binding`；`acbctl policy bind/unbind/status`。
2. `crates/kernel/src/policy.rs`：`PolicyProvider` 解析；timeout、fail-closed、fallback。
3. 审计：`policy_provider` / `policy_version` / `basis`。
4. 修复 `engine.rs:5836-5839` 的外部 access provider 旁路。
5. `plugin-api` 增补 policy provider 契约：native trait、`.so` C ABI、Lua 模块三种载体
   使用同一套可序列化 `PolicyRequest` / `Decision`；三种载体的注册面同样宽，但 v1 只
   交付 native 示例，`.so` / Lua 用 stub 做一致性测试，不要求都实现完整策略。
6. 测试：全拒插件、panic、超时、fallback=core；三载体同一组 golden corpus 决策一致。

验收：

```bash
acbctl policy bind test-policy --fallback deny
# 所有工具调用被拒，审计含 policy_provider
acbctl policy unbind
# 恢复 CorePolicy
```

### P7 控制台其余 tab（3d，可与 P4 并行）

J1/J2/J4：sessions / feedback / config 三个 tab 补成概览质量；窄屏卡片化；深浅色切换。

## 6. 测试与门禁

1. 每个阶段先跑 `./deploy/verify.sh`，再真机验收；不允许只跑单测就换代。
2. 权限改动必须跑 36 格 matrix 与基线 diff，零差异才算过。
3. 插件改动必须有「不改 core」的示例插件验收。
4. 控制台改动必须有桌面与窄屏截图。
5. 真机换代：备份三件套 → `nix build .#payload` → `profile add --config` → `profile switch` → `systemctl --user restart acb-bridge` → `acb-status.sh`。

## 7. 风险

1. Schema 16/17 单向迁移；回滚只能恢复备份。
2. 插件 command 进库后 nix store 路径会随 GC 失效；必须做指纹与可执行检查，失败 degraded。
3. external 存储的备份/迁移由插件负责；ACB 只能显示状态，不能替它修数据。
4. 票据审批加范围会让现有 trusted 群一次性失去「谁都能批」；迁移必须打印补角色清单。
5. 策略插件是最高权点；没有 allowlist / fail-closed 就是全站提权。
6. 密钥迁入库后备份文件成为新的泄露面；备份必须加密、0600、不进仓库。
7. 控制台一次加三块容易做成半成品；按 P2/P3/P4 分开验收。

## 8. 不在本轮

1. `principals` / `relations` / `overrides` / 委托链 / 判定缓存（设计稿 §3.3-3.5 的后半）。
2. 插件市场、签名与远程分发。
3. WASM 载体改造。
4. 进程外 policy provider（v1 只做 in-process）。
5. 动态 DDL 的插件分表（v1 builtin 用 `plugin_records` 通用 CRUD）。

### 8.1 下一期：模型后备链

用户 2026-11-10 提出：后备链可能不生效，且是全局单例；应按 ACB 会话配置，一个 ACB
会话对应一个 harness 会话，可用模型只由 harness 自己知道。

现状核实：

1. 链存在全局表 `model_fallbacks`，桥每轮把同一份链下发给所有会话（`engine.rs:8436`）。
2. dsh 驱动有重试循环（`#modelAttempts`），但只在「零桥工具调用 + 可重试模型失败」时重放。
3. `classifyTurnReason` 只看 `reason.error.code`；2026-11-10 09:11 的日志里
   content-blocked 的 `code` 是 `INVALID_REQUEST`，正文才含 `content-blocked`，
   按当前判定不会进后备链。这条需要复现确认，但很可能是「看起来不生效」的直接原因。

下一期设计（用户 2026-11-10 修订：不再要求「零工具调用」，改为会话级模型列表 + 冷却回退）：

1. 链改成 per-session：`[[sessions]]` 增加 `models = ["provider/model", ...]`，第一项是
   主模型；桥只下发该会话的列表，不再全局。
2. harness 上报可选模型：新增 `session/models` 帧，驱动连接后返回自己 catalog 里的
   provider/model 列表；桥按会话缓存，控制台只展示 harness 认识的模型。
3. 冷却/熔断：每个 `(session, model)` 维护 `fail_count` 与 `cooldown_until`。模型失败
   （内容审核、限流、下线、超时、认证、过载、上下文超限等）就标记冷却，按指数退避
   （如 30s/2m/10m，设上限），选列表中下一个不在冷却的模型。
4. 粘性切换：后备模型成功后就把它设为该会话的 `active_model`，后续轮次先用它；
   原主模型冷却结束后做半开探测，成功就切回，失败继续冷却。
5. 状态持久化：桥新增 `session_model_state(session, active_model, cooldowns_json,
   updated_at)`；驱动在切换/成功时上报 `model_state` 事件，桥落库，重启后不丢。
6. 分类修正：`classifyTurnReason` 同时看 `code`、`message` 和响应体，补
   `content_blocked` / `rate_limited` / `context_length` / `max_tokens` 等匹配与测试。
7. 重放不再以「零桥工具调用」为门槛；重放方式由 harness 声明（用户 2026-11-10 修订）：
   - 驱动握手时上报 `replay_modes`：`resume` / `ids` / `full` / `none`。
   - `resume`（dsh / opencode 默认）：投递已成功、会话上下文还在，只发一条短
     continuation（「上一模型在本轮失败（reason），继续完成本轮；未完成记录 id: ...；
     不要重复已完成的工具调用」），不重发正文。
   - `ids`：只发未读记录 id，harness 自己用 `get_history` / `fetch_records` 取内容。
   - `full`：harness 没有持久会话时才整轮重发；这条路径才需要桥的 effect ledger
     去重（同名工具 + 目标 + 参数摘要）；destructive/critical 不自动重放。
   - `none`：本轮失败，切模型留给下一轮。
8. 因为 `resume` 保留同一个 harness 会话，agent 在上下文里能看到自己已经调用过的
   工具和结果，不会重复执行；桥不需要为 resume 路径做副作用去重。effect ledger 只
   服务 `full` 路径。
9. 本轮重试在驱动内部完成，桥不重新投递、不重复 ack；只有所有模型都失败才按现有
   失败通知处理。
10. 全局链迁移：把现有 3 条复制到各会话作为初始值，然后删全局表；不做隐式回退。

验收：一个内容审核失败的轮次，在 dsh 会话 A 上换到 A 配置的后备模型，只发 continuation
就完成；已发过消息也不产生第二条；A 切到后备模型后下一轮仍用它；主模型冷却结束且
探测成功后切回；同一个桥上的会话 B 不受影响；`full` 模式的重放有去重测试。

### 8.2 下一期：投递格式与水印

用户 2026-11-10 定稿：投递格式改为「触发唤醒前后 N 条消息 + 完整未读编号区间」，
消息重读由 agent 自行处理；只有**真的投递了具体内容**的消息，才允许 agent 把水位标为已读。

现状：`Delivery.records` 按 `delivery_limit` 连续投完整内容；`more` / `omitted` 只
告诉 harness「还有多少没投」。`ack_messages` 已经区分 `delivered`（本轮投过内容）和
`read`（本轮用读工具读过），不是 `delivered`/`read` 的编号会报 `not_delivered`。

目标格式：

```text
Delivery {
  trigger_id: Option<u64>,        // 触发这条唤醒的记录；时钟/攒批为空
  records: Vec<Record>,           // 只放 context_before + trigger + context_after 的完整内容
  unread_ranges: Vec<[u64; 2]>,   // 完整未读编号区间，闭区间，如 [1,20] [30,40]
  unread_count: usize,            // 区间覆盖的总条数
}
```

规则：

1. `context_before` / `context_after` 按**订阅**（会话×房间）可配，默认 3/3；
   同一个会话订阅安静私聊和吵闹群时可以不同。会话级只做默认值，订阅覆盖它。
2. 时钟/攒批唤醒的「最旧 N 条」也按订阅可配（`batch_head_count`，默认 3），
   其余全部用 `unread_ranges` 给编号。
3. `unread_ranges` 必须完整覆盖本轮所有未读编号，不截断、不抽样；用区间压缩，
   例如 `1-20, 30-40`。编号重复出现不是问题，agent 自己决定读哪些。
4. 重复投递计数复用现有 `deliveries.times`（不是新表）：`count_delivery` 已经
   记录每条消息被投过几次，`ack_messages` 成功后会清零。新增两个聚合字段：
   `unread_redelivered`（未读里投过 ≥2 次的条数）与 `max_times`（最高次数），
   放进 Delivery 供渲染。
5. `records` 里的记录仍然写 `note = "第 N 次投递；调 ack_messages 确认 #id 后不再重投"`；
   `unread_ranges` 的提示写成「未读 31 条，其中 12 条已投过 2 次以上」。
6. `delivered` 集合只包含 `records` 的编号；`unread_ranges` 里的编号不能直接 ack，
   必须先用 `get_history` / `fetch_records` 读到，进入 `read` 集合后才能 ack。
7. `ack_messages` 行为不变：只接受 `delivered ∪ read`，其余报 `not_delivered`。
8. 水位可能停在未读编号前，这是有意的；下一轮仍然给完整区间，agent 自己推进。
9. **不假设 agent 会主动 ack**：提示词末尾固定给「未确认编号/区间 + 读完后调 ack_messages」；
   投递次数 ≥3 且仍未确认的记录，控制台标 `stuck_unread`，并在下一轮提示里加一句
   「这些已投过 N 次，请确认或说明不处理」。不自动推水位。
10. 不做旧协议兼容：没有外部用户，协议版本直接 bump，所有仓库内 harness 一起改。

实现位置：

1. `crates/kernel/src/config.rs`：`Subscription` 增加 `context_before` / `context_after` /
   `batch_head_count`，默认 3/3/3；会话级可选默认值，订阅覆盖。
2. `crates/kernel`：`DeliveryPlan` 增加 `context` / `unread_ranges`；默认 composer 按
   `trigger_id` 或 `batch_head_count` 选 `records`，其余转区间；用现有
   `count_delivery` 算 `unread_redelivered` / `max_times`。
3. `crates/proto`：`Delivery` 增加 `trigger_id` / `unread_ranges` / `unread_count` /
   `unread_redelivered` / `max_times`，删掉不再使用的 `more` / `omitted`
   （或保留字段但置空，方便分阶段改）。
4. `harnesses/opencode/plugin/acb-wire.mjs` 的 `renderTurn`：把区间渲染成
   「未读编号：1-20, 30-40；其中 12 条已投过 2 次以上；需要时用 fetch_records 读取」。
   dsh / opencode / workbuddy / lobsterai 都 import 这个函数，改一处即可。
5. 控制台会话页增加 `stuck_unread` 指标（投递 ≥3 次且未确认的区间/条数）。
6. 投递是**结构化 JSON 帧**，不是字符串模板插值；`renderTurn` 负责把对象拼成最终
   prompt 字符串，所以「不兼容」指的是帧字段变了，不是模板参数变了。

验收：一次 @ 唤醒只投前后 3 条 + 完整区间；agent 读其中 2 条并 ack，水位只推进到
这 2 条能连续覆盖的位置；没读的编号不会被误标已读；下一轮仍能看到完整未读区间；
时钟唤醒只投最旧 3 条正文 + 完整区间；投过 2 次以上的记录在提示里有次数提醒；
连续 3 轮未确认的区间在控制台可见。

## 9. 开工顺序

P0 → P1 → P2（含 P2.5）→ P3 → P4 → P5 → P6。P7 可与 P4 并行。

每个阶段一个提交（或一组小提交），提交尾注写明「改了什么 / 验证命令 / 还剩什么」；
推送前给 `git diff --stat`，推送须你明示。
