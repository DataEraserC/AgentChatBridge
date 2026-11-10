# 配置源、RBAC 与插件平台计划（2026-11-10）

> 状态：planning，未实现。对齐后再拆分提交。
> 关联台账：`docs/plan/todo-ledger.md` 的 F1b / F4 / F5 / F6 / J1-J4。
> 关联设计：`docs/design/acb-policy-store.md` §3.3-3.7、§6.2-6.4；`docs/plan/plugin-api.md`。

## 0. 现状（已核实，2026-11-10）

| 项 | 事实 | 证据 |
|---|---|---|
| 活配置 | `~/.local/share/acb/config/config.toml` 107 行，只有 secrets/endpoint/console/tickets/file_relay/notifications + 3 个全局键 | `acb-bridge --check` 报文件贡献 connections 0 / sessions 0 / rules 0 |
| 业务对象 | 连接 1 / 会话 6 / 人格 2 / 记忆 2 / harness 2，全在 `config_objects`（schema 15），session 行存 TOML 原文（含订阅） | `sqlite3 state/acb.db "select kind,count(*) ..."` |
| 权限 | `policy_rules` 11 / `policy_rooms` 7 / `policy_accounts` 0 / `policy_admins` 1 | 同上 |
| 主体与能力 | 4 个写死主体 `system/system_admin/bot_admin/user`；10 个写死能力 | `crates/kernel/src/grants.rs:167,329` |
| 角色 | 没有 roles / role_caps / assignments，角色到能力的映射在 `CorePolicy::floor` / `no_grant` | `crates/kernel/src/policy.rs` |
| 策略插件 | `Policy` trait 存在，只有 `CorePolicy`；没有 `policy_binding`、没有 allowlist | 全仓 0 命中 `policy_binding` |
| 会话级外部策略 | `Session.access.provider` 可指向 `ToolAccessProvider`，但 `plugin_tool_call` 在外部 provider 下直接 `return Ok(())`，跳过能力判定 | `engine.rs:5836-5839` |
| 插件注册 | `[[tool_plugins]]` 在 `config.d/80-issue-tools.toml`，command 是 nix store 路径 | 活配置 |
| 插件存储/配置 | 无 `plugin_registry` / `plugin_config` / `plugin_storage` / `plugin_secret_refs` | 全仓 0 命中 |
| 控制台插件页 | 只有 reload/unload，没有配置表单；四个 tab 仍是占位条 | `console.html:269-275`、`:262-285` |
| 驱动 | `deploy/presets/drivers/drivers_acb.sh` 启动时读一次 `acbctl sessions`，之后不再对账；端口 = 3101 + 会话下标 | 脚本全文 |
| 仓库 config | `config.toml` 被 gitignore，但仍含连接/会话/权限业务段；`acb-up.sh`、`start-drivers.sh` 仍引用它 | `git check-ignore`、`grep` |

## 1. 目标（用户 2026-11-10 指令）

1. 仓库里的业务段配置搬进「已弃用」目录；运行时配置文件只留启动必需。
2. 运行期新增/删除会话，驱动自动起停。
3. RBAC：角色可定义、可指派；ABAC 条件继续保留。
4. 权限判定可插件化，但执行收口与 fail-closed 留在 core。
5. 插件存储 builtin / external 可配置，配置落在 ACB 库。
6. 第三方作者不改 ACB 就能写插件；插件配置、CRUD、自定义操作能在控制台完成。
7. Agent 能调用配置类功能，但必须走权限校验与审计。

## 2. 目标架构

### 2.1 配置边界

配置文件只保留「启动前必须知道」的参数：

```text
[storage]  [endpoint]  [console]  [tickets]  [secrets]
[file_relay]  [notifications]
default_on_turn_error  delivery_limit  turn_timeout_secs
```

其余一律入库：连接、harness、人格、记忆、会话与订阅、权限（规则/角色/指派）、
模型后备链、插件注册/策略/存储/配置。

仓库清理：

1. 新建 `config.deprecated/`（gitignore），把仓库根 `config.toml` 移入并命名
   `config.toml.dev-20261109`，放一份 `README.md` 写明「不是运行配置，仅历史参考」。
2. `deploy/acb-up.sh`、`deploy/start-drivers.sh` 的默认配置改为
   `ACB_CONFIG_FILE` 优先、否则 `~/.local/share/acb/config/config.toml`，不再指向仓库根。
