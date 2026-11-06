# ACB Policy Store 终稿设计

状态：2026-11-06。**本文件取代**
[config-and-grants-model.md](../plan/config-and-grants-model.md) 与
[config-storage-audit.md](../plan/config-storage-audit.md)；那两页只保留为调研记录，
不作为实现依据。

范围：ACB bridge/kernel 的配置、授权、会话、插件策略与密钥。只写设计，不改代码、
不改生产配置。

---

## 0. 判定原则（放在最前）

> **会不会在运行时被改？会 → 库；改了得重新求值或重建 → 文件。**

这条原则决定单一数据源，不是“哪个文件方便放哪里”。

- **进库**：会话、插件、授权（角色 / 指派 / 房间 trust / approval_route / 例外）、
  运行态（审计、水位、订阅）、密钥（引用或加密真值）。
- **留文件**：ACB 自身组装件——端口分配、unit 与依赖、profile/generation 选择、
  二进制与脚本路径、supervisor 参数、DB 路径。
- **派生文件**：`config.active.toml` 迁移后只作历史/诊断，不再当源。
- **同一逻辑实体只有一个源**：同一行数据不得在 `config.toml`、`config.d`、Nix、
  控制台保存里各存一份靠顺序对账；按职责分表不等于同一行散落多处（见 §3.12）。

两条硬推论：

1. **Nix 降级为 seed**：`services.acb.sessions`、`modules.dsh.acbSessions`、provider
   注册片只在库为空时写入一次；库非空跳过并打日志。Nix 不再覆盖运行中的定义。
2. **preset 目录是派生物**：会话进库后，dsh preset 由 `preset reconciler` 从库渲染到
   preset 目录；Nix 生成的 preset 只作为 seed/初始内容。

---

## 1. 目标与非目标

### 1.1 目标

- 一个可动态调整的会话面：会话增删、绑定群、wake/clock、模型、启用状态、工具集、
  dsh preset/端口，都由库驱动，不再要求每次重建 Nix。
- 一个可动态调整的插件面：provider 注册、启用、作用域、能力、限流、超时、生命周期，
  由库驱动；`command/args` 也在库，Nix 只做 seed。
- 一个可解释的授权面：`effective = (floor ∪ grants) − deny`，显式 deny、角色/指派、
  房间 trust、关系、override、委托、审计都能回答“为什么允许/拒绝”。
- 一个可运维的密钥面：`token_ref`（默认）、`encrypted-store`（可选）、`plaintext`
  （仅本地调试）；库内不出现明文密钥，除非用户显式选择并接受风险。
- 一个可验证的迁移：旧 TOML 导入可 dry-run、可回滚；迁移后 `config.d` 只读历史；
  `export` 能与库逐条比对。

### 1.2 非目标

- 不做 Zanzibar 全关系图；只做 `owner/member/approver` 三种关系。
- 不做角色继承层级；角色扁平，组合靠多条 assignment。
- 不做通用 CRUD `ops`；能力保持固定枚举，写入/加载期硬校验。
- 不做权限市场、匿名授权、无限转授、跨平台主体隐式合并。
- 不把审计原始日志、access ticket、消息流水搬进配置库；运行态继续用 `state/acb.db`。
- 不把端口、unit、profile/generation、二进制/脚本路径、supervisor 参数搬进库。
- 不自己实现密码学；加密只用 SQLCipher 或现成 AEAD（age/AES-GCM）。

---

## 2. 单一数据源与逐项归属

### 2.1 三类存储

| 存储 | 内容 | 生命周期 |
|---|---|---|
| `state/policy.db` | sessions、session_rooms、session_tools、plugin_registry、plugin_policy、principals、roles、role_caps、assignments、relations、rooms、overrides、settings、secrets、audit、preset_manifests | 长期，可备份/导出 |
| `state/acb.db` | records、watermarks、acks、deliveries、notes、feedback、session_maps、meta | 运行态，现有单进程契约 |
| 文件/Nix | 端口、unit、profile/generation、二进制/脚本路径、supervisor、DB 路径、seed | 部署，可 diff/review |

`policy.db` 与 `acb.db` 分开：前者多写者、低频；后者高频、单进程。不要把一个高频
运行库和一个策略库混在同一个连接/事务域里。

### 2.2 归属表（逐项）

| 内容 | 现状 | 目标 | 结论 |
|---|---|---|---|
| endpoint/console/relay 监听地址、dsh 3080、driver 3101-3104、opencode 4097、snowluma 53001 | Nix `services.acb` + unit env | 文件/Nix | 留文件；改了要重启/重建 |
| unit/依赖/重启/超时/supervisor | Nix `modules.acb` | 文件/Nix | 留文件 |
| dsh profile/generation 选择、二进制/脚本路径 | Nix `modules.dsh`/`acb-dsh-exec` | 文件/Nix | 留文件 |
| issue 服务 listen/DB 路径/静态用户 | Nix `acb-issue.nix` | 文件/Nix | 留文件 |
| ntfy URL/min interval | Nix fragment | 文件/Nix | 留文件；topic 走 env |
| `[[connections]]` 拓扑（url/account/platform） | `config.toml` | 文件 | 留文件；token 走 env |
| `[[connections.routes]]`（Lua 路由） | `config.toml` | 文件 | 留文件（路由是部署/代码） |
| personas/memories/harnesses 定义 | `config.toml` | 文件 | 留文件（内容/路径定义） |
| **sessions（id/harness/persona/model/启用/端口/preset/工具集）** | `config.toml` + Nix sessions | `sessions`/`session_rooms`/`session_tools` | **进库**；Nix seed |
| **session access token** | `config.toml` 明文 | `secrets` 行，`mode=token_ref` | **进库只存引用**；真值 env/agenix |
| **subscriptions / wake / clock** | `config.toml` | `session_rooms.wake_json/clock_json` | **进库** |
| **plugin_registry（command/args/timeout）** | `config.d/80` + Nix | `plugin_registry` | **进库**；Nix seed；放弃 command 的 Nix 复现 |
| **plugin_policy（启用/scope/capabilities/rate_limit/lifecycle）** | 当前无 | `plugin_policy` | **进库** |
| **plugin_config（插件自定义运行期参数）** | 当前无 | `plugin_config` | **进库**；一 key 一行，数组/对象存 JSON |
| **plugin secrets** | env/agenix/文件 | `secrets` 行 + `plugin_secret_refs` | **进库只存引用** |
| **grants/roles/assignments** | base + 70 片 | `principals/roles/role_caps/assignments` | **进库** |
| **rooms trust/members/approval_route** | `[grants.rooms]` | `rooms`/`relations`/`approval_routes` | **进库** |
| **overrides** | 当前只有规则 | `overrides`（只 deny） | **进库** |
| `default_on_turn_error` | `config.toml` 顶层 | `settings` | **进库**（运行时可改） |
| `delivery_limit` / `turn_timeout_secs` | `config.toml` 顶层 | `settings` | **进库**（运行时可改） |
| `file_relay` | `config.toml` | 文件/Nix | 留文件（部署） |
| **`config.d/70`** | 文件 | `assignments` | **导入后归档**；只读历史 |
| **`config.d/80`** | 文件 | `plugin_registry`/`plugin_policy` | **导入后归档**；只读历史 |
| **`config.d/90`** | 文件（当前不存在） | `assignments/overrides/settings` | **控制台写库**；文件只做 export/seed |
| `config.active.toml` | 派生 | 历史/诊断 | 派生文件，不当源 |
| 运行态 records/watermarks/acks/deliveries/notes/feedback/session_maps | `state/acb.db` | 不变 | 留库 |
| issue 库 | `/var/lib/acb-issue/issues.db` | 不变 | 留库 |
| access tickets | 内存 | 不变 | 留内存；短 TTL |
| 审计原始日志 | `~/.local/state/acb/*.log` | 文件 + `audit` 表（判定事件） | 日志留文件；判定审计进库 |
| 密钥真值 | env/agenix | 不进库（token_ref/encrypted/store 例外） | 见 §7 |

### 2.3 被改判的条目

以下条目从“留文件/等规模”改为“进库”：

- sessions 身份、绑定群、端口/preset、工具集；
- subscriptions / wake / clock；
- session token（改为 `secrets.token_ref`，真值仍在 env）；
- plugin_registry（80 片）；
- plugin_policy；
- plugin secrets（改为 `secrets` 引用）；
- `90-console` 从覆盖文件改为写库；
- `default_on_turn_error`、`delivery_limit`、`turn_timeout_secs`；
- grants/rooms/approval_route/overrides（原本就是二期进库，现在统一归入 `policy.db`）。

以下条目保持文件/Nix，不因“配置太多”而搬库：

- 端口、unit、依赖、supervisor、profile/generation、二进制/脚本路径、DB 路径；
- `[[connections]]` 拓扑、Lua 路由、personas/memories/harnesses、file_relay；
- issue 服务与 issues.db；access tickets；审计原始日志。

---

## 3. 完整 Schema（DDL）

`policy.db` 独立文件；以下 DDL 为终稿结构。所有时间用 Unix 毫秒 `INTEGER`；
JSON 列必须 `json_valid`；外键开启。

### 3.1 版本与元信息

```sql
PRAGMA foreign_keys = ON;

CREATE TABLE schema_version (
  version INTEGER NOT NULL
);
INSERT INTO schema_version(version) VALUES (1);

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- 必填键：policy_version、seed_version、seed_source、secrets_mode、dsh_version
```

- `schema_version`：防“老程序读新库/新程序读老库”瞎猜。
- `policy_version`：单调递增；每次策略/会话/插件/密钥写事务 bump；缓存按它失效。
- `seed_version`/`seed_source`：记录 Nix seed 是否已写入；防每次启动重复 seed。
- `secrets_mode`：`token_ref|encrypted|plaintext`；启动日志打印，不打印值。
- `dsh_version`：preset reconciler 用来校验 `preset_schema_version`。

### 3.2 能力枚举

```sql
CREATE TABLE capabilities (
  name       TEXT PRIMARY KEY,
  class      TEXT NOT NULL CHECK (class IN ('resource','meta','risk','legacy')),
  grantable  INTEGER NOT NULL DEFAULT 1 CHECK (grantable IN (0,1)),
  delegable  INTEGER NOT NULL DEFAULT 0 CHECK (delegable IN (0,1)),
  risk_tier  TEXT NOT NULL CHECK (risk_tier IN ('low','medium','high','critical'))
);

INSERT INTO capabilities(name, class, grantable, delegable, risk_tier) VALUES
  ('room_read',      'resource', 1, 1, 'low'),
  ('room_write',     'resource', 1, 1, 'low'),
  ('room_history',   'resource', 1, 1, 'low'),
  ('account_read',   'resource', 1, 1, 'low'),
  ('account_message','resource', 1, 1, 'medium'),
  ('account_admin',  'resource', 1, 0, 'high'),
  ('tool_call',      'resource', 1, 0, 'high'),
  ('access_request', 'meta',     1, 1, 'low'),
  ('account_secret', 'meta',     0, 0, 'critical'),
  ('destructive',    'risk',     1, 0, 'high'),
  ('critical',       'risk',     1, 0, 'critical'),
  ('connection_read','legacy',   1, 0, 'low'),
  ('platform',       'legacy',   1, 0, 'low'),
  ('grant_manage',   'meta',     1, 1, 'high');
```

约束目的：能力名不在表里 -> 写入/加载直接拒绝；`account_secret` 不可常驻授予；
`account_admin`/`tool_call` 不可普通委托（见 §5）；`destructive`/`critical` 是风险层，
不是普通角色能力。

### 3.3 主体、角色、关系

```sql
CREATE TABLE principals (
  id          INTEGER PRIMARY KEY,
  platform    TEXT NOT NULL,
  external_id TEXT NOT NULL,
  kind        TEXT NOT NULL CHECK (kind IN ('user','bot','system')),
  display_name TEXT,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  UNIQUE(platform, external_id)
);

CREATE TABLE roles (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL
);

CREATE TABLE role_caps (
  role_id    TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
  capability TEXT NOT NULL REFERENCES capabilities(name),
  can_delegate INTEGER NOT NULL DEFAULT 0 CHECK (can_delegate IN (0,1)),
  PRIMARY KEY (role_id, capability)
);

CREATE TABLE relations (
  id            INTEGER PRIMARY KEY,
  subject_id    INTEGER NOT NULL REFERENCES principals(id) ON DELETE CASCADE,
  relation      TEXT NOT NULL CHECK (relation IN ('owner','member','approver')),
  resource_kind TEXT NOT NULL CHECK (resource_kind IN ('room','account','tool')),
  resource_id   TEXT NOT NULL,
  source        TEXT NOT NULL CHECK (source IN ('platform','explicit','seed')),
  since         INTEGER NOT NULL,
  until         INTEGER,
  evidence      TEXT,
  UNIQUE(subject_id, relation, resource_kind, resource_id)
);
CREATE INDEX relations_resource ON relations(resource_kind, resource_id, relation);
```

约束目的：主体身份唯一；角色名唯一；`owner/member/approver` 是事实，不是授权；
`relations` 不直接授予能力，只喂审批/floor。

### 3.4 指派（含能力/角色快照与委托）

```sql
CREATE TABLE assignments (
  id                  INTEGER PRIMARY KEY,
  subject_id          INTEGER NOT NULL REFERENCES principals(id),
  role_id             TEXT REFERENCES roles(id),
  scope_kind          TEXT NOT NULL CHECK (scope_kind IN ('global','group','private','room')),
  scope_id            TEXT,
  capability_snapshot TEXT NOT NULL CHECK (json_valid(capability_snapshot)),
  role_snapshot       TEXT CHECK (role_snapshot IS NULL OR json_valid(role_snapshot)),
  can_delegate        INTEGER NOT NULL DEFAULT 0 CHECK (can_delegate IN (0,1)),
  granted_by          INTEGER REFERENCES principals(id),
  granted_at          INTEGER NOT NULL,
  expires_at          INTEGER,
  parent_grant        INTEGER REFERENCES assignments(id),
  depth               INTEGER NOT NULL DEFAULT 0 CHECK (depth >= 0 AND depth <= 2),
  reason              TEXT NOT NULL,
  status              TEXT NOT NULL DEFAULT 'active'
                      CHECK (status IN ('active','revoked','expired','needs_review')),
  needs_review        INTEGER NOT NULL DEFAULT 0 CHECK (needs_review IN (0,1)),
  revoked_at          INTEGER,
  revoked_by          INTEGER REFERENCES principals(id),
  revocation_reason   TEXT,
  version             INTEGER NOT NULL DEFAULT 1,
  CHECK ((scope_kind = 'room' AND scope_id IS NOT NULL)
      OR (scope_kind <> 'room' AND scope_id IS NULL)),
  CHECK ((role_id IS NOT NULL) OR (capability_snapshot <> '[]'))
);
CREATE UNIQUE INDEX assignments_unique_active
  ON assignments(subject_id, COALESCE(role_id,''), scope_kind, COALESCE(scope_id,''))
  WHERE status <> 'revoked';
CREATE INDEX assignments_subject_scope
  ON assignments(subject_id, scope_kind, scope_id, status);
CREATE INDEX assignments_parent ON assignments(parent_grant);
CREATE INDEX assignments_expiry ON assignments(expires_at) WHERE expires_at IS NOT NULL;
```

约束目的：

- `capability_snapshot`/`role_snapshot` 防“角色日后扩容把历史授权追认成超额授权”。
- `scope_kind`/`scope_id` 防 scope 含义漂移；`room` 必须有具体 id。
- `depth <= 2` 防无限转授；`parent_grant` 供撤销级联。
- 唯一 active 索引防同一 subject×role×scope 重复授权。写入前必须先
  `UPDATE assignments SET status='expired' WHERE expires_at <= now AND status='active'`，
  否则到期的旧行会让新授权撞唯一索引。`overrides` 同理，写入前先标记过期行。