3. `config.example.toml` 保留完整示例，但首行加「业务对象以库为准，示例只描述形状」。
4. `config.d/` 只留 `history/`；`80-issue-tools.toml` 在 F4 搬库后归档。

### 2.2 Schema 16：RBAC

新增两张表（角色 + 指派），现有 `policy_rules` 继续作为 ABAC 条件：

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
```

语义：

1. `subject_id`：`subject` 用 `system_admin` 这类固定主体名；`account` 用
   `qq:323622442`；`member` 用 `qq:group/1#12345`（房间#成员）。
2. 内置角色 4 个：`role:system` / `role:system_admin` / `role:bot_admin` /
   `role:user`，能力集合与今天的 floor 一致。**内置角色只用于展示与迁移，求值仍走
   floor，避免一次性改掉 11 条规则的语义。**
3. 自定义角色是能力集合 + 指派范围；指派带 `capability_snapshot` /
   `role_snapshot`，角色日后扩容不追认旧指派（设计稿 §3.4）。
4. 求值顺序：`effective = (floor ∪ role_caps(assignments) ∪ rules) − deny`。
   `role_caps` 先按 `scope_kind/scope_id` 过滤，再按能力集合求交。
5. 审计 basis 增加 `role:<role_id>#<assignment_id>`，`acbctl grants explain`
   必须能打印到具体指派。
6. **能力值固定**：插件不能新增 `capability` 枚举值。插件需要更细的粒度时，用
   `target`（如 `plugin_tool:issue.create`）或 `scope` 表达，不靠新能力名。否则
   core 无法给新能力定风险等级、票据门槛、中文标签和审计字段，等于让插件绕过
   统一风控。插件可以声明「我需要哪些固定能力」，由 core 校验。

API 与控制台：

1. `GET/POST /api/policy` 的返回体增加 `roles` 与 `assignments`；写入仍整份校验。
2. 控制台「权限」页拆三块：规则（现有）、角色、指派。
3. 保存后仍只置 pending，轮次边界生效。

验收：

1. 36 格 matrix 与今天零差异。
2. 新建 `group_moderator`（`room_read` + `room_write`），指派到某群某成员；
   该成员在该群可读写，换一个群被拒；删除指派下一轮拒绝。
3. 角色扩容后旧指派仍按快照，`needs_review=1`。

### 2.2.1 票据审批必须有范围

现状的问题：`approval_allowed`（`engine.rs:3449-3475`）只判「私信/控制台是不是
管理员」或「群是否 trusted + 成员名单」，**不看审批人有没有票据申请的那个能力**。
结果是一个可信群里任何成员都能批准任意票据，包括 `account_secret`。

新规则（三条同时满足才允许批准）：

1. 审批人持有 `access_approve` 能力，且在**当前审批房间**的范围内有效。
2. 审批人持有票据申请的**每一个** `capabilities`，且范围覆盖票据 `target`
   （房间/连接/插件工具）；范围不覆盖就拒绝，并回话说明缺哪个范围。
3. 高危能力（`account_secret` / `destructive` / `critical` / `policy_write` /
   `plugin_admin`）只允许 `system_admin` 或 break-glass 批准；普通 `access_approve`
   不能批。

实现要点：

1. `GrantCapability` 新增 `AccessApprove`，进入固定枚举与风险表。
2. 审批时用同一套 `GrantQuery` 求值两次：一次 `AccessApprove`，一次票据里的
   每个能力；两者都 allow 才继续。
3. 票据审批消息里直接列出「需要能力 + 目标范围」，审批人能看到自己是否有资格。
4. 审计记录 `approver`、`approved_capabilities`、`approval_scope`、`basis`；拒绝
   也落一条，不再静默。
5. 迁移：现有 trusted 群成员默认不再自动有审批权；给 `system_admin` 内置
   `access_approve`，需要群审批的房间显式建 `approver` 角色并指派。迁移脚本
   打印「哪些房间原本靠 trusted 批准、现在需要补角色」，不静默降权。

验收：

1. 可信群成员没有 `access_approve` → 批准被拒并回话。
2. 有 `access_approve` 但没有目标房间 `room_write` → 拒绝。
3. 有 `access_approve` 且范围覆盖 → 批准成功，票据可用一次。
4. 群成员尝试批准 `account_secret` → 拒绝，提示只有 system_admin 能批。

### 2.2.2 插件自定义能力（可选）