### 3.5 房间、审批、例外

```sql
CREATE TABLE rooms (
  id          TEXT PRIMARY KEY,
  trust       TEXT NOT NULL DEFAULT 'untrusted'
              CHECK (trust IN ('untrusted','trusted')),
  updated_at  INTEGER NOT NULL,
  version     INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE approval_routes (
  owner_kind  TEXT NOT NULL CHECK (owner_kind IN ('room','account')),
  owner_id    TEXT NOT NULL,          -- room id 或 connection:account
  seq         INTEGER NOT NULL,
  target_room TEXT NOT NULL,
  PRIMARY KEY (owner_kind, owner_id, seq)
);

CREATE TABLE overrides (
  id            INTEGER PRIMARY KEY,
  resource_kind TEXT NOT NULL CHECK (resource_kind IN ('room','account','tool')),
  resource_id   TEXT NOT NULL,
  subject_id    INTEGER REFERENCES principals(id),
  role_id       TEXT REFERENCES roles(id),
  capability    TEXT NOT NULL REFERENCES capabilities(name),
  effect        TEXT NOT NULL DEFAULT 'deny' CHECK (effect = 'deny'),
  reason        TEXT NOT NULL,
  granted_by    INTEGER REFERENCES principals(id),
  granted_at    INTEGER NOT NULL,
  expires_at    INTEGER,
  revoked_at    INTEGER,
  revoked_by    INTEGER REFERENCES principals(id),
  version       INTEGER NOT NULL DEFAULT 1,
  CHECK ((subject_id IS NOT NULL) <> (role_id IS NOT NULL))
);
CREATE UNIQUE INDEX overrides_unique_active
  ON overrides(resource_kind, resource_id, COALESCE(subject_id,-1),
               COALESCE(role_id,''), capability)
  WHERE revoked_at IS NULL;
```

约束目的：`overrides.effect` 只允许 deny，防“房间例外偷偷做加法”；`approval_routes`
把房间/账号路由规范化；`rooms.trust` 是 config axis，不是能力本身。

### 3.6 会话、订阅、工具集

```sql
CREATE TABLE sessions (
  id                    TEXT PRIMARY KEY,
  name                  TEXT,
  harness               TEXT NOT NULL,
  persona               TEXT NOT NULL,
  model                 TEXT,
  memories_json         TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(memories_json)),
  on_turn_error_json    TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(on_turn_error_json)),
  enabled               INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  status                TEXT NOT NULL DEFAULT 'active'
                        CHECK (status IN ('active','disabled','degraded','pending_delete')),
  status_reason         TEXT,
  driver_port           INTEGER,
  preset                TEXT,
  preset_schema_version INTEGER NOT NULL DEFAULT 1,
  secret_id             INTEGER REFERENCES secrets(id),
  source_seed           TEXT,
  created_at            INTEGER NOT NULL,
  updated_at            INTEGER NOT NULL,
  version               INTEGER NOT NULL DEFAULT 1,
  UNIQUE(driver_port)
);

CREATE TABLE session_rooms (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  room_id    TEXT NOT NULL,           -- 规范形式 qq:group/<id> / qq:private/<id>
  wake_json  TEXT NOT NULL CHECK (json_valid(wake_json)),
  clock_json TEXT CHECK (clock_json IS NULL OR json_valid(clock_json)),
  PRIMARY KEY (session_id, room_id)
);

CREATE TABLE session_tools (
  session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  tool       TEXT NOT NULL,
  allowed    INTEGER NOT NULL DEFAULT 1 CHECK (allowed IN (0,1)),
  PRIMARY KEY (session_id, tool)
);
```

约束目的：`driver_port` 唯一防端口冲突；`secret_id` 只引用密钥行；`session_rooms`
规范化群/私信；`session_tools` 是新增策略（当前工具面是计算出来的，不迁移旧字段）。

### 3.7 插件注册与策略

```sql
CREATE TABLE plugin_registry (
  id              TEXT PRIMARY KEY,
  kind            TEXT NOT NULL DEFAULT 'tool'
                  CHECK (kind IN ('tool','policy')),
  trust_tier      TEXT NOT NULL DEFAULT 'tool'
                  CHECK (trust_tier IN ('tool','policy')),
  command         TEXT NOT NULL,
  args_json       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(args_json)),
  timeout_ms      INTEGER NOT NULL DEFAULT 5000
                  CHECK (timeout_ms BETWEEN 100 AND 60000),
  command_kind    TEXT NOT NULL DEFAULT 'absolute'
                  CHECK (command_kind IN ('absolute','profile','nix_store','relative')),
  command_fingerprint TEXT,
  config_schema_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_schema_json)),
  schema_version   INTEGER NOT NULL DEFAULT 0,
  migrations_json  TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(migrations_json)),
  storage_api_version INTEGER NOT NULL DEFAULT 1,
  source_seed     TEXT,
  uninstalled_at  INTEGER,
  orphaned_at     INTEGER,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  version         INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE plugin_policy (
  plugin_id       TEXT PRIMARY KEY REFERENCES plugin_registry(id) ON DELETE CASCADE,
  enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  scope_kind      TEXT NOT NULL DEFAULT 'room'
                  CHECK (scope_kind IN ('room','account','global')),
  scope_id        TEXT,
  capabilities_json TEXT NOT NULL DEFAULT '["tool_call"]'
                    CHECK (json_valid(capabilities_json)),
  rate_limit_json TEXT CHECK (rate_limit_json IS NULL OR json_valid(rate_limit_json)),
  lifecycle       TEXT NOT NULL DEFAULT 'resident'
                  CHECK (lifecycle IN ('resident','on_demand')),
  external        INTEGER NOT NULL DEFAULT 0 CHECK (external IN (0,1)),
  allow_unknown   INTEGER NOT NULL DEFAULT 0 CHECK (allow_unknown IN (0,1)),
  updated_at      INTEGER NOT NULL,
  version         INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE plugin_config (
  plugin_id   TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  key         TEXT NOT NULL,
  value_json  TEXT NOT NULL CHECK (json_valid(value_json)),
  type        TEXT NOT NULL
              CHECK (type IN ('string','integer','number','boolean','array','object')),
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (plugin_id, key)
);
CREATE INDEX plugin_config_plugin ON plugin_config(plugin_id);

CREATE TABLE plugin_secret_refs (
  plugin_id TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  name      TEXT NOT NULL,
  secret_id INTEGER NOT NULL REFERENCES secrets(id),
  PRIMARY KEY (plugin_id, name)
);

CREATE TABLE plugin_storage (
  plugin_id   TEXT PRIMARY KEY REFERENCES plugin_registry(id) ON DELETE CASCADE,
  kind        TEXT NOT NULL DEFAULT 'builtin'
              CHECK (kind IN ('builtin','file')),
  data_class  TEXT NOT NULL DEFAULT 'domain'
              CHECK (data_class IN ('domain','policy')),
  name        TEXT,
  rebuildable INTEGER NOT NULL DEFAULT 0 CHECK (rebuildable IN (0,1)),
  retention   TEXT,
  backup      INTEGER NOT NULL DEFAULT 0 CHECK (backup IN (0,1)),
  backup_order INTEGER NOT NULL DEFAULT 0,
  statement_timeout_ms INTEGER NOT NULL DEFAULT 250
                  CHECK (statement_timeout_ms BETWEEN 10 AND 5000),
  max_rows     INTEGER NOT NULL DEFAULT 10000 CHECK (max_rows > 0),
  write_rows_per_sec INTEGER NOT NULL DEFAULT 1000 CHECK (write_rows_per_sec > 0),
  write_burst_rows INTEGER NOT NULL DEFAULT 2000 CHECK (write_burst_rows > 0),
  volume_justification TEXT,
  CHECK (kind = 'builtin' OR (name IS NOT NULL AND length(name) > 0)),
  CHECK (data_class = 'domain' OR kind = 'builtin'),
  CHECK (rebuildable = 0 OR backup = 0),
  CHECK (kind = 'builtin' OR rebuildable = 1
         OR (volume_justification IS NOT NULL AND length(volume_justification) > 0)),
  CHECK (name IS NULL OR (name NOT LIKE '%/%' AND name NOT LIKE '%..%'))
);

CREATE TABLE policy_binding (
  id              INTEGER PRIMARY KEY,
  plugin_id       TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  enabled         INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0,1)),
  priority        INTEGER NOT NULL DEFAULT 0,
  fallback        TEXT NOT NULL DEFAULT 'deny' CHECK (fallback IN ('deny','core')),
  cache_ttl_ms    INTEGER NOT NULL DEFAULT 0,
  allowlisted_by  INTEGER REFERENCES principals(id),
  allowlisted_at  INTEGER NOT NULL,
  UNIQUE(plugin_id)
);
```

约束目的：`command` 写入时校验可执行；`command_fingerprint` 记录内容指纹，用于
drift；`scope_kind` 防插件越权到别的资源；`capabilities_json` 必须是枚举子集；
`lifecycle` v1 只实现 `resident`，`on_demand` 预留；`external` 决定 `tool_call` 是否
高危。`kind`/`trust_tier` 把 policy provider 与普通 tool provider 分开；`policy_binding`
要求显式 allowlist、优先级和 fallback，防“装个插件就接管全量判定”。`plugin_config`
的 key/value 由 `plugin_registry.config_schema_json` 校验，防插件运行时参数绕过类型
约束。`plugin_storage` 默认 builtin；策略数据不得声明 file；file 的 name 只能是
basename、不允许路径穿越；`rebuildable` 与 `backup` 互斥，避免把可重建数据塞进备份。

### 3.8 设置、密钥

```sql
CREATE TABLE settings (
  key        TEXT PRIMARY KEY,
  value_json TEXT NOT NULL CHECK (json_valid(value_json)),
  updated_at INTEGER NOT NULL,
  version    INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE secrets (
  id          INTEGER PRIMARY KEY,
  owner_kind  TEXT NOT NULL CHECK (owner_kind IN
              ('session','plugin','console','notification','connection')),
  owner_id    TEXT NOT NULL,
  purpose     TEXT NOT NULL,
  mode        TEXT NOT NULL CHECK (mode IN ('token_ref','encrypted','plaintext')),
  token_ref   TEXT,
  ciphertext  BLOB,
  plaintext   TEXT,
  key_id      TEXT,
  nonce       BLOB,
  aad         TEXT,
  created_at  INTEGER NOT NULL,
  rotated_at  INTEGER,
  UNIQUE(owner_kind, owner_id, purpose),
  CHECK (
    (mode = 'token_ref' AND token_ref IS NOT NULL
      AND ciphertext IS NULL AND plaintext IS NULL)
    OR
    (mode = 'encrypted' AND token_ref IS NULL
      AND ciphertext IS NOT NULL AND plaintext IS NULL)
    OR
    (mode = 'plaintext' AND token_ref IS NULL
      AND ciphertext IS NULL AND plaintext IS NOT NULL)
  )
);
```

约束目的：三档互斥；`token_ref` 不出现真值；`encrypted` 必须有密文/nonce/key_id；
`plaintext` 只能本地调试。`settings` 存运行时可改的 `delivery_limit`、
`turn_timeout_secs`、`default_on_turn_error`。

### 3.9 判定审计

```sql
CREATE TABLE audit (
  id            INTEGER PRIMARY KEY,
  at            INTEGER NOT NULL,
  request_id    TEXT,
  actor_id      INTEGER REFERENCES principals(id),
  subject_id    INTEGER REFERENCES principals(id),
  role_id       TEXT REFERENCES roles(id),
  assignment_id INTEGER REFERENCES assignments(id),
  override_id   INTEGER REFERENCES overrides(id),
  relation_id   INTEGER REFERENCES relations(id),
  resource_kind TEXT NOT NULL CHECK (resource_kind IN ('room','account','tool')),
  resource_id   TEXT NOT NULL,
  capability    TEXT NOT NULL REFERENCES capabilities(name),
  decision      TEXT NOT NULL CHECK (decision IN ('allow','deny')),
  floor_json    TEXT NOT NULL CHECK (json_valid(floor_json)),
  policy_provider TEXT NOT NULL DEFAULT 'core',
  rule_id       TEXT,
  policy_version INTEGER NOT NULL,
  resource_version INTEGER,
  reason        TEXT NOT NULL,
  detail_json   TEXT CHECK (detail_json IS NULL OR json_valid(detail_json))
);
CREATE INDEX audit_at ON audit(at);
CREATE INDEX audit_subject ON audit(subject_id, at);
CREATE INDEX audit_request ON audit(request_id);
CREATE INDEX audit_resource ON audit(resource_kind, resource_id, at);

CREATE TABLE config_audit (
  id            INTEGER PRIMARY KEY,
  at            INTEGER NOT NULL,
  actor_id      INTEGER REFERENCES principals(id),
  action        TEXT NOT NULL,          -- plugin_config_set / plugin_config_unset / ...
  plugin_id     TEXT,
  key           TEXT,
  old_value_hash TEXT,
  new_value_hash TEXT,
  old_value_json TEXT,                   -- 默认 NULL；脱敏时只留 hash
  new_value_json TEXT,                   -- 默认 NULL
  redacted      INTEGER NOT NULL DEFAULT 1 CHECK (redacted IN (0,1)),
  reason        TEXT NOT NULL,
  request_id    TEXT,
  policy_version INTEGER NOT NULL
);
CREATE INDEX config_audit_at ON config_audit(at);
CREATE INDEX config_audit_plugin ON config_audit(plugin_id, key, at);
```

审计行至少能写出：主体、角色、override、relation、floor、resource、capability、
request id、policy version、决定、理由。主体和资源同名时用两个字段分开，不合并。

`config_audit` 专门记配置写入：`key`、旧/新值 hash、`redacted` 标志、actor、reason、
request_id。默认 `redacted=1`，只存 hash 不存原值；只有显式 `--show-values` 且值通过
密钥扫描时才允许 `redacted=0`，且仍不得写入任何含密钥的值。

### 3.10 缓存版本与 preset 清单

```sql
CREATE TABLE resource_versions (
  resource_kind TEXT NOT NULL,
  resource_id   TEXT NOT NULL,
  version       INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL,
  PRIMARY KEY (resource_kind, resource_id)
);

CREATE TABLE preset_manifests (
  session_id  TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT,
  generation  INTEGER NOT NULL,
  content_hash TEXT NOT NULL,
  schema_version INTEGER NOT NULL,
  previous_generation INTEGER,
  previous_hash TEXT,
  preset_dir  TEXT NOT NULL,
  manifest_json TEXT NOT NULL CHECK (json_valid(manifest_json)),
  status      TEXT NOT NULL CHECK (status IN ('active','degraded','pending_delete')),
  degraded_reason TEXT,
  updated_at  INTEGER NOT NULL
);
```

- `resource_versions` 供缓存按资源失效；`meta.policy_version` 供全局失效。
- `preset_manifests` 供 reconciler 管理自有文件、删除、degraded 状态。

### 3.11 `schema_version` 迁移规则

- 先补表/列，再 bump `schema_version`；中途失败下次可继续。
- 旧程序遇到更高版本直接拒绝；新程序读旧库按版本补。
- 每次迁移前 `acbctl export` + `sqlite3 .backup`；失败用备份恢复。
- 不用 `PRAGMA user_version` 替代 `schema_version`；两者可同步，但以表为准。

### 3.12 同一逻辑实体只有一个源