如果前提是「所有插件均可信」，可以放开能力值，但必须命名空间化，不能让插件
直接 mint `room_write` 这种 core 能力名：

1. core 能力枚举不变；插件能力 id 统一为 `plugin:<plugin_id>:<name>`。
2. 插件 manifest 声明每个能力的 `risk`（low/medium/high）、`requires_ticket`、
   `scopes`、`description`；core 只校验格式、命名空间和与 core 能力不冲突。
3. 未知/缺省 risk 按 high + 需要票据处理，fail-closed。
4. 角色与指派可以引用插件能力；审计记录全名，展示时按插件分组。
5. 插件卸载后能力变 orphaned，引用它的指派标 `needs_review`，不静默删除。

代价：策略语言变长、控制台要按插件分组、插件消失后规则悬空。
更简单的替代：只保留一个 `plugin` 能力，插件内部自己判权，ACB 不提供细粒度授权。

建议采用**混合模型**，避免二选一的耦合：

1. **粗粒度门禁**：core 永远保留一个 `plugin` 能力，决定「这个会话/角色能不能
   用这个插件」。这层由 core 统一判，插件不需要实现角色系统。
2. **细粒度可选**：插件如果声明了 `plugin:<id>:<name>`，core 在 `plugin` 门禁之后
   再按声明检查；没声明就跳过，交给插件内部判权。
3. **票据门槛归 core**：`requires_ticket` 不进权限插件。插件 manifest 只能「加严」
   不能「放宽」——core 存 `plugin_capabilities` 表（risk / requires_ticket /
   scopes），实际门槛取 `max(core 下限, 插件声明)`，未知一律 high + 票据。
   权限插件只回答 allow/deny，不读插件 manifest，因此不产生耦合。
4. 这样：想集中管权限的插件声明细粒度能力；不想管的插件只靠 `plugin` 门禁 +
   自己的内部逻辑。两种插件可以共存，ACB 不需要知道插件内部模型。

单能力方案的代价：ACB 侧确实不耦合，但**每个插件都要自己实现一套角色/范围/审计**，
管理面分散、审计口径不一致、插件一个 bug 就是越权；它把耦合从 ACB 转移到了
每个插件内部。混合模型把「统一门禁」留在 core，把「插件私有逻辑」留给插件，
是耦合最少的分法。

### 2.3 权限插件化

现状不是「权限拆成插件」，是「trait 在、内置实现、一个会话级旁路」：

1. `Policy` trait + `CorePolicy` 已就位，但没有 provider 解析、没有 allowlist。
2. `Session.access.provider` 可以换成外部 `ToolAccessProvider`，但只覆盖
   read/send/connection 三类粗粒度动作，且 `plugin_tool_call` 在外部 provider 下
   直接放行，属于必须修的旁路。

计划：

1. 修旁路：外部 access provider 只替代「房间/连接可达性」判定；插件工具能力
   仍由 core 的 `GrantCapability::Plugin` 收口，不允许 `return Ok(())`。

**解耦原则（回应「权限插件校验新插件会耦合」）**：

1. 插件之间不互相调用，也不互相校验；所有插件只依赖 core 的固定能力枚举和
   `Policy` facade。
2. 权限插件只收到 core 归一化后的 `(subject, place, target, capability, scope)`，
   不知道插件内部实现，也不需要为新插件改代码。
3. 没有 `policy_binding` 时用 `CorePolicy`；权限插件是可选替换项，不是其他插件的
   运行前置。卸载权限插件必须能回落到 `CorePolicy`，否则不允许卸载。
4. 插件注册只声明「需要哪些固定能力」，注册流程不依赖权限插件；权限插件只在
   运行时回答 allow/deny。这样新增插件不需要改权限插件，权限插件扩容也不需要
   改业务插件。
2. 新增 `policy_binding` 表（设计稿 §3.7）：`plugin_id`、`enabled`、`priority`、
   `fallback`（`deny` / `core`）、`allowlisted_by/at`。没有 binding 时用
   `CorePolicy`；binding 必须显式 allowlist。
3. v1 只支持 in-process policy provider（`kind='policy'`），不引入进程外 IPC；
   超时/panic/非法 Decision 一律 deny；只有 `fallback='core'` 才回退并写
   `policy_provider_fallback` 审计。
4. core 保留不变式：单一执行收口、ticket 不可伪造、能力枚举固定、审计先落盘、
   break-glass 不经插件。