先更正一个说法：之前“表被分成若干份”指的不是“分库 vs 分表”，而是**同一逻辑实体被
拆到多个配置文件里**——同一行数据多处各存一份，靠加载顺序/覆盖语义对账。

硬约束：

> **同一逻辑实体的数据只允许出现在一个源里；不得因文件分片、Nix 声明、控制台保存
> 而出现第二份。**

当前实际违例：

| 实体 | 现在散落在哪 | 对账方式 | 后果 |
|---|---|---|---|
| grants 规则 | `config.toml` 的 `[grants.rules]` + `config.d/70`（曾计划再加 `71`） | 数组追加 + 后匹配覆盖 | 同一房间/主体规则可能多份；`rule:6` 说不清来源 |
| sessions | `config.toml` 的 `[[sessions]]` + Nix `services.acb.sessions` + `modules.dsh.acbSessions` | 三处手工对齐 id/preset/port | 漂移后 driver 无 preset、preset 无 driver、token 查不到 |
| provider 注册 | `config.d/80` 内联 Nix store 路径 | 手写路径 + 重建后失效 | 重建/GC 后 command 不可执行 |
| console | `90-console.toml` 按顶层 key 整体替换 | 文件名/加载顺序 | 覆盖他处写入的 grants/sessions 等 |

进库解决的是：**一个实体一张表、行只有一份，唯一约束与 FK 承担一致性**，不再依赖
加载顺序或“谁最后覆盖谁”。

注意区分：**按职责分表 ≠ 同一行散落多处**。`plugin_<id>_*` 是不同实体的表，不是同一
行在多个文件里各存一份。

#### 插件数据归属（补充）

插件数据归属仍是一份 `policy.db`、按职责分表：

- **分表**：同一个 `policy.db` 里，按职责/插件前缀开表；
- **分库**：另开一个 SQLite 文件，拥有独立的 WAL、schema、备份、加密密钥和迁移链。

默认是**分表**，不是“每个插件一份库”。

| 档 | 库 | 适用 | 规则 |
|---|---|---|---|
| A | 既有运行时 `acb.db`（不是新拆出来的库） | core 的 records/watermarks/acks/deliveries/notes/feedback/session_maps | core 独占；插件不碰 |
| B（默认） | 同一份 `policy.db` | 插件运行期数据/参数/迁移记录 | 表名 `plugin_<id>_*`；`plugin_migrations` 按 plugin_id 分段；文件/连接/WAL/备份/加密由 core 持有；插件只拿“只能碰自己前缀”的受限句柄 |
| C（例外） | 独立进程自己的库，如 issue 服务的 `issues.db` | 仅当插件是独立进程、有自身信任边界与自身备份（如 issue 服务） | 不作为默认选项；core 不打开该库；不做跨库事务；审计用 `request_id` 双向追溯；schema/版本各自管理 |

```sql
CREATE TABLE plugin_migrations (
  plugin_id  TEXT NOT NULL REFERENCES plugin_registry(id) ON DELETE CASCADE,
  version    INTEGER NOT NULL,
  checksum   TEXT NOT NULL,
  applied_at INTEGER NOT NULL,
  PRIMARY KEY (plugin_id, version)
);
```

硬规则：

- **只有 core 开 SQLite 句柄**；进程内 `.so` 插件不得自行打开库文件，避免同文件多连接
  导致 schema 归属不清、备份/加密/WAL 分叉。
- B 档插件通过 core 提供的受限句柄访问：句柄绑定 `plugin_id`，只允许
  `plugin_<id>_*` 表和 `plugin_migrations` 自己的行；不得 `ATTACH`、不得读其他前缀、
  不得执行 DDL 之外的跨域操作。
- C 档必须是独立进程且已有自身备份/信任边界；它不允许把数据写回 `policy.db`，
  也不允许 core 直接读它的表；需要跨边界时走只读 API + `request_id` 审计。
- **不允许新增第二份配置文件或第二个策略库文件；能在一份库里分表就分表。**
- 插件表前缀必须由 core 生成（sanitize `plugin_id`），插件不能自定义任意表名。

### 3.13 宿主存储接口（StorageHandle v1）

`storage.kind = "builtin"` 的语义是**插件使用宿主提供的存储接口**，不得自行链接
SQLite、打开文件或持有裸连接。

接口方法面：

```rust
trait StorageHandle {
    fn exec(&self, sql: &str, params: &[Value], opts: ExecOptions) -> Result<u64, StorageError>;
    fn query(&self, sql: &str, params: &[Value], opts: QueryOptions) -> Result<Rows, StorageError>;
    fn tx<T>(&self, f: impl FnOnce(&Tx) -> Result<T, StorageError>) -> Result<T, StorageError>;
    fn migrate(&self, migrations: &[Migration]) -> Result<u32, StorageError>;
}
```

要求：

1. **前缀强制**：语句中的表名必须是 `plugin_<id>_*`，由宿主校验并拒绝，不依赖插件
   自觉。实现用 SQLite authorizer 回调，不用正则猜 SQL。
2. 封禁 `ATTACH`/`DETACH`/`PRAGMA`/`VACUUM`/写 `sqlite_master`（以及任何系统表）。
3. 句柄为**借用语义**，插件卸载/禁用即失效；不暴露裸 `Connection` 或文件路径。
4. 单语句超时、行数上限、`SQLITE_BUSY` 由宿主重试；超时/超限/重试耗尽返回
   `StorageError`，插件按失败隔离处理。
5. `tx` 是宿主事务内的**savepoint**；core 持有外层事务，插件不能提交/回滚整库。
6. `migrate` 只允许声明式迁移脚本，走 §3.12 的单事务、失败回滚、版本检查；普通
   `exec` 不允许 DDL。
7. 写入节奏：按 `plugin_storage.write_rows_per_sec/write_burst_rows` 做令牌桶限流；
   超限返回 `StorageError::Quota`，插件降级/重试，不拖慢整库热路径。

形态：

- 进程内 Rust 插件直接用宿主 crate trait，无 ABI 开销；
- `.so` 与进程外插件走同一方法面的 C ABI / RPC；语义一致，不因形态改变鉴权/限额；
- `v1` 兼容策略：**只加不改签名**；破坏性变更必须升 major，插件声明
  `storage_api_version`，低版本对高版本拒绝加载；
- 行数上限/超时/限流值来自 `plugin_storage`，插件不能自行放宽。

#### 插件 schema 契约：schema 归插件，文件与连接归 core

- 插件自带**声明式 schema**：版本号 + 有序迁移脚本；表/索引/FK 名称必须在
  `plugin_<id>_` 前缀内，FK 只能指向同前缀表。
- core 代执行迁移：单事务、失败回滚；已应用版本记入按插件分段的
  `plugin_migrations`；插件版本低于库版本（回退）时**拒绝加载**，不得强行跑。
- 插件禁止：自开库文件、改 PRAGMA/WAL、跨命名空间 FK、运行时未声明的
  `CREATE TABLE`、访问他人表。
- 失败隔离：迁移失败 -> 插件置 `disabled` 并上报，core 继续运行，其他插件不受影响。
- 卸载策略：默认**保留表并标记孤儿**，按保留期或显式清理再删；理由：删表不可逆，
  先保留数据/备份，再决定清理。
- 插件注册表用软删除（`uninstalled_at`/`orphaned_at`），避免级联删掉迁移记录；
  `plugin_migrations` 与孤儿表保留到显式清理。

---

## 4. 判定算法与审计

### 4.1 输入

```rust
struct Query {
  subject: PrincipalId,
  place: Place,              // private / group / console
  resource: Resource,        // room:<id> / account:<platform>:<id> / tool:<id>
  capability: Capability,
  request_id: Option<String>,
  session: Option<SessionId>,
}
```

### 4.2 公式

```text
trust_cap = trust_cap(resource, capability)
floor     = subscription_floor ∪ admin_floor ∪ plugin_floor
grants    = ⋃ matching assignments (capability_snapshot ∩ trust_cap)
deny      = ⋃ matching overrides (effect = deny)
effective = (floor ∪ grants) − deny
```

- `floor` 始终参与；不是“没有匹配才回退”。
- 一条 assignment 匹配 subject/place/scope，但快照里没有该 capability：什么都不做，
  既不 allow 也不 deny。
- 多条 assignment 做并集；不是第一条赢。
- override 只在并集之后减；顺序无关。
- `trust_cap` 是上限，不是 floor：角色不能绕过 room trust。

### 4.3 floor 的具体来源

| 场景 | floor |
|---|---|
| 房间读/写/历史 | 当前会话的 subscription reachability |
| 管理员私信/控制台 | `admin_private` 基线（system_admin/bot_admin） |
| 普通主体 plugin/tool | `base:plugin`（沿用上游 scope 检查） |
| 管理员 plugin/tool | floor 为空，必须显式 assignment |
| account_read / account_message | 账号资源的 subscription/连接可达；否则拒绝 |
| access_request | 管理员私信/控制台基线；其余订阅可达 |
| destructive/critical/account_secret | floor 不含；票据或显式高风险授权 |

`trust_cap`：

- `untrusted`：允许 `room_read/room_write/room_history/connection_read/account_read/
  access_request` 和本地 `tool_call`；不允许 `account_admin`、外部 `tool_call`、
  `destructive`、`critical`、`account_secret`。
- `trusted`：允许除 `account_secret`（票）以外的角色能力；`destructive/critical`
  仍受风险/票门槛。
- `account_secret` 永远不进入 role/assignment 的常驻集合。

### 4.4 匹配与 scope

```text
scope hierarchy:
  room:<id> ⊆ group ⊆ global
  private 是独立分支；room:<private-id> 最具体
```

- assignment 的 `scope_kind/scope_id` 必须覆盖 query 的 place/resource。
- `global` 覆盖所有；`group` 覆盖所有群；`private` 覆盖私信/控制台；`room:<id>`
  只覆盖该房间。
- `room` 级 assignment 不能覆盖别的房间；`group` 级不能覆盖私信。
- `overrides` 的 resource 必须精确匹配；不做“父级 override 自动下传”。

### 4.5 缓存

- 进程内 `PolicySnapshot`：`policy_version` + 所有表的内存索引。
- 缓存键：`(policy_version, subject_id, resource_kind, resource_id, capability, place)`。
- 写入：`policy_version++`，bump `resource_versions(kind,id)`；bridge 轮询
  `meta.policy_version`（1s）或 `PRAGMA data_version`，变化时重建 snapshot。
- 不在每条消息查库；不在判定路径做写事务。
- 缓存失效必须覆盖：assignment、override、relation、room、role_caps、principal、
  plugin_policy、session_rooms。
- `plugin_config` **不参与授权判定**，用独立的 `resource_versions('plugin', plugin_id)`
  粒度，不 bump 全局 `policy_version`；bridge 只重载该插件。若 `plugin_config` 改了
  工具面/能力（例如启用某个 tool），才额外 bump `policy_version` 并重建判定快照。
- 多进程：console 通过 bridge 写；`acbctl` 直接写；两者都 bump 对应版本；bridge 看到
  版本变化后重建，不依赖进程间通知。

### 4.6 错误码

| 错误 | 触发 | 行为 |
|---|---|---|
| `policy_unavailable` | 读库失败/版本不可读 | fail closed |
| `unknown_capability` | 写入/加载遇到枚举外能力 | 写入拒绝，启动失败 |
| `scope_mismatch` | assignment/override scope 不覆盖 query | 不匹配，不拒绝 |
| `grant_escalation` | 候选能力超出授予者 effective | 写入拒绝 |
| `grant_scope_violation` | 候选 scope 超出授予者 scope | 写入拒绝 |
| `delegation_depth` | 链深 > 2 | 拒绝，不截断 |
| `grant_cycle` | parent_grant 成环 | 拒绝 |
| `secret_decrypt_failed` | 加密库/列解不开 | fail closed，提示 key/mode |
| `preset_schema_mismatch` | preset_schema_version 与 dsh 不符 | 拒绝渲染，degraded |
| `conflict` | 乐观锁 version 冲突 | 返回当前版本，要求重试 |
| `drift` | 文件 seed/DB 不一致 | DB 为准，日志/doctor 报 |
| `plugin_config_unknown_key` | key 不在 schema 且 `allow_unknown=0` | 写入拒绝 |
| `plugin_config_type_mismatch` | 类型/范围/枚举不符 | 写入拒绝 |
| `plugin_config_secret_detected` | key/value 像密钥 | 写入拒绝，提示走 `plugin_secret_refs` |

### 4.7 审计格式

数据库行见 §3.9。人读日志：

```text
[access] at=... request=... actor=principal:qq:<id> subject=principal:qq:<id>
  role=group-plugin assignment=42 override=- relation=approver:77
  resource=room:qq:group/<id> capability=room_read decision=allow
  floor=base:subscription grants=role:group-plugin deny=- policy_version=...
  reason="..."
```

- 主体与资源同名：`subject=principal:qq:<id>` 与 `resource=account:qq:<id>` 必须分开。
- 只写 floor allow 的审计可以不写（现有语义）；有 assignment/override/deny 必须写。
- `request_id` 用于票据/委托审批链路；`grant_manage` 写入必须带 actor、target、
  capability set、scope、parent、reason。

---

## 5. 委托语义

### 5.1 `grant_manage`

新增能力 `grant_manage`：允许写授权表。能授出什么由授予者在同一 scope 的 effective
约束：

```text
candidate_caps ⊆ effective(grantor, scope)
candidate_scope ⊆ grantor_scope
```

`grant_manage` 本身也受此约束；没有它不能写授权。

### 5.2 能力快照

assignment 存 `capability_snapshot` + `role_snapshot`，不只存 role_id。

- 角色日后扩容，旧 assignment 仍用旧快照，不会追认超额。
- 角色变更时旧 assignment `needs_review=1`，`grants tree` 提示复审；不静默扩展、
  不自动回收。
- 新 assignment 用新快照。

### 5.3 行与撤销

```text
id, subject, role_id, scope_kind, scope_id,
capability_snapshot, role_snapshot,
can_delegate, granted_by, granted_at, expires_at,
parent_grant, depth, reason, status, needs_review,
revoked_at, revoked_by, revocation_reason, version
```

撤销父授权 -> 递归标记所有后代 `revoked_at`；拒绝环；到期按 `expires_at` 过滤；
不物理删除，保留审计。

### 5.4 继续委托

- 只有 `can_delegate=1` 的 `grant_manage` 才能把 `grant_manage` 再授出。
- 普通 `grant_manage` 可以授普通能力，但不能造新的委托者。
- 链深默认 2；超过拒绝，不截断。
- 禁止自授、环、跨 scope 放大。

### 5.5 高危能力

`account_admin`、对外 `tool_call`、`account_secret`、`destructive`/`critical` 不经普通
委托；走 `approval_route`：

- `acbctl grant` 对高危能力创建 pending grant request。
- 复用现有票据审批：`approval_route` + `approval_allowed`（trust/members）+ approver。
- 批准后写 assignment，`granted_by=approver`，`request_id` 进审计。
- 无路由/不可达 -> pending，不落授权；不另造审批。

### 5.6 接口

```text
acbctl grant <principal> <role|capability> --scope <place>
  [--can-delegate] [--expires <ts>] [--reason <text>]
acbctl revoke <assignment-id>
acbctl grants tree [--principal <p>] [--scope <s>]
```

每次写入 `audit`：actor、目标、能力集、scope、parent_grant、reason、request_id、
结果。高危 `grant` 返回 pending + 路由。

### 5.7 边界

- 不做通用权限市场。
- 不做无限转授。
- 不做跨平台主体隐式映射；`principals(platform, external_id)` 是复合身份。

---

## 6. 会话与插件进库

### 6.1 会话

- `sessions`：id/harness/persona/model/enabled/driver_port/preset/preset_schema_version/
  `secret_id`/memories/`source_seed`/version。
- `session_rooms`：room_id + wake_json + clock_json；替代 `[[sessions.subscriptions]]`。
- `session_tools`：显式工具 allowlist；当前工具面是计算出来的，此表是新增策略；
  空表 = 默认工具面。
- token 只存 `secrets` 引用；`mode=token_ref` 时只出现 `env:NAME`。
- 写入校验：harness/persona 必须存在；`driver_port` 唯一；`preset` 不能为空（dsh）；
  `token_ref` 指向的 env 必须存在；`preset_schema_version` 必须是已知值。
- `acb-drivers` 从“Nix 固定列表”改为**读库的通用 supervisor**：每个 enabled dsh
  session 按 `driver_port`/`preset`/`token_ref` spawn 一个 driver；禁用/删除先停
  driver；opencode session 不走这个 supervisor（它们由 `acb-opencode-serve` 共享实例
  处理，`driver_port`/`preset` 为空）。
- Nix `services.acb.sessions` 只做空库 seed；`modules.dsh.acbSessions` 只生成初始
  preset seed；运行期由 reconciler 接管。

### 6.2 插件

- `plugin_registry`：id/command/args/timeout/command_kind/fingerprint/config_schema_json/
  source_seed。
- `plugin_policy`：enabled/scope_kind/scope_id/capabilities_json/rate_limit_json/
  lifecycle/external/allow_unknown。
- `plugin_config`：plugin_id/key/value_json/type/updated_at；一 key 一行，数组/对象存一格
  JSON，不拆行。
- `plugin_secret_refs`：plugin_id/name/secret_id。
- 注册与启用分离：`plugin_registry` 是“有这个插件、怎么启动”；`plugin_policy` 是
  “现在是否启用、在哪个 scope、允许什么能力、限流多少”；`plugin_config` 是插件自己
  引入的**非部署类运行期参数**。
- 部署项不进 `plugin_config`：可执行路径/args/fixed port/unit 参数仍在
  `plugin_registry` 或 Nix；`plugin_config` 只放插件业务参数（如 provider 选择、阈值、
  feature flag、模型名等）。
- `command` 进库 = 放弃 Nix 对 command 的复现。必须写清：Nix store 路径会随重建/GC
  失效；`doctor`/`reload` 检查可执行；失败报 `plugin_command_unavailable`。
- provider 生命周期：
  - 现状：`ProcessToolLoader` 在启动时 `ProcessToolProvider::spawn`，每个 provider 一个
    常驻子进程 + supervisor 重启。
  - v1：`lifecycle=resident`；`on_demand` 预留，暂不实现。
  - 禁用：不挂载 provider，不发 `plugin_tool_call`；已运行调用等待完成再停。
  - 启用/重启失败：保留上一版本 policy，provider 标记 degraded，会话可用但该工具不可用。
- drift 检测点：
  - bridge 启动；
  - `acbctl reload`/`reconcile`；
  - `acbctl doctor`；
  - provider 子进程重启；
  - `export/import`。
  - 文件 seed 有、库 disabled：DB 为准，日志 `plugin_drift: registered_in_seed_but_disabled`；
    库有、command 不存在：启动失败/降级；库有、文件 seed 没有：正常（库是源）；
    command fingerprint 变化：`plugin_command_changed`，需显式 reload/approve。

### 6.3 会话与插件写入校验

写入时拒绝，不留到运行时：

- 会话引用不存在的 persona/harness；
- `driver_port` 重复或越界；
- `preset` 为空但 harness=dsh；
- `token_ref` 指向的 env 不存在；
- 插件 command 不可执行/不存在；
- 能力名不在 `capabilities`；
- `scope_kind` 不在枚举；
- `rate_limit_json` 非 JSON/负值；
- `timeout_ms` 不在 100..60000；
- `plugin_config` 的 key 不在插件 `config_schema_json`，且 `allow_unknown=0`；
- `plugin_config` 的 `type` 与 schema 不符，或 `minimum/maximum/enum` 不满足；
- `plugin_config` 的 key 形似密钥（`token/secret/password/passwd/api_key/apikey/
  credential/cookie`）或 value 是高熵长字符串；
- `preset_schema_version` 未知。

错误必须带字段、行 id、当前版本；写入失败不 bump `policy_version`。

`plugin_config` 的密钥规则：

- key 命中敏感词 -> 硬失败，提示改用 `plugin_secret_refs`；
- value 是长字符串（≥20 字符、无空格、字符分布高熵，或匹配 `sk-`/`ghp_`/`AKIA`/
  32+ hex）-> 硬失败；不靠“看起来像”放行；
- `allow_unknown=1` 只放宽未知 key，不放宽密钥扫描或类型校验；
- 数组/对象整体是一格 JSON；扫描递归检查其中所有字符串。

### 6.4 权限决策做成插件

原则：**决策可插件化，执行收口必须在 core。**

#### A. core 保留的不变量

| 不变量 | 违反后的后果 |
|---|---|
| 1. 工具调用只有单一收口点；策略插件不得提供第二条执行路径 | 出现绕过 `check_access`/ticket/审计的调用；同一动作两套判据，越权和漏审 |
| 2. fail-closed：超时、报错、插件 panic/崩溃一律 deny | 故障时放行 = 权限提升；插件崩溃变成“全站允许” |
| 3. ticket 不可伪造、一次性、绑定 session+turn+room+capability | 可重放、跨房间/跨轮次使用；票据变成长期通行证 |
| 4. 审计先落盘再执行；判定结果不改变这一顺序 | 拒绝/执行后审计丢失；事故无法复盘，审计可被绕过 |
| 5. 能力枚举固定且由 core 校验；插件不能新增能力值扩权 | 新能力绕过 `GrantCapability` 校验、风险映射和票据门槛 |
| 6. break-glass 由 core 持有，不经插件 | 插件可伪造/关闭紧急通道，或把 break-glass 变成常规授权 |

这些不变量属于 core 的“百分百正确”部分，不随策略实现变化。

#### B. 搬到插件侧的

- 规则匹配（subject/place/scope/resource/capability）；
- 角色与能力集合（roles/role_caps/assignments 快照）；
- 房间 trust 与关系推导（owner/member/approver 如何影响 floor）；
- overrides（只减）；
- 转授深度与级联撤销；
- `explain` 的“为什么”：命中哪条 rule/assignment/override/relation。

core 只消费 `Decision`，不重新解释规则。

#### C. 接口

```rust
pub struct PolicyRequest {
    pub subject: PrincipalId,
    pub place: Place,
    pub resource: Resource,
    pub capability: GrantCapability,
    pub session: Option<SessionId>,
    pub room: Option<RoomId>,
    pub request_id: Option<String>,
    pub policy_version: i64,
    pub resource_version: i64,
}

pub enum Effect { Allow, Deny }

pub struct Decision {
    pub effect: Effect,
    pub rule_id: String,        // 必填且可追溯
    pub reason: String,
    pub policy_version: i64,
}

pub enum PolicyError {
    Unavailable,
    Timeout,
    InvalidDecision,
    VersionMismatch,
    Panic,
}

pub trait Policy {
    fn decide(&self, req: &PolicyRequest) -> Result<Decision, PolicyError>;
}
```

- 默认实现 `CorePolicy` 由 core 内置，就是今天这套 RBAC；
- 插件 adapter 只在 `Policy` 之后，不直接碰执行路径；
- P1 不得把判定写死在 kernel 业务代码里：`check_access`、`platform_call`、
  `plugin_tool_call`、`access_request` 全部经 `Policy::decide`，这样后续整体替换成
  插件时调用方不动；
- core 校验 `rule_id` 非空、`policy_version` 与请求一致、`effect` 合法；不合法直接
  deny（`invalid_policy_decision`）；
- `rule_id` 必须能定位到具体 assignment/override/relation/floor；禁止再出现只写
  `rule:6` 却说不清来源的审计。

#### D. 形态

- **in-process trait 先行**：热路径逐消息判定，进程外多一跳延迟；
- 将来挪出进程必须满足：请求/响应可序列化、无借用状态、协议版本化、超时/取消、
  只读快照、审计字段由 core 定；满足后再评估 IPC 延迟，不能为了“插件化”先加一跳；
- 进程外插件仍然只能返回 `Decision`，不能执行工具/发消息/改票据。

#### E. 风险与挡法

- `rule_id` 必填、唯一、可追溯到 assignment/override/relation/floor；
- 策略插件只能读主体/房间/关系/策略快照并返回结论；不能调用平台、不能写流水、
  不能发消息、不能改票据；
- `explain` 由插件实现，但**输出字段格式由 core 定**；插件只填 `rule_id`/`reason`/
  `sources` 等值，不能自造审计格式；
- 审计必须记录 `policy_provider`、`rule_id`、`policy_version`、`resource_version`、
  `request_id`；插件崩溃也要写一条 deny 审计（`policy_provider_error`）。

#### F. 两个判断

**策略插件是否走 `plugin_registry`？**

建议走，但**单独可信等级**：

- `plugin_registry.kind='policy'`、`trust_tier='policy'`，不是普通 tool provider；
- 必须显式 allowlist：`policy_binding` 行（`allowlisted_by/at`），不能通过普通插件
  市场/随手 `plugin add` 接管全量判定；
- 加载时校验 command fingerprint/签名；只读策略快照，不给 secrets/网络/执行动作；
- 默认 `policy.provider=core`；没有 active `policy_binding` 时用 `CorePolicy`；
- `policy_binding.fallback` 默认 `deny`，`core` 是显式选项并审计。

**判定结果能否缓存？**

可以，但按版本：

- 缓存键 `(policy_version, resource_version, subject, resource, capability, place)`；
- 只缓存同一版本的 `Decision`；`policy_version`/`resource_version` 变化即失效；
- 插件超时/报错/panic：**默认 deny**；只有 `fallback=core` 时才回退到内置实现，且
  写 `policy_provider_fallback` 审计；
- 插件崩溃后不服务旧缓存（可能过期/越权），先清该 provider 的缓存，再按 fallback
  策略处理；
- 缓存 TTL 只是上限，不能跨版本复用。

P1 调整：P1 的验收仍是“9 群行为不变 + matrix 差异”，但实现上必须先有 `Policy` trait
和 `CorePolicy`，`explain/matrix/diff` 走 trait；不允许在 engine 里直接调
`evaluate_rules` 作为唯一路径。

### 6.5 clean core：插件边界

判据：**core 里出现业务语义词/业务分支（房间运营、群、issue、通知、具体产品名、
harness 名）即视为渗漏；只有「不变量 + 流水」允许留在 core。**

协议类型不算渗漏：`RoomId`、`Place::Group`、`Record` 这类是 core 的通用结构；渗漏的是
“知道 issue 是什么、知道 ntfy 怎么发、知道 QQ 群/DSH/opencode 的产品分支”。

#### core 保留清单

- 工具调用单一收口点；
- ticket 签发/校验（不可伪造、一次性、绑定 session+turn+room+capability）；
- 能力枚举与风险映射；
- 审计格式与先落盘后执行；
- 插件生命周期、加载、失败隔离；
- fail-closed；
- break-glass；
- DB 句柄、备份、加密、迁移执行；
- 协议版本/接入握手；
- 流水（records/watermarks/acks/deliveries）与队列。

#### 迁插件清单

- 各 harness 适配器（opencode/dsh 驱动、MCP server）；
- tool providers（含 `issue.tools`）；
- 权限判定实现（`Policy` trait 的插件实现；`CorePolicy` 只作默认）；
- preset 渲染与 reconcile；
- 通知派发（ntfy/进程通知）；
- 其余业务策略（notes/feedback/tasks/memory/prompt 业务规则、Lua 路由、console 业务）。

#### core 残留清单（按实际目录扫描）

扫描证据：`crates/kernel/src/engine.rs` 有 748 行业务词匹配（room/group/issue/
notification/qq/opencode 等），`config.rs` 224，`config_patch.rs` 54，`console.rs` 25。
这不是要求把协议类型删掉，而是说明 core 里有业务分支需要拆。

| 模块/目录 | 当前职责 | 建议处置 | 理由 | 迁移前置 |
|---|---|---|---|---|
| `crates/kernel/src/engine.rs` | 轮次调度、工具分发、访问检查、ticket、notes/feedback/tasks/memory/prompt、platform call | **拆分**：choke point/ticket/audit 留 core；notes/feedback/tasks/memory/prompt 业务迁插件；platform call 执行留 core | 业务语义集中在 engine；不拆会继续把产品策略写进 core | `Policy` trait、插件协议、ticket 收口测试 |
| `crates/kernel/src/grants.rs` | RBAC 求值、target/scope、subject | **迁 Policy 实现**：默认 `CorePolicy` 可留作内置实现，求值逻辑不在 engine 内联 | 判定可插件化；core 只保留接口/校验 | P1 trait、matrix 零差异 |
| `crates/kernel/src/config.rs` | 配置加载/校验、sessions/grants/rooms/plugins | **拆**：加载/bootstrap 留 core；策略数据/会话/插件注册迁 policy store | 运行时可变数据不该在 core 配置结构里 | P2 schema、P3 import |
| `crates/kernel/src/plugin.rs` | 插件 manager、provider 注册、扩展点 | 留 core（生命周期/失败隔离） | 插件宿主是不变量 | 无 |
| `crates/kernel/src/tickets.rs` | 票据状态、nonce、审批元数据 | 留 core | ticket 不可伪造/一次性是安全不变量 | 无 |
| `crates/kernel/src/storage/*` | `acb.db` 运行态、schema/WAL | 留 core（A 档） | 流水必须 core 独占 | 无 |
| `crates/kernel/src/endpoint.rs` | harness 握手/连接 | 留 core | 协议/身份边界 | 无 |
| `crates/kernel/src/wire.rs` | 协议帧 | 留 core | 协议版本 | 无 |
| `crates/kernel/src/adapter.rs` | 适配器 trait/描述 | 留 core | 插件契约 | 无 |
| `crates/kernel/src/outbound.rs` | platform method/risk/target | **拆**：MethodSpec 结构留 core；具体平台目录/risk 表迁 adapter 插件 | core 不应认识 QQ 方法表 | adapter 插件契约 |
| `crates/kernel/src/deps.rs` | 依赖状态 | 留 core（可观测） | 基础设施 | 无 |
| `crates/kernel/src/event_queue.rs` | 入站/路由队列 | 留 core | 流水/背压 | 无 |
| `crates/kernel/src/memory.rs` | memory 工具/仓库 | 迁插件 | 业务功能 | memory provider 插件 |
| `crates/kernel/src/relay.rs` | 文件中继 | 迁插件/独立服务 | 产品服务，不是 core 不变量 | relay 插件 + 配置 |
| `crates/kernel/src/route_worker.rs` | Lua 路由 | 迁插件 | 业务路由 | route provider 插件 |
| `crates/kernel/src/render.rs` | 提示词渲染 | 待定 | 可能是通用流水，也可能含人格业务 | prompt 插件契约后定 |
| `crates/bridge/src/main.rs` | 组装/启动 | 留 core | ACB 组装 | 无 |
| `crates/bridge/src/console.rs` | 控制台 HTTP/UI、配置保存 | 迁插件/独立 console | 产品 UI/业务编辑 | console 插件 + policy API |
| `crates/bridge/src/config_patch.rs` | sessions/connections 的 TOML patch | 迁 console/config 插件 | 业务配置编辑 | policy store API |
| `crates/adapters/*` | OneBot/process/dynamic 适配器 | 迁插件 | 平台适配器不是 core | adapter 插件契约 |
| `crates/plugins/*` | process-tools/Lua/tool providers | 已是插件 | tool provider 边界 | 无 |
| `crates/issue-service` | issue 服务 + issues.db | 留独立服务（C 档） | 独立进程/自身备份/信任边界 | 无 |
| `crates/notifier-process` | ntfy/进程通知 | 迁插件 | 通知派发是业务 | notifier 插件 |
| `crates/memory-steward` | memory 同步 | 迁插件/服务 | 业务同步 | memory provider |
| `crates/supervisor-policy` | 监督策略决策表 | 迁插件 | 业务策略 | supervisor 插件契约 |
| `crates/supervisor` | supervisor 进程 | 留 core（ACB 组装） | 启动/回滚/生命周期 | 无 |
| `crates/wasm-host` | wasm 插件宿主 | 留 core | 插件宿主不变量 | 无 |
| `crates/extensions` | 扩展 authoring/boundary | 待定/迁 console | 插件开发接口，非核心判定 | 插件契约 |
| `crates/acb` / `crates/launcher` | acbctl/run 后端 | 留 core（运维组装） | 部署工具 | 无 |
| `crates/proto` / `crates/i18n` | 协议/文案 | 留 core | 协议与公共文案 | 无 |