验收：

1. 一个测试 policy 插件「全拒」，所有工具调用被拒且审计含 `policy_provider`。
2. 删除 binding 后回 `CorePolicy`，36 格 matrix 恢复。
3. 插件 panic / 超时 → deny，不出现「插件崩了变全允许」。

### 2.4 插件平台（注册、存储、配置、CRUD、动作）

新增表（按设计稿 §3.7，`plugin_storage.kind` 改为 `builtin` / `external`）：

```sql
plugin_registry(id, kind, trust_tier, command, args_json, timeout_ms, command_kind,
                command_fingerprint, config_schema_json, schema_version,
                migrations_json, storage_api_version, source_seed, created_at, updated_at)
plugin_policy(plugin_id, enabled, scope_kind, scope_id, capabilities_json,
              rate_limit_json, lifecycle, allow_unknown, updated_at)
plugin_storage(plugin_id, kind CHECK(kind IN ('builtin','external')),
               location, data_class, backup, retention, updated_at)
plugin_config(plugin_id, key, value_json, type, encrypted, updated_at,
              PRIMARY KEY(plugin_id,key))
plugin_secret_refs(plugin_id, name, secret_id)
```

存储语义：

1. `builtin`：ACB 提供命名空间。v1 先做 `plugin_kv(plugin_id, key, value_json,
   updated_at)` 与 schema 声明的资源表；卸载默认保留数据，删除需显式确认。
2. `external`：**插件自己加载数据库、自己建表/迁移/备份/降级**；ACB 不提供
   StorageHandle、不转发 SQL、不打开它的库。ACB 只做三件事：存库位置
   （`plugin_storage.location` + `location_kind`）、存凭据引用（`plugin_secret_refs`
   或加密字段）、启动时把这两样交给插件。这完全可行，也是 external 的定义。

   两种模式的职责边界：

   | 事项 | builtin | external |
   |---|---|---|
   | 建表/迁移 | ACB | 插件 |
   | 查询接口 | ACB 提供 KV/资源 API | 插件自己的 DB 驱动 |
   | 备份/保留期 | ACB | 插件 |
   | 库位置 | ACB 命名空间 | ACB 存并传给插件 |
   | 凭据 | ACB secrets | `plugin_secret_refs` / 加密字段 |
   | 控制台数据页 | ACB 通用 CRUD | 插件声明的 resources/actions，或 `ui_url` |

   代价：external 的数据安全、备份、迁移正确性由插件作者负责，ACB 只能显示
   「由插件管理」和健康状态，不能替它做数据浏览或修复。
3. **库内允许密钥，但必须按字段声明是否加解密**：`plugin_config.encrypted=1` 的
   值用 `ACB_KEY_FILE` / `ACB_KEY` 派生的密钥加密后存库（`v1:<kid>:...`），
   读取时按权限解密，日志与 API 默认打码；`encrypted=0` 存明文，启动时对形似
   密钥的字段给警告。加密密钥本身不进任何配置文件。
3. 选择落在 `plugin_storage.kind`，可在控制台改；切换 kind 视为迁移动作，
   必须停用插件并要求显式确认，不能静默搬数据。

插件声明的三块 schema（写进 `plugin_registry.config_schema_json` 或独立 manifest）：

```json
{
  "config": { "type": "object", "properties": { "...": {} } },
  "resources": [
    { "name": "rules", "primary_key": "id",
      "crud": ["create", "read", "update", "delete"],
      "schema": { "type": "object", "properties": { "...": {} } } }
  ],
  "actions": [
    { "name": "test_connection", "label": "测试连接",
      "params_schema": { "type": "object" },
      "result_schema": { "type": "object" } }
  ]
}
```

约束：

1. core 只做类型/枚举/范围/密钥扫描校验，不解释业务字段。
2. 密钥形态的 key/value 硬拒绝（`token/secret/password/api_key/...`、高熵长串），
   引导改用 `plugin_secret_refs`。
3. 控制台按 schema 渲染表单、资源表格和动作按钮；插件自带网页可以交给插件
   作者自觉，默认允许 `ui_url`。控制台仍加最低成本保险：sandbox iframe、独立
   origin、拿不到控制台 cookie/token、全局 `allow_embedded_ui` 开关可由管理员
   关闭。作者对页面内容负责，ACB 只保证隔离边界。