这张表是“从哪开始拆”的清单，不要求一次拆完；标“待定”的必须先有插件契约再动。

#### 插件存储声明（声明制）

插件清单增加 `[storage]`，默认 `builtin`：

```toml
[storage]
kind = "builtin"     # 默认，落在 policy.db 的 plugin_<id>_* 表
# kind = "file"; name = "xxx.db"; rebuildable = true; retention = "30d"; backup = false
```

规则：

1. **策略类数据只能 `builtin`**：grants、角色、房间、会话定义、插件注册与策略，不得
   迁到插件自有文件——避免同一逻辑实体出现第二个落点。
2. `file` 仅限插件自有领域数据，且须满足至少一条：写入量/体积会污染 `policy.db`
   热路径；或数据可重建、无需与策略数据同事务。
3. 文件由 core 创建、core 持有句柄与生命周期，迁移同样由 core 代执行；插件不得传
   路径、不得开连接、不得改 PRAGMA。
4. `rebuildable = true` 的文件明确排除在备份集之外；`backup = true` 的纳入备份，
   并在恢复流程里排定顺序。
5. 不做跨库事务，core 不 join 插件库；插件失败只影响自身文件。
6. 显式代价：额外备份条目、额外恢复顺序、额外版本对齐（core 升级与插件 schema
   版本）。

字段校验：

- `kind = "file"` 时 `name` 必须在 core 管理的目录白名单内，只允许 basename
  （如 `xxx.db`）；禁止绝对路径、`../`、`/` 或任意路径拼接；core 负责拼出
  `state/plugin-db/<plugin_id>/<name>`。
- `data_class = "policy"` 时 `kind` 必须 `builtin`；`kind = "file"` 只允许
  `data_class = "domain"`。
- `rebuildable = 1` 时 `backup` 必须为 0；`kind = "file"` 且 `rebuildable = 0` 时
  必须填写 `volume_justification`。
- `backup = 1` 时必须有 `backup_order`，恢复时按 order 从小到大；core 表先恢复，
  插件文件后恢复。
- 声明与 `plugin_storage` 表不一致、name 越界、策略数据声明 file -> 注册/写入拒绝。

#### 插件契约

- 加载：经 `plugin_registry` + `plugin_policy`；in-process `.so` / process provider 都
  声明扩展点、API 版本、能力；policy provider 走 `kind/trust_tier=policy` + allowlist。
- 失败隔离：插件 panic/超时/启动失败只影响自己；core 继续；失败插件 disabled/
  degraded；不得让 core 崩。
- 版本兼容：`PLUGIN_API_VERSION` + `preset_schema_version`/`schema_version`；低版本
  对高版本拒绝，不强行跑。
- 能力声明：固定枚举，由 core 校验；插件不能新增能力值。
- 禁止事项：不得自开执行路径、不得自定义能力枚举、不得绕过审计、不得自开 DB、
  不得跨命名空间 FK。
- 数据：默认 B 档分表；C 档仅独立进程；schema 归插件，文件/连接/WAL/备份归 core；
  `builtin` 必须走 `StorageHandle`，不得自行链接 SQLite/打开文件；`.so` 与进程外走
  同一方法面（C ABI/RPC），语义一致。

---

## 7. 密钥三档

### 7.1 模式

| 模式 | 库内内容 | 真值来源 | 用途 |
|---|---|---|---|
| `token_ref`（默认） | `env:NAME` 或 `age:<file>#<name>` | agenix → systemd EnvironmentFile → 进程 env | 生产默认；备份/库文件不含真值 |
| `encrypted-store` | `ciphertext` + `nonce` + `key_id` | systemd `LoadCredential` | 用户显式启用；整库/列级加密 |
| `plaintext` | `plaintext` 列 | 库本身 | 仅本地调试；启动警告；建议拒绝非回环/生产 |

`meta.secrets_mode` + 每个 `secrets` 行的 `mode`。启动日志只打印模式，不打印值。
`plaintext` 需要显式 `ACB_ALLOW_PLAINTEXT_SECRETS=1`，否则启动失败。

需要密钥引用的位置：

| 位置 | 库内表示 | 真值来源 |
|---|---|---|
| session access token | `secrets` 行（`owner_kind=session`，`mode=token_ref`） | agenix → systemd env → bridge/driver |
| plugin secret | `secrets` 行 + `plugin_secret_refs` | agenix → systemd env → provider 子进程 |
| console token | 文件 `${ACB_CONSOLE_TOKEN}`（不进库） | agenix → systemd env |
| ntfy topic | 文件/Nix `${NTFY_ACB_TOPIC}`（不进库） | agenix → systemd env |
| connection token | 文件 `${ACB_SNOWLUMA_TOKEN}`（不进库） | agenix → systemd env |
| model/issue/opencode 密钥 | env（不进库） | agenix / `dsh.env` / `opencode.env` 迁移后 agenix |

### 7.2 加密实现（只用现成方案）

**方案 A：整库 SQLCipher**

- 打开 `policy.db` 时 `PRAGMA key = <credential>`；所有表/索引/备份加密。
- 密钥来自 `systemd LoadCredential=acb-policy-key:/run/agenix/acb-policy-key`，
  进程从 `$CREDENTIALS_DIRECTORY` 读取；不落库、不落配置文件。
- 迁移：`sqlcipher_export` 或 dump/import；`PRAGMA cipher_integrity_check` 验证。
- 优点：统一加密，schema/查询不变；缺点：需要 SQLCipher 构建，密钥丢失即整库不可读。

**方案 B：列级 AEAD（age / AES-GCM）**

- 只加密 `secrets.ciphertext` 等敏感列；非密钥列保持可读。
- 使用现成库：age（X25519/ssh key）或 AES-256-GCM；不自己实现 KDF/协议。
- `nonce` 每行唯一；`aad` 绑定 `owner_kind|owner_id|purpose|key_id`，防行替换。
- 密钥来自 `LoadCredential`；key_id 用于轮换。
- 优点：可精确加密字段、迁移小；缺点：schema/关联元数据仍泄露，需自己做 AAD 规范。

推荐：生产优先 `token_ref`；需要“库文件+备份也不含真值”时选 SQLCipher；SQLCipher
不可用再选列级 AEAD。两者都不自研密码学。

### 7.3 威胁模型

防：

- 拿到 `policy.db` 文件、备份、WAL/SHM 的本地/离线攻击者；
- 拿到 Nix store 里的派生文件/日志（`token_ref` 模式下无真值）；
- 误把库备份上传/同步到别处。

不防：

- 拿到进程内存/`$CREDENTIALS_DIRECTORY` 的同用户/root；
- 拿到解密密钥的人；
- SQL 注入/应用漏洞导致的在线读取；
- 流量侧信道、日志中误打印（需额外 redaction）。

与 agenix 的差别：agenix 保护“密钥文件”本身，密钥由 age 解密后交给 systemd；
policy-store 加密保护“整个库/列 + 备份”。agenix 的轮换是替换 secret 文件 + 重启；
加密库的轮换是 rekey/重加密，成本更高。两者可以叠加：`token_ref` 指向 agenix，
或 `encrypted-store` 的数据密钥由 agenix/LoadCredential 提供。

### 7.4 默认、日志、迁移、轮换

- 默认 `token_ref`；`encrypted-store` 需显式启用；`plaintext` 仅调试。
- 启动日志：`policy secrets mode: token_ref` / `encrypted (key_id=...)` / `plaintext (WARNING)`。
- 迁移：
  - `token_ref` → `encrypted`：新建/重加密库，导入引用；验证 `secret` 行可解密；
    旧库备份保留；失败回滚。
  - `plaintext` → `encrypted`：dump/import → 加密；用 `VACUUM`/安全删除清旧库、
    WAL/SHM、旧备份；验证行数/hash。
  - 解密失败：`secret_decrypt_failed`，fail closed，提示 key_id/mode；不自动回退
    plaintext；break-glass 只用于管理员恢复。
- 轮换：
  - `token_ref`：换 env/agenix 值 + `acbctl reload`；会话 token 轮换要求 harness
    重连（旧连接仍持旧 token，需 `reload --rotate-session` 优雅断开）。
  - `encrypted-store`：SQLCipher `PRAGMA rekey`（整库）或列级重加密；旧 key 保留到
    验证完成；更新 LoadCredential；审计轮换事件。
  - `plaintext`：直接改库 + 审计；不推荐。

### 7.5 元数据泄露

即使加密，以下仍可能泄露：行数、更新时间、owner_kind/owner_id、purpose、scope、
能力名、plugin id、audit 频率、`policy_version`。SQLCipher 隐藏 schema；列级 AEAD
不隐藏 schema/关联。设计上接受这些元数据；不要把“加密”说成“全匿名”。

---

## 8. Preset Reconciler

### 8.1 定位

- **库是唯一源**：sessions 表的 `preset`、`driver_port`、`preset_schema_version`、
  session 的 persona/model 等是期望状态。
- **preset 目录是派生物**：`preset_dir` 下由 reconciler 渲染；Nix 只做初始 seed。
- 解决“运行中新增 dsh 会话没有 Nix preset”的硬约束：不再要求 Nix 生成每个 preset。

### 8.2 触发

- 每个实例启动/重载时调用：
  `acbctl preset render --session <id>`
- 全局兜底：`acbctl reconcile` 扫描所有 enabled sessions，补渲染/清理。
- bridge `reload` 后调用 `reconcile`；driver 启动前调用 `preset render`。
- `acbctl preset show <session>` 打印当前生效内容 + generation/hash/schema。

### 8.3 幂等与原子写

- 先算渲染内容 hash；与 `preset_manifests.content_hash` 相同 -> 不写。
- 原子写：
  - 单文件：写 `preset_dir/.tmp-<gen>`，`rename` 到目标文件；
  - 多文件/多代：写 `preset_dir/generations/<session>/<gen>/`，`rename` 后原子切换
    `preset_dir/<session>/current` 软链。
- 推荐 generation 软链，便于回滚/保留上一代。
- 写入后更新 `preset_manifests`（generation/hash/schema/manifest/status）。
- manifest 列出本会话自有文件、hash、generation；不要用通配删除。

### 8.4 删除

- 只删除 `preset_manifests.manifest_json` 标记为本系统所有的文件/目录。
- 删除会话必须先停实例（driver/dsh session），再删 preset；停失败 -> 拒绝删除，
  会话保持 `pending_delete`/`degraded`。
- 不删用户/其他工具写进 preset 目录的文件；发现非自有文件 -> 报告 drift，不删除。

### 8.5 失败与 degraded

- 渲染/校验/rename 失败：保留上一代 `current`，`preset_manifests.status='degraded'`，
  `sessions.status='degraded'`（新增或复用 status 字段），日志/审计记录原因。
- `preset_schema_version` 与 dsh 版本不匹配：拒绝渲染，直接 degraded；不写半成品。
- `acbctl doctor` 检查 degraded 会话；`reconcile` 可重试。
- 失败不得删除上一代；不得把旧 preset 当新内容。

### 8.6 与 dsh 的衔接

- 渲染内容形状与 `nix/dsh-bundle/preset-row.yml.tmpl` 对齐：
  `id: preset-<preset>`，`@deepseek-ai/dsh-agent-preset`，MCP client
  `url: http://127.0.0.1:<driver_port>/mcp`，compaction group。
- `preset_schema_version` 由 dsh 版本/兼容表决定；不匹配拒绝。
- 渲染目录通过 profile bundle/patch 暴露给 dsh；具体路径由部署配置
  `preset_dir` 决定，但必须是 reconciler 自有 manifest 管理的目录。
- 初次迁移：Nix seed 渲染初始 preset；之后 reconciler 接管。**Nix 生成的 preset
  不能继续和 reconciler 目录同时挂载同一批 id**：seed 导入后需把 Nix preset bundle
  从 `web-acb` profile 移除（或改为空），否则同 id 两行会冲突。迁移验收必须检查
  `dsh --dump-config` 里每个 `preset-<id>` 只出现一次，且都来自 reconciler 目录。

---

## 9. `acbctl` 命令全集与运维

### 9.1 命令

| 命令 | 作用 | 输入 | 输出/退出 |
|---|---|---|---|
| `acbctl seed --from-nix --if-empty` | 空库写入 Nix seed；非空跳过并日志 | seed 文件/路径 | 0 成功/跳过；2 校验失败 |
| `acbctl import --dry-run|--apply <toml>` | 导入旧 TOML | 路径/格式 | dry-run diff；apply 事务 |
| `acbctl export [--format=toml|json]` | 库→可 review 快照（脱敏） | 输出路径 | 快照 + hash |
| `acbctl reload [--rotate-session <id>]` | 优雅重载策略/会话/插件 | 无/会话 id | 0 成功；失败回滚上一版本 |
| `acbctl reconcile [--dry-run]` | 全局 preset 渲染/清理 | 无 | 渲染数/跳过数/degraded |
| `acbctl preset render --session <id>` | 单会话渲染 preset | session id | 0/1，内容 hash |
| `acbctl preset show <session>` | 打印生效 preset 内容 | session id | 内容 + generation/schema |
| `acbctl sessions list|add|update|disable|remove` | 会话 CRUD | JSON/字段 | 行 id/版本 |
| `acbctl plugins list|add|update|disable|remove` | 插件 CRUD | JSON/字段 | 行 id/版本 |
| `acbctl plugins config get|set|unset <plugin> [key]` | 插件自定义运行期参数 | JSON/字段 | 校验结果；旧/新值默认脱敏；bump `plugin:<id>` |
| `acbctl plugins config show <plugin>` | 打印插件生效参数 | 插件 id | 脱敏后的 key/type/value |
| `acbctl grant ...` | 授权/委托 | principal/role/cap/scope | assignment id/pending |
| `acbctl revoke <assignment-id>` | 撤销+级联 | id | 撤销数 |
| `acbctl grants tree` | 授权树/快照/复审 | 过滤 | 树/JSON |
| `acbctl grants explain` | 单条判定解释 | query | floor/grants/deny/effective |
| `acbctl grants matrix` | 全矩阵判定 | 过滤/模式 | JSON 行 |
| `acbctl grants diff` | 旧/新判定差异 | before/after | 差异行 |
| `acbctl doctor` | 校验：schema、command、preset、drift、密钥 | 无 | 诊断/退出码 |
| `acbctl breakglass` | 一次性紧急授权（不写库） | 本机 token | TTL 授权 + 审计 |