4. 插件作者路径：manifest + 可执行文件 + 可选迁移脚本；`acbctl plugin add`
   或控制台注册；不改 `crates/kernel` 一行代码。

验收：

1. `issue.tools` 从 `80-issue-tools.toml` 搬进 `plugin_registry`，删文件后桥正常启动，
   控制台能看到并改它的 3 个配置项，改完下一轮生效。
2. 写一个 200 行示例插件：声明 1 个配置项、1 个资源（CRUD）、1 个动作；
   不改 ACB 代码即可注册、配置、增删改查、执行动作。
3. 把示例插件切到 `external`，它自己建表存数据，ACB 不碰其库。

### 2.5 会话生命周期对账（F1b）

1. `Session` 增加可选 `driver_port`；dsh 会话必须显式给端口，opencode 不需要。
   迁移时给现有 4 个 dsh 会话写入当前实际端口（3101/3102/…），不再按下标推导。
2. `drivers_acb.sh` 改成常驻对账循环：
   - 每 5 秒跑 `acbctl sessions`；
   - 按 `id + token + port + preset + model` 算指纹；
   - 新会话起驱动；删除/禁用会话 SIGTERM 对应进程；指纹变化重启该驱动；
   - 单个驱动崩溃只重启它（指数退避），不再整组退出。
3. 桥侧已经按轮次边界热加载会话，不需要新的桥→驱动 IPC；两边都以库为准。
4. 端口冲突在写入时校验（DB 唯一约束 + 启动期检查），不再静默平移。

验收：

1. 控制台新增一个 dsh 会话，10 秒内驱动起来、ESTAB +1、MCP 端口监听。
2. 删除该会话，10 秒内驱动退出、端口释放；其余 4 个不受影响。
3. 改某个会话的 token/model，只有该驱动重启。
4. 重启 launcher 后端口与今天一致，不发生平移。

### 2.6 Agent 配置工具与权限

新增工具（走现有 `harnesses/shared/tools.mjs` 单一工具面）：

```text
config        op=list|get|set|delete  kind=session|persona|memory|harness|connection
policy        op=list|get|set|roles|assign
plugin_config op=list|get|set
plugin_action name=<action>
```

能力枚举新增（core 固定，插件不能扩）：

```text
config_read   低危：读会话/人格/记忆/连接/插件配置
config_write  高危：写上述对象；默认只给 system_admin，其他人走票据
policy_write  最高危：改规则/角色/指派；只能票据或 break-glass
plugin_admin  高危：注册/卸载/改插件 command 或存储类型
```

规则：

1. 所有写入复用控制台同一套校验与库写入函数，不出现第二条写路径。
2. 写操作审计到 `policy_audit`（或现有审计表），记录 subject、capability、
   target、before/after 摘要、policy_version。
3. `config_read` 默认授予 `system_admin` 私信/控制台；群里默认拒绝。
4. `config_write` / `policy_write` 默认需要票据；管理员可在控制台显式常驻授权。
5. 工具返回体只给字段与错误，不回显密钥值。

验收：

1. 群里普通成员调用 `config set` → `tool_not_allowed`，审计有记录。
2. 管理员私信 `config get session dsh-dev` 成功；`config set` 需要票据，批准后下一轮生效。
3. `policy roles` 能列出角色；`policy assign` 写入后 matrix 变化符合预期。
4. 所有写操作在重启后仍存在，且 `config.toml` 不变。

### 2.7 密钥迁移（库外 → 库内）

现状：密钥来源是 `[secrets.sources]` 的 env/file（agenix、session-tokens.env）。
目标是「能迁移则迁移」：

1. 新增 `secrets` 表：`id/name/value_ciphertext/encrypted/key_id/source/created_at/
   updated_at`。默认 `encrypted=1`，密文形如 `v1:<kid>:...`。
2. 解析顺序：库内 secret 优先，找不到再回落到 env/file 来源；迁移完成后可以删
   掉文件来源，只留 `ACB_KEY_FILE` / `ACB_KEY` 作为解密钥。
3. `acbctl secrets migrate`：读当前 sources，逐条写库（加密），只打印名字与来源，
   不打印值；不自动删原文件，由人确认后再删。
4. `{ secret = "name" }` 在配置和插件配置里都解析到同一张表；插件通过
   `plugin_secret_refs` 引用，不直接读表。
5. 加密密钥本身不进库、不进配置文件，只能从 `ACB_KEY_FILE` / `ACB_KEY` 读；
   密钥缺失时遇到密文必须启动失败，不能当明文读。
6. 迁移前先导出加密备份；回滚时用备份恢复，不靠「再迁回去」。

验收：删掉 `session-tokens.env` 与 agenix 的 token 来源后，桥仍能启动并解析 6 个
会话 token；`acbctl secrets migrate --check` 只读、零副作用。

## 3. 阶段与顺序

| 阶段 | 内容 | 依赖 | 估时 | 交付 |
|---|---|---|---|---|
| P0 | 仓库配置搬 `config.deprecated/`，修脚本默认路径 | 无 | 0.5d | 仓库不再有活跃业务段 |
| P1 | F1b 驱动对账 + 显式端口 | 无 | 3d | 动态增删会话可用 |
| P2 | Schema 16 RBAC：角色/指派 + 票据审批范围 + API + 控制台 | P0 | 6d | 可定义角色；票据只能由有对应范围权限的人批准 |
| P3 | 插件注册/策略/存储/配置入库（F4/F5-L1） | P0 | 4d | `issue.tools` 从库加载 |
| P4 | 插件配置页 + CRUD + 自定义动作 + 示例插件 | P3 | 6d | 第三方插件零改 core 接入 |
| P5 | Agent 配置工具 + 新能力 + 审计 | P2、P3 | 4d | AI 可配置但受权限约束 |
| P6 | 权限判定插件化（policy_binding） | P2 | 4d | 可替换策略实现，fail-closed |

建议顺序：**P0 → P1 → P2 → P3 → P4 → P5 → P6**。P1 先做是因为它直接卡住动态调整；
P2 先于 P5/P6，因为角色模型是它们的前置。

## 4. 已对齐 / 仍待确认

已对齐（2026-11-10 用户答复）：

1. 能力值固定，插件只能声明使用，不能扩展枚举；更细粒度用 target/scope 表达。
2. 配置驱动可行；`config_write` 走「管理员直接写 + 审计，其他人票据」。
3. 库内允许存密钥，但每个字段要有 `encrypted` 开关决定是否加解密；密钥本身
   只从 `ACB_KEY_FILE` / `ACB_KEY` 读，不进配置文件。库外来源能迁则迁入库内，
   见 §2.7。
4. 插件自带网页不推荐，除非 schema 确实表达不了；默认关闭，显式打开并用
   sandbox iframe 隔离。
5. P6 由我安排：先 P2 角色层，P6 视 P2 落地后再排。
6. 仓库旧 `config.toml` 移动并 gitignore。

仍待确认：

1. `external` 存储的库位置：已确认「路径和 DSN 都允许」，用 `location_kind`
   区分；控制台按 kind 渲染输入框并做基本校验。
2. 票据审批迁移：已确认接受「trusted 群成员默认降权、显式补 `approver` 角色」。
3. 加密字段：已确认控制台允许「查看明文」；查看动作本身要记审计，列表默认打码。
4. 插件自定义能力：建议采用混合模型——core 保留 `plugin` 粗门禁，插件可选声明
   `plugin:<id>:<name>` 细能力；`requires_ticket` 归 core，权限插件不读 manifest。
   请确认这个混合模型，还是坚持只保留一个 `plugin` 能力。

## 5. 风险

1. Schema 16 是单向迁移：换代前必须备份三件套，旧代不认新 schema。
2. 插件 command 进库后，nix store 路径会随 GC 失效；必须做指纹与可执行检查，
   失败要显式 degraded，不能静默少工具。
3. `external` 存储把数据库责任交给插件；ACB 只能保证连接信息不泄露，不能保证
   插件自身迁移正确。
4. 策略插件是最高权点：没有 allowlist、没有 fail-closed 就是全站提权；P6 必须
   有「插件 panic → 全拒」的测试。
5. 票据审批加范围后，现有 trusted 群会一次性失去「谁都能批」的能力；迁移必须
   打印需要补角色的房间，避免上线后没人能批。
6. 库内加密字段的密钥轮换要带 `kid`；旧密文在密钥缺失时必须报错而不是当明文读。
7. 密钥迁入库后，备份文件就是新的泄露面；备份必须加密，权限 0600，且不能进仓库。
8. 控制台一次加三块（角色/插件配置/资源 CRUD）容易做成半成品；按 P2/P3/P4
   分开验收，每阶段都要有真实页面截图与一次真机操作。