> 当前 `grants explain/matrix/diff` 的落地形态是只读 CLI：本期 `policy.db` 尚不存在，
> 源固定为 config（`PolicySource` 的 `ConfigSource` 实现），复用同一套 `CorePolicy`。
> 输出带 `source` 标识，`explain` 带 `policy_version`（静态 CLI 默认 0），因此可明确看出
> 它不是 live bridge 的实时判定。三个命令均不写配置、不写 `policy.db`；上线后默认源切到
> db，config 转为导入输入，命令面不变。写操作一律走 policy API，由 bridge 独占写。

### 9.2 写路径与冲突

- 所有写命令：`BEGIN IMMEDIATE`，校验，写表，bump 对应版本，写审计，提交；失败回滚。
- `plugin_config` 只 bump `resource_versions('plugin', <id>)`，写 `config_audit`；不
  bump 全局 `policy_version`（除非它改变了工具面/能力）。
- 乐观锁：更新/删除带 `version`；冲突返回 `conflict` + 当前版本；不覆盖。
- console 是 bridge 内写者；`acbctl` 是外部写者；两者都走同一校验函数。
- `reload`：先校验新版本，再 stage；会话/插件启动失败回滚到上一 `policy_version`；
  不把半成品提交。
- 读命令：WAL 读快照；不阻塞写。

### 9.3 热重载

- `acbctl reload`：
  - 会话：新增/更新/禁用/删除；活跃轮次用开轮时配置快照，结束后切换；删除会话先停
    driver/dsh；不改的连接保持。
  - 插件：新增/更新/禁用 provider；新 provider 启动成功才提交；删除等 in-flight 完成。
  - 失败：回滚上一版本；日志/审计；会话标记 degraded，不静默。
- 必须重启（不是 reload）的改动：endpoint/console/relay listen、dsh `--port`、
  `policy.db`/`acb.db` 路径、unit/依赖、profile/generation 切换、二进制/脚本路径。
- `reload` 不等于 `reconcile`：前者策略/会话/插件；后者 preset 派生目录。

### 9.4 备份与 break-glass

- `policy.db` 备份：`sqlite3 .backup`/`VACUUM INTO`；不要文件拷贝 WAL 库。
- `acbctl export` 生成脱敏快照，进 git/审查。
- break-glass：本地文件/env 一次性 token，固定 `system_admin` 能力，TTL 5 分钟，
  写独立审计文件，不进 `policy.db`；用完删除；不得成为常规路径。

---

## 10. 数据源搬运与重构（含迁移 map）

### 10.1 搬运顺序与独立回滚

固定顺序，每一步都能单独停/回滚：

1. **sessions**（含 session_rooms、session_tools、token_ref）。
   - 前置：P1 语义已定；P2 `sessions`/`secrets` schema 可用；`acb-drivers`
     通用 supervisor 能读库；Nix sessions 已生成 seed。
   - 动作：导入 sessions/订阅/tool；token 只写 `secrets.token_ref`；切读路径。
   - 验证：会话列表、driver 端口、preset、token 握手、wake/clock。
   - 回滚：恢复 `config.toml` 的 `[[sessions]]`/subscriptions；`policy.store=file`；
     重启 driver/bridge；Nix 仍可 seed。
2. **插件注册与配置**（plugin_registry/plugin_policy/plugin_config/plugin_secret_refs）。
   - 前置：sessions 已稳定；plugin schema/config 校验、command 可执行检查、provider
     lifecycle 可用。
   - 动作：导入 80 片 command/args/timeout；插件自定义参数写 `plugin_config`；
     密钥只写 `plugin_secret_refs`。
   - 验证：provider 工具列表、config schema、单独 reload、drift。
   - 回滚：恢复 80 片与 env；`policy.store=file`；停新 provider、起旧 provider。
3. **grants/rooms/overrides/relations**（授权与策略）。
   - 前置：P1 `Policy` trait + CorePolicy；`grants explain/matrix/diff` 已能零差异；
     70 片已删回 `["plugin"]`。
   - 动作：导入 base rules、70 片、rooms trust/members/approval_route、accounts。
   - 验证：9 群 allow、未订阅收紧、matrix 只含预期差异、审批路由。
   - 回滚：恢复 base/70/`.pre-rooms`；`policy.store=file`；重启 bridge。
4. **settings**（`default_on_turn_error`、`delivery_limit`、`turn_timeout_secs` 等）。
   - 前置：sessions/plugins/grants 已稳定；`settings` 读写与 reload 可用。
   - 动作：导入顶层策略；控制台写库。
   - 验证：失败通知、投递截断、超时行为；reload 后生效。
   - 回滚：恢复 `config.toml` 顶层键；`policy.store=file`；重启。

每一步切换前先 `acbctl export`；切换失败用 export/备份恢复对应表，文件源仍保留到
该步验收通过。不要一次性把所有表切完，否则无法判断是哪一步造成的行为差异。

### 10.2 seed 语义与“Nix 不再覆盖”的证据

- 库为空（`sessions`/`plugin_registry`/`roles` 等目标表无行，或 `meta.seed_version`
  为空）：写入 Nix seed，记录 `seed_version`/`seed_source`，日志
  `policy seed applied: sessions=N plugins=M roles=K`。
- 库非空：**跳过**，日志 `policy seed skipped: db has rows (sessions=N plugins=M)`；
  不覆盖、不合并、不删除。
- 证据：
  1. 启动日志出现 `seed applied` 一次；第二次启动同一库出现 `seed skipped`；
  2. 改 Nix `services.acb.sessions` 后 `nixos-rebuild`，库非空时运行会话不变；
  3. `acbctl export` 的 sessions/plugins 与库一致，不出现 Nix 新值；
  4. `acbctl doctor` 报告 `seed_state=skipped` 和 seed hash。
- `acbctl seed --force` 是显式覆盖，必须带 `--reason` 并写 `config_audit`；默认不提供。

### 10.3 归档判定（`config.d/70/80/90`）

归档必须同时满足：

- 对应数据已导入并在 `acbctl diff`/`matrix` 中零差异；
- 该片的删除/修改不再改变运行行为（删除测试通过）；
- `acbctl export` 能重建等价内容；
- 该片不再被 bridge 加载（`config.d` 迁移后只读）。

归档动作：

- 移到 `config.d/history/<name>.migrated-<timestamp>`，扩展名不再是 `.toml`，加载器
  自然忽略；
- 记录 sha256、导入行数、导入 `policy_version`；
- `config.d` 目录保留一个 `README.migrated` 说明“只读历史，不再加载”；
- `90-console.toml` 归档后控制台写库，文件只作 export/seed。

### 10.4 单一数据源证明

见 §12；本章只固定证据链：

- `export` vs 库逐表逐行；
- 删除任意 `config.d` 归档片行为不变；
- Nix 改 seed 库非空不生效；
- `explain` 每条判定有来源；
- `reconcile` 二次零写入；
- `doctor` 检查无 live fragment、无未审计写入。

### 10.5 反向路径：从库回到文件

不是“删库”，而是导出快照后切回文件源：

1. 停止写入（`acbctl reload --freeze` 或停 bridge）；
2. `acbctl export --to-files <dir>` 生成可加载的 TOML：
   - `config.toml` 的文件域（connections/personas/memories/harnesses/file_relay）；
   - `config.d/70-grants.toml`（assignments/overrides/rooms，脱敏、只含 token_ref）；
   - `config.d/80-plugins.toml`（plugin_registry/policy/config，command 原样）；
   - `config.d/90-sessions.toml`（sessions/session_rooms/session_tools，token_ref）；
   - secrets 只写 `env:NAME` 引用，不写值。
3. 校验导出文件能被 `acbctl import --dry-run` 读回，且 diff 为零；
4. 设置 `policy.store=file`，把导出目录放到 `config.d`，启动 bridge；
5. 验证后保留 `policy.db` 备份；文件源成为新源，DB 不再是唯一源（这是一次显式
   降级，不是自动回滚）。

反向路径的用途：迁移失败、DB 损坏、需要回到 Nix/文件 review 流程。它必须是一步
显式操作，不能由 `seed` 自动触发。

### 10.6 迁移 map

一张表，行是每一项具体配置；`类别` 只是分组。

| 类别 | 配置项 | 现状位置 | 目标位置 | 迁移动作 | 验证 | 回滚 | 阶段 |
|---|---|---|---|---|---|---|---|
| config.toml | `[endpoint]` listen | `config.toml` + env | 文件/Nix | 保持不变 | 启动监听 | git revert | P1-P4 |
| config.toml | `[console]` listen | `config.toml` + env | 文件/Nix | 保持不变 | 控制台可访问 | git revert | P1-P4 |
| config.toml | `[console]` token | `${ACB_CONSOLE_TOKEN}` | env/agenix | 保持不变；不落库 | 401/登录 | rekey | P1-P4 |
| config.toml | `[[connections]]` 拓扑（含 `admin_qqs`） | `config.toml` | 文件 | 保持不变 | 适配器连接；`admin_qqs` 只标 sender_role，不鉴权 | git revert | P1-P4 |
| config.toml | `[[connections]]` token | `${ACB_SNOWLUMA_TOKEN}` | env/agenix | 保持不变 | 登录成功 | rekey | P1-P4 |
| config.toml | `[[connections.routes]]` | `config.toml` | 文件 | 保持不变 | Lua 路由 | git revert | P1-P4 |
| config.toml | `[[personas]]` | `config.toml` | 文件 | 保持不变 | 人格注入 | git revert | P3 |
| config.toml | `[[memories]]` | `config.toml` | 文件 | 保持不变 | memory 工具 | git revert | P3 |
| config.toml | `[[harnesses]]` | `config.toml` | 文件 | 保持不变 | harness 连接 | git revert | P3 |
| config.toml | `[[sessions]]` 身份（含 `name`/`on_turn_error`） | `config.toml` | `sessions` | 导入；token→`secrets`；Nix seed | 会话列表/端口/preset | 恢复文件/export | P3 |
| config.toml | `[[sessions.subscriptions]]` | `config.toml` | `session_rooms` | 导入 | 订阅/唤醒 | 恢复文件 | P3 |
| config.toml | `[sessions.subscriptions.wake]` | `config.toml` | `session_rooms.wake_json` | 导入 | wake 行为 | 恢复文件 | P3 |
| config.toml | `[sessions.subscriptions.clock]` | `config.toml` | `session_rooms.clock_json` | 导入 | clock 轮次 | 恢复文件 | P3 |
| config.toml | `[grants.system_admin]` | `config.toml` | `principals`+`assignments` | 导入角色/指派 | explain | 恢复文件 | P3 |
| config.toml | `[grants.bot_admin]`（若有） | `config.toml` | `principals`+`assignments` | 导入角色/指派 | explain | 恢复文件 | P3 |
| config.toml | `[[grants.rules]]` | `config.toml` | `roles/role_caps/assignments` | 导入；70 临时四能力不回填 | matrix/diff | 恢复文件 | P3 |
| config.toml | `[grants.rooms]` trust/members/approval_route | `config.toml` | `rooms`/`relations`/`approval_routes` | 导入 | 审批路由 | 恢复文件 | P3 |
| config.toml | `[grants.accounts]`（若有） | `config.toml` | `approval_routes(owner_kind=account)` | 导入 | 账号路由 | 恢复文件 | P3 |
| config.toml | `file_relay` | `config.toml` | 文件/Nix | 保持不变 | 文件 URL | git revert | P1-P4 |
| config.toml | `default_on_turn_error` | `config.toml` | `settings` | 导入 | 失败通知 | 恢复文件 | P3 |
| config.toml | `delivery_limit` | `config.toml` | `settings` | 导入 | 投递截断 | 恢复文件 | P3 |
| config.toml | `turn_timeout_secs` | `config.toml` | `settings` | 导入 | 超时行为 | 恢复文件 | P3 |
| config.toml | `[notifications]` | Nix fragment | 文件/Nix + env | 保持不变 | ntfy | git revert | P1-P4 |
| config.toml | `config.active.toml` | 派生 | 历史/诊断 | 降级为历史 | 不再被读为源 | 保留备份 | P3 |
| config.toml | `config.toml.bak*` | 配置目录 | backups/ | 移动/保留 | 可恢复 | 保留 | P3 |
| config.d | `70-grants-issue.toml` | `config.d/` | `assignments` | 导入；临时四能力只导入 plugin；归档 `.migrated` | 9 群 explain | 恢复 `.pre-rooms` | P3 |
| config.d | `80-issue-tools.toml` | `config.d/` | `plugin_registry`/`plugin_policy` | 导入 command/args/timeout；归档 | 工具列表 | 恢复片 | P3 |
| 新增 | `plugin_config`（插件自定义运行期参数） | 当前无 | `plugin_config` | 新增；无存量数据；按 schema 校验；密钥扫描 | 写入拒绝测试；插件单独 reload | 删表/回滚 | P4 |
| 新增 | `policy_binding`（策略插件 allowlist） | 当前无 | `policy_binding` | 新增；默认无 active，使用 CorePolicy | 无 active 时走 core；插件崩溃 deny | 删行/回滚 | P4 |
| 新增 | `plugin_migrations` / `plugin_<id>_*` 分表 | 当前无 | 同一 `policy.db` | 新增；core 生成前缀；插件只拿受限句柄 | 前缀隔离/迁移测试 | 删表/回滚 | P2/P4 |
| 新增 | `plugin_storage`（声明制 builtin/file） | 当前无 | `plugin_storage` | 声明导入；策略数据只能 builtin；file 走白名单/备份规则 | name/前缀/备份集测试 | 删表/回滚 | P2/P4 |
| config.d | `90-console.toml`（若有） | `config.d/` | `assignments/overrides/settings` | 导入；控制台改写字库；归档 | 控制台保存 | 恢复 `.bak` | P3/P4 |
| config.d | `*.pre-rooms` | `config.d/` | backups/ | 保留不加载 | 仅回滚 | 复制回 | P3 |
| config.d | 其他 `.toml` | `config.d/` | 只读历史 | 归档/不再加载 | 删除片行为不变 | 恢复 | P3 |
| Nix | `services.acb.sessions` | `vars/networking-downstream.nix` | seed 文件/`sessions` | 降级为“库空时 seed 一次” | 空库 seed；非空跳过日志 | 恢复 Nix | P3 |
| Nix | `modules.dsh.acbSessions` | `modules.dsh` option | seed preset + reconciler | 降级为初始 preset seed；运行期 reconciler | preset render/show | 恢复 Nix | P3/P4 |
| Nix | 端口分配 | `services.acb.ports` | 文件/Nix | 不变 | 监听 | git revert | P1-P4 |
| Nix | unit/依赖/supervisor | `modules.acb` | 文件/Nix | 不变 | systemd | git revert | P1-P4 |
| Nix | profile/generation 选择 | `modules.dsh` | 文件/Nix | 不变 | dsh 启动 | git revert | P1-P4 |
| Nix | issue 服务/DB 路径 | `acb-issue.nix` | 文件/Nix | 不变 | issue API | git revert | P1-P4 |
| Nix | ntfy URL/topic | `acb-notifications.nix` | 文件/Nix + env | 不变 | 通知 | git revert | P1-P4 |
| Nix | preset 生成 | `nix/dsh-bundle` | `preset_manifests`/preset 目录 | 初始 seed；导入后从 `web-acb` 移除 Nix preset bundle；后续 reconciler | 幂等/内容 hash；`dsh --dump-config` 每个 preset id 只出现一次 | 恢复 seed | P4 |
| env/agenix | 6 个 session token（5 个不同） | `config.toml` 明文 | `secrets` token_ref；真值 env/agenix | 生成 `env:NAME`，值迁 agenix | 握手成功；旧值失效 | 恢复 config | P3 |
| env/agenix | plugin secrets | env/agenix/文件 | `secrets` + `plugin_secret_refs` | 生成引用 | provider 启动 | 恢复 env | P3 |
| env/agenix | console token | env/agenix | 不变 | 不落库 | 控制台登录 | rekey | P1-P4 |
| env/agenix | ntfy topic | env/agenix | 不变 | 不落库 | 通知 | rekey | P1-P4 |
| env/agenix | connection token | env/agenix | 不变 | 不落库 | 适配器登录 | rekey | P1-P4 |
| env/agenix | `dsh.env`/`opencode.env` | 配置目录 | agenix/env | 迁移 | 模型/opencode 启动 | 恢复文件 | P3 |
| env/agenix | 其他 env 密钥（`ACB_MODEL_KEY`、`NEW_API_...`、`ACB_ISSUE_TOKEN`、`ACB_OPENCODE_PASSWORD`） | agenix/env | 不变 | 不落库 | 模型/issue/opencode | rekey | P1-P4 |
| env/agenix | unit 部署变量（`ACB_ISSUE_URL`、`ACB_LUA_DIR`、`ACB_OPENCODE_*`、`ACB_CWD`） | Nix/unit | 文件/Nix | 不变 | 服务启动 | git revert | P1-P4 |
| 已有库 | `state/acb.db` records/watermarks/... | 运行库 | 不变 | 不迁 | 消息/水位 | 备份 | P1-P4 |
| 已有库 | `/var/lib/acb-issue/issues.db` | issue 库 | 不变 | 不迁 | issue 工具 | 备份 | P1-P4 |
| 已有库 | access tickets | 内存 | 不变 | 不迁 | 票据 TTL | 重启 | P1-P4 |
| 已有库 | 审计日志 | `*.log` | 文件 + `audit` 表 | 判定事件进库；原始日志留文件 | explain/audit | 保留日志 | P4 |

---

## 11. 分期与验收

### P1 语义

**改什么**：

- 先落 `Policy` trait 和内置 `CorePolicy`（今天这套 RBAC 的默认实现）；
  `check_access`/`platform_call`/`plugin_tool_call`/`access_request` 全部经
  `Policy::decide`，**不得**在 engine 业务代码里直接调 `evaluate_rules` 作为唯一路径。
- `CorePolicy` 内部改成 `floor ∪ grants − deny`；`evaluate_rules` 不再“匹配即否决”。
- 显式 `effect=deny`；未知 capability 加载期硬失败。
- `acbctl grants explain/matrix/diff`（先只读，走 `Policy`）。
- 70 片临时四能力删回 `["plugin"]`。

**验收**：

- 9 个群在 `769843907` 等房间仍能 `room_read/room_write/access_request/plugin`；
- 未订阅场景从 allow 收紧为 deny；
- 审计出现 `basis=base:subscription,rule:N`，不再出现“规则匹配但缺能力 -> 整条 deny”；
- `grants matrix` 只出现预期差异。

**回滚**：恢复 `70-grants-issue.toml.pre-rooms`；若已引入 `grants.mode`，切回
`legacy`；重启 bridge。

**依赖**：无。

**影响文件**：`crates/kernel/src/policy.rs`（trait/CorePolicy）、`grants.rs`、
`engine.rs`、相关测试、`config.d/70`、文档。

**P1 实际落地差异（2026-11-06）**

- `Policy` trait 在 `crates/kernel/src/policy.rs`；`CorePolicy` 无状态，策略快照
  （`GrantQuery.grants`）与 `policy_version` 随请求传入，保证一轮内冻结，不把
  engine 的业务状态带进判定实现。
- `Bridge` 持有 `Arc<dyn Policy>`；engine 的 5 个调用点全部走
  `policy_decision()`，失败/缺 `rule_id`/版本不匹配一律 deny。
- `GrantRule` 新增必填 `id`；新增独立的 `[grants].deny` 列表（没有 `effect` 布尔）。
  **重复 id 现在是启动硬失败**，旧的 `duplicate_rules` 告警路径不再适用于同 id。
- `GrantDecision` 新增 `rule_id`/`policy_version`；`basis` 记录
  `base:*`/`rule:<id>`/`deny:<id>` 来源；`should_audit` 按来源逐项判断，floor+rule
  也会审计。
- 功能边界：ticket 签发/校验、审计写入、失败即拒、能力枚举、break-glass 仍在 core；
  规则匹配、floor/union/deny 计算、来源拼接在 `CorePolicy`。
- 暂留 core、可后续插件化的项：`platform_capability` 风险映射、`TICKET_OPS`
  展示映射、外部 `AccessProvider` 选择、审计文本格式；原因是它们属于能力/风险枚举
  或票据不变量，先不动。
- live 前置：生产 `config.toml` 的 base `[[grants.rules]]` 与 `config.d/70` 都必须
  补 `id`；70 片的四能力临时补丁按 live 步骤再删回 `["plugin"]`。

### P2 schema 与命令面

**改什么**：

- 建 `policy.db`、DDL、约束、`schema_version`、`policy_version`、
  `plugin_migrations`。
- 定插件数据归属：A=`acb.db` core 独占；B=同一 `policy.db` 的 `plugin_<id>_*` 前缀
  分表；C=仅独立进程例外；core 生成前缀并持有句柄。
- `acbctl explain/matrix/diff`（只读）；`sessions/plugins/grant/tree/export/import/seed/
  reload/reconcile/preset` 命令骨架。
- 双读：文件 evaluator vs 库 evaluator；不切写。

**验收**：

- DDL/约束测试：未知能力、越权 scope、快照缺失、parent 环、depth>2、override allow
  全部拒绝；
- 只读工具与旧 evaluator 全矩阵零差异；
- 写工具只写 staging/测试库，不碰生产。

**回滚**：功能开关 `policy.store=file`；不切读路径；删除 `policy.db`。

**依赖**：P1。

**影响文件**：新 policy store 模块、`crates/acb` CLI、`docs/design`、迁移测试。

### P3 迁移与 seed

**改什么**：

- 导入 `config.toml`/`70/80/90`/provider；校验；dry-run；事务；归档旧片。
- Nix `services.acb.sessions`/`modules.dsh.acbSessions`/provider 片降级为 seed。
- 会话/插件/授权/rooms/overrides/settings 切库；`config.d` 变只读历史。
- 插件自定义数据按 `plugin_<id>_*` 分表；`plugin_migrations` 只记录该插件自己的版本；
  不创建第二份 policy 库。

**验收**：

- `import --dry-run` 与 `grants matrix/diff` 零差异；
- 空库启动 seed 一次；非空库 Nix 不覆盖，日志 `seed skipped`；
- 任意 `config.d` 片段删除后行为不变（除归档）；
- `acbctl export` 与库逐条比对一致。

**回滚**：备份 + `export`；恢复旧文件；`policy.store=file`；重启。

**依赖**：P2。

**影响文件**：importer、nix-config、`config.d`、`acb-drivers`、`acb-config-merge`、
`acbctl`。

### P4 运行期能力

**改什么**：

- 热重载：会话/插件优雅 reload，不断连接；失败回滚上一版本。
- `policy_version` + `resource_versions` 缓存失效；多写者冲突。
- 密钥三档：`token_ref` 默认；`encrypted-store` 可选；`plaintext` 调试。
- 插件 DB 受限句柄：core 绑定 `plugin_id`，只允许 `plugin_<id>_*` 表和
  `plugin_migrations` 自己的行；进程内 `.so` 不拿到裸 SQLite 连接。
- preset reconciler：`preset render`/`reconcile`/`show`；generation/hash/degraded。
- 策略插件：`policy_binding` allowlist、`Policy::decide` 插件 adapter、按
  `policy_version`+`resource_version` 缓存、崩溃 fail-closed/显式 core fallback。
- 控制台写库；`90-console` 只 export/seed。

**验收**：

- `reload` 不断连接；失败回滚；会话/插件变更生效；
- 版本 bump 后 bridge 缓存失效，判定变化；
- 加密库/列在拿不到密钥时 fail closed，错误可读；轮换后旧密钥失效；
- `reconcile` 幂等（第二次不写）；删除只删 manifest 自有文件；删除会话先停实例；
- `preset_schema_version` 不符拒绝渲染并 degraded；
- `plugin_config` 写入成功后 bump `resource_versions('plugin', <id>)`，bridge **只重载
  该插件**，不整栈重启；重载失败保留上一版本并标记该插件 degraded；第二次写相同值
  不产生新 generation/不重载；
- 策略插件绑定后，所有判定仍只经 core 的单一收口；插件崩溃/超时/panic 默认 deny，
  只有显式 `fallback=core` 才回退并写审计；缓存不跨 `policy_version`/`resource_version`；
  每条审计有 `policy_provider`/`rule_id`/`policy_version`/`resource_version`；
- `plugin_storage`：策略数据声明 `file` 被拒绝；file 的 `name` 只允许 basename 且
  落在 core 目录白名单；`rebuildable` 不进备份；`backup` 进备份并按 `backup_order`
  恢复（core 表先、插件文件后）；无跨库事务，插件失败只影响自身文件。

**回滚**：版本回滚 + 备份；`token_ref` 模式可退回；reconciler 保留上一代。

**依赖**：P2/P3。

**影响文件**：bridge reload/cache、console、policy store、secrets、preset reconciler、
unit `LoadCredential`。

---

## 12. 验收总纲：证明“只剩一个数据源”

P1-P4 每期结束：

| 期 | 判定这期真的做成 |
|---|---|
| P1 | 9 群仍 allow；未订阅收紧；matrix 只含预期差异；审计 basis 不再是“规则缺能力 deny” |
| P2 | schema 约束全拒绝；只读 explain/matrix/diff 与旧 evaluator 零差异；写路径未接生产 |
| P3 | `import --dry-run` 零差异；空库 seed 一次；非空库 Nix 不覆盖；删任意 `config.d` 行为不变；export 与库逐条一致 |
| P4 | reload 不断连接且失败回滚；缓存按版本失效；加密 fail closed；reconcile 幂等；preset schema 不符拒绝 |

整套完成后，用以下方式证明单一数据源：

1. `acbctl export` 全量导出；与 `policy.db` 逐表逐行比对（行数、主键、hash、快照）。
2. 删除任意 `config.d` 片段（已归档），重启/`reload` 后行为不变；证明它们不再是源。
3. `acbctl grants explain` 对每个 subject×resource×capability 都能给出 floor/assignment/
   override/relation 来源；审计行有 `policy_version`。
4. `acbctl reconcile` 连续跑两次，第二次零写入；preset 目录内容 hash 不变。
5. Nix `nixos-rebuild` 改 `services.acb.sessions`/provider 片，库非空时启动日志显示
   `seed skipped`，运行行为不变；证明 Nix 不再是运行源。
6. 控制台、`acbctl` 的每次写都有 `audit` 行；直接 `sqlite3` 写库被 `doctor` 标记
   （校验 `policy_version`/审计缺口）。
7. **按实体核对**：逐实体（grants 规则、角色指派、房间边界、会话、插件注册、插件
   参数）确认只有一个可写源；其余位置是只读派生（export/preset/Nix seed）或已归档。
   发现第二份即验收失败，不论两份当前是否相同。

---

## 13. 风险与未决问题

| 风险/未决 | 说明 | 建议 |
|---|---|---|
| dsh preset 动态 | reconciler 解决渲染，但 dsh 是否热加载新 preset 需实测 | P4 先做 `render/show`，再验证 dsh reload；不支持则要求重建 dsh 实例 |
| `command` 进库 | Nix store 路径会随重建/GC 失效；放弃 Nix 复现 | 存 `command_kind`/fingerprint；`doctor` 检查；生产用 profile 稳定路径 |
| 多写者冲突 | console 与 acbctl 同时写 | `BEGIN IMMEDIATE` + 行 `version` + `conflict` 错误；不做静默覆盖 |
| 密钥三档 | encrypted-store 需要 SQLCipher/AEAD 构建；plaintext 风险 | 默认 token_ref；encrypted 需显式；plaintext 需显式 env 开关 |
| 角色爆炸 | 一房间一角色 | 阈值（>5 override/role 或 >20% role-room 对）退回 ABAC |
| 委托滥用 | `grant_manage` 可造新授权 | depth 2、快照、非越权、can_delegate、审计；高危走审批 |
| 策略插件越权 | 它能读全量主体/房间/关系，权限比普通 tool provider 高 | `kind/trust_tier=policy` + `policy_binding` 显式 allowlist + 只读快照 + fingerprint；默认 core |
| 策略插件故障 | 超时/panic/缓存过期可能放行 | fail-closed 默认 deny；只有显式 `fallback=core` 才回退并审计；不跨版本用缓存 |
| 插件表前缀冲突 | 两个插件/版本抢同一表名 | core 生成并校验 `plugin_<id>_*` 前缀；`plugin_migrations` 按 plugin_id 分段；插件不能自定义表名 |
| 插件自开 DB | 同文件多连接导致 schema/WAL/备份分叉 | 只有 core 开句柄；进程内 `.so` 拿受限 API，不拿裸连接；C 档仅独立进程 |
| 插件迁移失败/回退 | 插件版本低于库版本或迁移冲突 | 单事务执行；失败插件 disabled，core 继续；低版本拒绝加载；默认保留孤儿表 |
| 跨命名空间 FK | 插件表引用他人表 | core 校验表/索引/FK 前缀；FK 只能指向 `plugin_<id>_*`；违规拒绝迁移 |
| trust 语义 | 角色不得绕过 trust | `trust_cap` 只限高危；读写仍按订阅；9 群不受影响 |
| 能力映射 | `account_message`/`account_admin`/`room_history` 当前无独立入口 | P1/P2 先保留 legacy；新入口另开；不要假装已统一 |
| `connection_read`/`platform` | 现有 target 8 类，资源只 3 类 | 保留 legacy/internal；后续再折叠 |
| `account_secret` | 只能票据 | 保持 `grantable=0`；不进入角色 |
| 插件 on-demand | 现有 process-tools 只常驻 | v1 只 resident；on_demand 预留，不承诺 |
| `plugin_config` schema 漂移 | 插件升级后参数 schema 变化 | 注册时存 `config_schema_json`/fingerprint；写入按当前 schema 校验；不匹配标记 needs_review |
| `plugin_config` 密钥误判 | 高熵启发式可能误杀/漏判 | 敏感 key 硬失败；value 高熵硬失败；错误提示走 `plugin_secret_refs`；允许 `allow_unknown` 但不放宽扫描 |
| 审计量 | 每次判定写 audit 可能大 | 只写有 assignment/override/deny 的判定；原始日志留文件 |
| 迁移漂移 | 文件/库双读期可能不一致 | dry-run + matrix/diff + export 比对；不一致禁止切换 |
| DB 损坏 | policy.db 是单点 | WAL、备份、export、`doctor`；break-glass |
| Nix seed 漂移 | 库非空后 Nix 改 seed 不生效 | 日志 `seed skipped`；`acbctl seed --force` 显式 |
| 90-console/[fragment] | 旧方案退场 | 控制台写库；文件只 export/seed |
| 未决：console/ntfy 是否也进库 | 当前留文件/env | 若运行时可改，按原则进 `settings`/`secrets` |
| 未决：policy.db 是否同 acb.db | 当前建议分开 | 多写者/高频读写分离；若合并需重新评估锁 |
| 未决：break-glass 形式 | 文件/env/本地 token | 5 分钟 TTL、固定能力、独立审计；不进库 |

---

## 14. 可行性核验（对实际代码）

判定三档：**能实现** / **需新增能力（说明缺什么）** / **不可行（说明原因）**。

| # | 项 | 判定 | 关键证据 |
|---|---|---|---|
| 1 | Policy trait 插入现有判定链 | **能实现**（需同步改 4 个调用点 + 测试） | `engine.rs:4548/4941/5158/6665/10194`；`grants.rs:711-792` |
| 2 | `.so` 进程内插件 | **需新增能力**（当前只有 adapter `.so`，没有 tool/policy/storage ABI） | `adapters/dynamic/src/lib.rs:53/85/87/2244`；`adapter-api/src/lib.rs:33-96`；`plugins/process-tools/src/loader.rs:48-60` |
| 3 | 宿主存储接口两形态 | 进程内 Rust **能实现**；`.so`/进程外 **需新增能力** | `adapter-api/src/lib.rs:61-67`（HostV1 只有 submit_event/should_stop/log）；`kernel/src/plugin.rs:39` |
| 4 | SQLite 迁移/前缀/备份 | 同库加表/迁移/authorizer **能实现**；多进程/DB 备份 **需新增能力** | `storage/sqlite.rs:39/155/198/202/210/247/341`；rusqlite `hooks/mod.rs:436` |
| 5 | sessions 进库与 Nix eval | 库为准 + seed **能实现**；运行中新增 dsh 会话/preset **需新增能力** | `acb/default.nix:87-113/296-308`；`dsh/default.nix:765-768/925-936`；`nix/dsh-bundle/default.nix:10-22` |
| 6 | P1-P4 | P1 可立即落地；P2/P3 需前置；P4 部分需新增能力 | 见 §14.6 |

### 14.1 Policy trait

现状：`grants::evaluate_access`/`evaluate_platform`/`evaluate_access_request`/
`evaluate_tool` 在 `grants.rs:711-792`；engine 只有 5 个调用点：
`4548`（platform）、`4941`（plugin tool）、`5158`（access_request）、`6665`
（check_access）、`10194`（high_risk_grant 展示）。没有散落在业务代码里的第二套判定。

结论：**能实现**，但要同步改：

- `engine.rs:6621-6807` `check_access`：把内置分支换成 `Policy::decide`；外部
  `AccessProvider` 分支保留，二者仍只能有一个生效；
- `engine.rs:6750-6762` ticket 回退：Policy 先 deny，ticket 仍可授权；ticket 校验
  不能由 Policy 替代；
- `engine.rs:6885-6898` 审计：Policy 返回 `rule_id`/`reason`，审计格式仍由 core 写；
- `engine.rs:4548` `evaluate_platform` 的 `Option` 语义（无匹配 = 基础访问放行）
  要由 `CorePolicy` 显式编码为 allow，不能直接改成“无匹配 deny”；
- `engine.rs:4941` admin `evaluate_tool` 无匹配 = deny，要和普通主体 base allow 区分；
- 测试：`grants.rs:891/1000/1020` 单测，以及 `tests/access_ticket_flow.rs`、
  `tests/plugin_tools_flow.rs`、`tests/platform_flow.rs`、
  `tests/place_approval_flow.rs` 需要跑一遍矩阵。

风险：Policy 插件返回 `rule_id` 必须可追溯到 assignment/override/relation/floor；
`policy_version` 必须与请求一致；否则 core 直接 deny（`invalid_policy_decision`）。

### 14.2 `.so` 进程内插件

现状不是“没有”，而是**只有 adapter `.so`**：

- `crates/adapters/dynamic` 用 `libloading`（`Cargo.toml:14`、`src/lib.rs:53`），
  `Library::new`（`:85`）、入口 `acb_adapter_entry_v1`（`:87`）、
  `abi` 函数表在 `crates/adapter-api/src/lib.rs:33-96`；
- 有 ABI v1/v2、`PLUGIN_FLAG_UNLOAD_SAFE`、`catch_unwind` 边界
  （`dynamic/src/lib.rs:2244/2286/2294`）；
- 但 `HostV1` 只有 `submit_event`/`should_stop`/`log`（`adapter-api:61-67`），
  **没有 storage handle、没有 Policy 决策入口**；
- tool provider 目前是子进程 JSON：`plugins/process-tools/src/loader.rs:48-60`、
  `provider.rs:111-120` spawn；`kernel/src/plugin.rs:39` 的 `PLUGIN_API_VERSION` 是
  manager/provider descriptor 版本，不是 `.so` ABI。

结论：**`.so` tool/policy/storage 是新增能力**。代价：

- ABI 稳定：需要新增 `acb_plugin_entry_v3` 或 RPC，函数表只加不改；
- panic 隔离：跨 FFI 的 `catch_unwind` 不能覆盖所有 UB；插件 panic 必须被 core 边界
  捕获并 deny；
- Rust 版本耦合：`.so` 与宿主 Rust ABI/allocator 耦合，跨版本不可靠；
- unsafe 边界：句柄生命周期、借用、卸载后失效都要在 core 侧强校验。

建议：v1 只支持“进程内 Rust trait + 进程外 JSON RPC”；`.so` storage/policy 不进入
第一版，除非先做 ABI v3 和跨版本测试。

### 14.3 宿主存储接口两形态

- 进程内 Rust：`StorageHandle` 是宿主 crate trait，直接传 handle；**能实现**，无 ABI
  开销。
- `.so`：当前 `HostV1` 没有 storage 字段；需要新增 ABI v3（函数表加 `storage_*`）或
  让 `.so` 走进程外 RPC。**需新增能力**。
- 进程外：必须由宿主侧代理执行 SQL，插件只发请求；前缀校验在宿主侧（rusqlite
  authorizer），不在插件侧；事务语义：
  - 宿主持有外层事务，插件 `tx` 用 savepoint；跨进程用 `tx_token` + request-id 绑定，
    插件不能直接 `BEGIN/COMMIT` 整库；
  - 超时/行数上限/BUSY 重试全部宿主侧做；插件只收到结果/错误；
  - 若插件要求“跨进程长事务 + 多语句原子提交”，**不可行**（SQLite 事务不跨进程）；
    替代方案是插件独立进程独占文件（C 档），但不得与策略库同事务/join。
- `.so` 若强行拿裸连接，会破坏 core 的 WAL/备份/加密/迁移；结论：不提供裸连接，
  只提供受限 handle；`.so` 形态不进入第一版。

### 14.4 SQLite 迁移可行性

- 同库加表：`storage/sqlite.rs:39` `SCHEMA_VERSION=11`，`:42` `SCHEMA` 全部
  `CREATE TABLE IF NOT EXISTS`；新增 `plugin_<id>_*` 表/索引可行；
- 迁移：`:247-347` 已有“先补列/表、再 bump 版本”的迁移路径；`plugin_migrations`
  可按 plugin_id 记录；
- WAL/busy：`:198` `busy_timeout(5s)`、`:202` `journal_mode=WAL`、`:210`
  `synchronous=NORMAL`；但 `:155` 明确“多进程同时开同一文件不在支持范围”，
  所以 `policy.db` 的多写者需要新连接/事务策略，不能直接复用 `acb.db` 的单进程契约；
- 备份：当前没有 DB 备份 API，只有控制台 `90-console.toml.bak`
  （`bridge/src/console.rs:1311-1333`）；`VACUUM INTO`/`Connection::backup` 可行，
  但是新增能力；文件拷贝在 WAL 下不安全；
- 前缀强制：rusqlite 0.37 有 `Connection::authorizer`（`hooks/mod.rs:436`），可在
  SQLite 层拒绝非 `plugin_<id>_*` 表、`ATTACH`/`DETACH`/`PRAGMA`/系统表；比解析 SQL
  可靠。`VACUUM` 不进入 `StorageHandle` API 面（`exec` 只接受 DML、DDL 只走
  `migrate`），authorizer 只作兜底。DDL/索引名也要在迁移检查里校验。
- 结论：同库分表、按插件迁移、前缀强制 **能实现**；多进程写、DB 备份、加密
  **需新增能力**，不能假装已有。

### 14.5 sessions 进库与 Nix eval

- `modules.acb.sessions` 是 eval 期 option（`acb/default.nix:296-308`），
  `driverSessionBlocks`（`:87-113`）在 Nix 求值时生成 driver 块；
  `modules.dsh.acbSessions`（`dsh/default.nix:925-936`）在 eval 时生成
  `acbBundle`（`:765-768`），再进 `web-acb` profile（`:1089-1092`）；
  preset 行由 `nix/dsh-bundle/default.nix:10-22` 渲染；端口来自
  `vars/networking-downstream.nix:586-619`。
- 结论：
  - **库为准 + seed 能实现**：Nix 生成初始 sessions/presets/ports，库空时写入；
    库非空跳过；这不需要 Nix 读库。
  - **运行中新增 dsh 会话需新增能力**：`acb-drivers` 现在是 Nix 固定块，必须改成
    读库的通用 supervisor；动态端口要从 Nix 预留的 range 分配；preset 必须由
    reconciler 渲染到 `web-acb` 能看到的目录。
  - **不可行**：让 Nix eval 期知道运行期 DB 并生成新 session 的 unit/preset。
    Nix 只能 seed；运行期新增必须走 supervisor + reconciler。
  - **条件**：dsh 必须扫描/热加载 reconciler 目录；如果 dsh 只在启动时读 profile，
    新增会话需要重启 dsh 实例——这仍是“动态创建”，但不是“零重启”。`web-acb`
    profile 必须包含该目录；这是新增的部署项，不是纯库改动能解决的。
  - opencode 会话不走 dsh preset/端口，仍由共享 `acb-opencode-serve`；不在这个
    supervisor 的 `preset` 路径里。

### 14.6 P1-P4 可行性

| 期 | 判定 | 最小验证 |
|---|---|---|
| P1 | **可立即落地**（代码范围已定位；Policy trait + CorePolicy + 语义修复） | `cargo test -p acb-kernel grants`；`cargo test --test access_ticket_flow --test plugin_tools_flow --test platform_flow --test place_approval_flow`；9 群 live explain |
| P2 | **需前置 P1**；schema/命令面可做，写路径先 staging | DDL 在临时 sqlite3 执行；`grants matrix` 与旧 evaluator 零差异；未知能力/越权/快照/环/depth 拒绝测试 |
| P3 | **需前置 P1/P2**；seed/导入可做；动态新增会话需 P4 的 supervisor | `import --dry-run` 零差异；空库 `seed applied`、二次 `seed skipped`；删 `config.d` 行为不变；`export` 对账 |
| P4 | **需前置 P1/P2/P3 + 新增能力**；in-process Rust 可做，`.so` storage/policy 不建议第一版 | `reload` 不断连接；`plugin_config` 只重载单插件；`reconcile` 二次零写；加密 fail closed；策略插件崩溃 deny；`.so` ABI 先不做 |

**不建议做**：`.so` policy/storage ABI 第一版；跨进程长事务；Nix eval 生成运行期
session/preset；把 `acb.db` 改成多进程写。**不可行**：跨进程 SQLite 事务、`.so` 裸
连接、让 Nix 读运行期 DB 生成 unit。

---

## 附录 A：Nix 选项与当前对齐

### A.1 字段

| 选项/来源 | 字段 | 类型 | 默认 | 必填 |
|---|---|---|---|---|
| `services.acb.hosts.<host>.sessions[]` | id | string | 无 | 是 |
| 同上 | preset | string | 无 | 是 |
| 同上 | port | int | 无 | 是 |
| `modules.acb.sessions` | id | str | 无（整体默认 `[]`） | 是 |
| 同上 | preset | str | 无 | 是 |
| 同上 | port | `types.port` | 无 | 是 |
| `modules.dsh.acbSessions` | id | str | 无（整体默认 `[]`） | 是 |
| 同上 | preset | str | 无 | 是 |
| 同上 | port | `types.port` | 无 | 是 |

`services.acb` 是 `vars/networking-downstream.nix` 的普通 attrset 注册表，不是 Nix
option；host 在 `hosts/idols-ai/default.nix:138-154` 把同一份 sessions 赋给两个 option。

### A.2 消费点

- `modules.acb.sessions` → `acb/default.nix:87-113` `driverSessionBlocks` →
  `driver-session.sh` → `acb-drivers`。每会话一个 dsh driver，`ACB_PRESET`/`ACB_MCP_PORT`/
  endpoint/DSH URL；token 由脚本从 `config.toml` 提取（迁移后改 `token_ref`）。
- `modules.dsh.acbSessions` → `dsh/default.nix:765-768` `acbBundle` override →
  `nix/dsh-bundle/default.nix` 生成 `preset-<preset>` 行，MCP client
  `http://127.0.0.1:<port>/mcp` → `webAcbBundles` → `nix-web-acb` profile。
- 两份必须一致：id = config session id；preset = dsh bundle preset id；port = driver
  MCP 端口 = preset URL 端口。不一致 -> token 查不到/预设缺失/工具连不上/端口冲突。

### A.3 当前对齐

| 会话 | services.acb.sessions | modules.dsh.acbSessions | 端口 | preset |
|---|---|---|---|---|
| dsh-qas | 是 | 是 | 3101 | acb-dsh |
| dsh-group2 | 是 | 是 | 3102 | acb-dsh-group2 |
| dsh-dev | 是 | 是 | 3103 | acb-dsh-dev |
| dsh-snowluma-test | 是 | 是 | 3104 | acb-dsh-snowluma-test |
| opencode-1093015329 | 否 | 否 | 4097（共享 serve） | 无 |
| opencode-1124995534 | 否 | 否 | 4097（共享 serve） | 无 |

4 个 dsh 会话逐条对齐；2 个 opencode 不在两个 Nix 选项里。当前是 6 个会话，
不是 8 个；8 个 token 字段 = console + QQ 连接 + 6 个会话 token，其中 opencode 两个
会话共用 token。

---

## 附录 B：现有能力/资源与鉴权入口

| 代码能力 | 入口 | 说明 |
|---|---|---|
| `room_read` | `ensure_readable`/`check_access(Read)` | get_history/fetch_records/wait/notes/events |
| `room_write` | `ensure_can_send`/`check_access(Send)` | send_message/send_message_to |
| `connection_read` | `ensure_connection_readable`/`check_access(Connection)` | 连接级读 |
| `platform` | `platform_capability` → `evaluate_platform` | 普通平台方法 |
| `destructive` | `Risk::Destructive` | kick/mute/mute_all/member.admin/name 等 |
| `critical` | `Risk::Critical`/unverified | 票据 |
| `account_read` | `platform_capability`（Account） | account/file/stream 读 |
| `account_secret` | `ticket_capabilities` | 只能票据 |
| `access_request` | `evaluate_access_request` | 票据申请 |
| `plugin` | `evaluate_tool` | plugin_tool_call；room scope 另需 room_write，connection scope 另需 connection_read |

现有 `AccessTargetKind` 8 个：room/message/member/file/account/connection/plugin_tool/
platform_op。新资源模型只保留 room/account/tool，其余先保留 legacy，不假装已统一。

无独立鉴权入口：`member.kick`/`group.mute`/`group.mute_all`/`member.admin`/`group.approve`
只走 destructive 风险；`SenderRole::Owner/Admin` 只记录不鉴权；`account_message` 无入口
（私信仍 room_write）；`account_admin` 无能力；`plugin` 永远按当前房间判定；
`capabilities` 是只读探测；`ack_messages` 靠轮次票。

---

## 附录 C：迁移后的 `config.toml` 与 `config.d`

- `config.toml` 只保留文件域：endpoint/console listen、connections 拓扑、routes、
  personas/memories/harnesses、file_relay、notifications、运行调参（若不愿进 settings）。
- `config.d` 归档为只读历史；bridge 迁移后不再加载；`config.active.toml` 只作历史。
- 90-console 不再作为覆盖层；控制台写 `policy.db`；文件只作 export/seed。
- 任何删除片/改片的行为在迁移后必须不变（因为它们不是源）。

