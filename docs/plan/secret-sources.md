# 密钥来源抽象（Secret Sources）设计草案

状态：**已实现（2026-11-07）**。设计结论已落到代码、fixture、控制台和部署脚本。

目标：把「密钥从哪来」从「配置文件里的 `${NAME}` 文本插值」改成可替换的
**来源声明 + 名称引用**。至少支持 `env`（读进程环境，agenix 现在走这条）和
`file`（读独立密钥文件，权限自管），以后可扩 keyring 之类的提供者。

硬约束：

- 配置里只出现「来源类型 + 名称/路径」这类引导参数；密钥本体不进数据库、
  不进配置文件、不做配置插值。
- 读不到密钥必须 fail-closed：明确报错退出，不能静默变成空串。
- 无 systemd 的启动路径（`examples/deploy/no-systemd-run.sh` 用 `env -i` 白名单）
  也必须能拿到密钥。
- 一次性迁移，不长期双读「旧插值」和「新来源」两套机制。

---

## 0. 结论摘要

1. 新增顶层 `[secrets]` 声明和 `[[secrets.sources]]` 命名来源；所有密钥字段改成
   `{ secret = "<name>" }` 引用，解析在配置加载期完成。
2. 两种来源：`kind = "env"`（变量名）/ `kind = "file"`（绝对路径）。
   `file` 支持 `format = "raw"`（一文件一值，默认）和 `format = "dotenv"`（KEY=VALUE，
   兼容现有 agenix `acb.env` / `ntfy-acb-topic.env`）。
3. **删除 `${NAME}` / `${{NAME}}` 文本插值**（同一个改动里删干净，不留兼容层）。
   非密钥部署常量（端口、model、路径、数字）改为字面量或由 Nix 生成到配置分片。
4. 解析是**启动期一次性、全量、fail-closed**：任一来源缺失/不可读/权限不合规/
   值为空/引用未知，都算配置错误，`acb-bridge` 以 78 退出（systemd 不重启）。
5. 作用域决策：**单一全局命名空间**，来源声明全局唯一；「作用域」由引用所在的
   字段决定（连接 / 会话 / 全局）。不设 per-connection / per-harness 的来源子表。
6. 无 systemd 路径：`file` 来源由桥进程自己直接读文件，不依赖 `env -i` 白名单，
   因此不需要给脚本加白名单键。
7. 边界：本设计只覆盖**桥自己消费**的密钥（连接 token、会话接入 token、控制台
   token、通知 token/topic）。dsh/opencode 等子进程直接消费的 API key 不在此列，
   它们继续由各自的环境/EnvironmentFile 提供（见 §8）。

---

## 1. 现状基线（代码事实）

### 1.1 插值入口

| 位置 | 事实 |
|---|---|
| `crates/kernel/src/config.rs:1219-1278` | `expand_env_vars`（:1224）在 TOML 解析**之前**对整段文本做字符串展开，支持 `${NAME}`、`${{NAME}}` 和 `:-默认` |
| `crates/kernel/src/config.rs:1295-1302` | `Config::parse` 先展开再 `toml::from_str` |
| `crates/kernel/src/config.rs:2982-2999` | 分片文件 `parse_config_file` 同样逐文件展开 |
| `crates/kernel/src/config.rs:1280-1287` | `escape_toml_string` 只服务于插值 |

关键点：展开发生在 TOML 解析之前，所以**任何**字段都能拿到环境变量，包括
token；代码无法区分「密钥字段」和「端口字段」。这正是当前无法做到
「密钥只走来源、其余不做插值」的原因。

### 1.2 密钥字段

| 字段 | 定义 | 原始结构 |
|---|---|---|
| `connections[].token`（可选） | `Connection.token: Option<String>`（:337） | `RawConnection.token: Option<String>`（:2847） |
| `sessions[].token`（必填） | `Session.token: String`（:618） | `RawSession.token: String`（:2693） |
| `console.token`（必填） | `ConsoleConfig.token: String`（:547） | `RawConsole.token: String`（:2893） |
| `notifications.token`（可选） | `NotificationsConfig.token: Option<String>`（:446） | `RawNotifications.token: Option<String>`（:2783） |
| `notifications.topic`（可选，实际按密钥管理） | `NotificationsConfig.topic: Option<String>`（:444） | `RawNotifications.topic: Option<String>`（:2782） |

消费点只有少数几处，适合保持「解析后仍是 `String`」的形态：

- 连接 token：`crates/adapters/onebot-snowluma/src/link.rs:684-692, 1143-1147`；
  另有 `crates/kernel/src/wire.rs:119` 把 token 带进 Wire 配置
- 会话 token 匹配：`crates/kernel/src/endpoint.rs:141`
- 控制台鉴权/脱敏：`crates/bridge/src/console.rs:478, 504`
- 通知 token：`crates/plugins/process-tools/src/notifier.rs:47-61`

### 1.3 配置合并与覆盖

| 位置 | 事实 |
|---|---|
| `config.rs:3141-3171` | `merge_document` / `merge_entry`：数组追加，普通表「后者整体覆盖」 |
| `config.rs:3173-3198` | `merge_grants` 是唯一按业务键合并的特例 |
| `config.rs:3217-3230` | `apply_console_overlay`：`90-console.toml` 的顶层键整体替换 |
| `config.rs:3113-3139` | `check_duplicate_ids` 只检查 connections / sessions 的 id 跨文件重复 |
| `config.rs:3063-3082` | `finish_loaded` 把合并后的 TOML 序列化再反序列化，产出 `merged_toml` |

因此新增 `[secrets]` 必须补一个 `merge_secrets` 特例，否则第二个分片会把第一个
分片的 sources 整体覆盖掉。

### 1.4 部署路径

| 位置 | 事实 |
|---|---|
| `deploy/units/systemd/acb-bridge.service.tmpl:15` | 桥单元用 `EnvironmentFile=-@ACB_UNIT_ENV_FILE@` 注入密钥 |
| `deploy/units/systemd/acb-launcher.service.tmpl:18` | launcher 同样注入 |
| `deploy/units/systemd/dsh-acb.service.tmpl:15` | dsh harness 同样注入（它需要自己的 API key） |
| `crates/acb/src/run.rs:375-397` | launcher 起子进程不 `env_clear`，systemd 的环境会传给桥 |
| `examples/deploy/no-systemd-run.sh:13-21,49-56,83,93` | 脚本先 source `deploy/acb.env`，但随后用 `env -i` 只导出 `ACB_UNIT_*`；密钥和其它变量被丢掉 |
| `crates/acb/tests/examples_contract.rs:29` | 静态约束：示例脚本只能导出白名单 `ACB_UNIT_*` |

### 1.5 配置之外还读密钥的脚本

| 位置 | 事实 |
|---|---|
| `deploy/start-drivers.sh:57-65,79-88` | 用 awk 从 `config.toml` 的 `[[sessions]]` 里直接提取 token，塞进 `ACB_TOKEN` 给驱动 |
| `deploy/acb-up.sh:102-107` | 用 grep 从 `config.toml` 提取 console token 做健康探针 |

这两处是「密钥本体进配置文件」的直接证据，迁移时必须一起改，否则新模型下
脚本会读到 `{ secret = ... }` 而不是 token。

### 1.6 现有密钥清单（只列键名）

`/run/agenix/acb.env`：`ACB_CONSOLE_TOKEN`、`ACB_SNOWLUMA_TOKEN`、`ACB_MODEL_KEY`、
`NEW_API_Y9000P2025_OPENAI_RESPONSES_API_KEY`、`ACB_OPENCODE_PASSWORD`（可能还有
`ACB_ISSUE_TOKEN`）。

`/run/agenix/ntfy-acb-topic.env`：`NTFY_ACB_TOPIC`。

其中只有前两项 + `NTFY_ACB_TOPIC` 属于「桥自己消费」；其余是 dsh/opencode/issue
服务自己的环境变量。

---

## 2. 设计总览

三层结构：

1. **声明（source）**：`[secrets]` 下的命名来源，描述「类型 + 变量名/路径」。
2. **引用（ref）**：密钥字段写 `{ secret = "<name>" }`，不写值。
3. **解析（resolve）**：配置加载期把每个 ref 解析成值，填进原有的 `Option<String>` /
   `String` 字段。解析失败即配置加载失败。

这样做的收益：

- 消费方（适配器、endpoint、console、notifier）完全不需要改类型，改动集中在
  `config.rs`；
- 「用哪来的密钥」和「密钥用在哪」解耦：同一个来源可以被多个字段复用；
- 未来加 `keyring` / `systemd-creds` 只是新增一个 `kind`，不改字段。

不做的事：

- 不把密钥写进 SQLite（与 `docs/design/acb-policy-store.md` 里 `secrets` 表的设想
  无关，本设计明确不落库）；
- 不让插件/适配器在 `options` 里声明密钥（`options` 是原样透传的 JSON/TOML，
  core 不解释；动态库插件要拿密钥属于后续 plugin-api 话题）；
- 不做密钥轮换热更新；文件/环境变化仍然要重启（与控制台保存触发的配置重载
  一致）。

---

## 3. 引导配置 schema

### 3.1 声明

```toml
[secrets]
# 可选：未写 kind 的来源条目默认用这个；默认 "env"。
default_kind = "env"

[[secrets.sources]]
name = "console-token"
kind = "env"
variable = "ACB_CONSOLE_TOKEN"

[[secrets.sources]]
name = "snowluma-token"
kind = "env"
variable = "ACB_SNOWLUMA_TOKEN"

# 一文件一值；适合 agenix 的单 secret 文件
[[secrets.sources]]
name = "dsh-dev-token"
kind = "file"
path = "/run/agenix/acb-dsh-dev.token"
format = "raw"

# KEY=VALUE 文件；兼容现有 acb.env / ntfy.env
[[secrets.sources]]
name = "ntfy-topic"
kind = "file"
path = "/run/agenix/ntfy-acb-topic.env"
format = "dotenv"
key = "NTFY_ACB_TOPIC"
```

字段规则：

| 字段 | 适用 kind | 规则 |
|---|---|---|
| `name` | 全部 | 全局唯一；`[A-Za-z0-9_.-]+`，非空 |
| `kind` | 全部 | `env` / `file`；未知 kind 启动失败并列出已知 kind |
| `variable` | `env` | 必填；`[A-Za-z0-9_]+`；不允许 `file/path/format/key` 字段 |
| `path` | `file` | 必填；**必须是绝对路径**；`~` 不展开；不允许 `variable` |
| `format` | `file` | `raw`（默认）/ `dotenv`；其它值报错 |
| `key` | `file` + `dotenv` | 必填；`raw` 下出现即报错 |

`sources` 条目用 `deny_unknown_fields`，拼错字段直接报错，不静默忽略。

### 3.2 引用

```toml
[console]
listen = "127.0.0.1:3100"
token = { secret = "console-token" }

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
transport = "ws-client"
url = "ws://127.0.0.1:53001/ws"
token = { secret = "snowluma-token" }

[notifications]
enabled = true
url = "http://127.0.0.1:8080"
topic = { secret = "ntfy-topic" }

[[sessions]]
id = "dsh-dev"
harness = "dsh"
persona = "default"
token = { secret = "dsh-dev-token" }
```

规则：

- 密钥字段只接受 `{ secret = "<name>" }` 或省略（可选字段）；写普通字符串视为
  「字面量密钥」，按 §4.3 的迁移策略处理。
- 引用未知 name：配置错误，报错里带 name。
- 同一个 name 可以被多个字段复用（例如两个会话共用 opencode token）。
- `url` 里自带 `?token=xxx` 的写法**不纳入**来源抽象：请把 token 拆到
  `token` 字段，URL 里的查询串保持非敏感；启动期检测到 `url` 查询参数名为
  `token`/`access_token` 时给出警告（不阻断，避免误伤）。

### 3.3 作用域：全局声明 + 使用点决定作用域

决策：**只做一层全局命名空间**。

- 连接的密钥引用写在 `[[connections]]` 里；会话的写在 `[[sessions]]` 里；
  控制台/通知写在各自的全局表里。这就是「按连接 / 按 harness（会话）」的实际
  作用域。
- `Harness` 结构自身只有 `id` / `name`（`config.rs:595-600`），桥不保存 harness
  的连接地址或内部配置；harness 是反向连进来的，校验的是**会话 token**。因此
  不存在「harness 级密钥」，只有「会话级接入 token」。
- 不增加 `[connections.secrets]` / `[harnesses.secrets]` 子表：connections /
  sessions 是数组条目，分片合并时整条追加，per-entry 子表会让跨文件合并语义
  更复杂；而全局表 + 命名约定（`<connection-id>-token`、`<session-id>-token`）
  已经够用。
- 将来若需要强制隔离，可在 source 上加 `scope = "connection:qq"` 之类字段；
  v1 不做。

### 3.4 分片与 `90-console.toml`

- `[[secrets.sources]]` 允许出现在 base 或任意 `config.d/*.toml` 分片。
- 合并规则要新增 `merge_secrets`：`sources` 数组追加，`default_kind` 后者覆盖；
  合并后跨文件查重 name，重复即 `DuplicateAcrossFiles` 风格错误。
- `90-console.toml`（控制台覆盖层）**不允许**出现 `[secrets]`：控制台只引用
  已有 name，不创建来源。出现即报错，避免「网页表单能改密钥来源」这种隐式提权。
- `LoadConfig.merged_toml` 保持原文（含 ref、不含值），Steward / process-provider
  等独立解析器看到的仍是 ref。

### 3.5 校验时机

`Config::load` / `Config::load_with_console_overlay` 在 `finish_loaded` 阶段：

1. 校验 schema（kind、必填/互斥字段、name 格式、跨文件重名）；
2. 解析**全部**声明的 source（不因为「没人引用」就跳过——声明坏了就应该报）；
3. 校验所有 ref 都能命中 source；
4. 把值填进 `Config`。

控制台热重载走同一路径：新配置里密钥读不到时，保存被拒绝，旧配置继续生效。

---

## 4. `${NAME}` / `${{NAME}}` 怎么退场

### 4.1 推荐：同一个改动里直接删除，不留兼容层

理由：

- 插值发生在 TOML 解析前，只要它还在，任何字段（包括 token）都还能从环境变量
  拿值，fail-closed 和「密钥只走来源」的约束就形同虚设；
- 保留一个版本的兼容意味着要同时维护两套解析路径，违背「一次性迁移」；
- 删除后的报错很直接：配置里出现 `${` 会变成 TOML 字面量或字段类型错误，
  迁移时一次就能发现。

删除内容：

- `expand_env_vars`、`escape_toml_string`；
- `Config::parse` 和 `parse_config_file` 里的展开调用；
- `config.rs:3270-3308` 与 `config.rs:5025-5031` 的插值单测；
- `docs/guide/config.md:11-42` 的「环境变量注入」章节。

### 4.2 对已有配置的影响面

| 用途 | 现在 | 迁移后 |
|---|---|---|
| 连接 token / 会话 token / console token | `${ACB_*}` | `{ secret = "<name>" }` |
| 通知 topic / token | `${NTFY_ACB_TOPIC}` 等 | `{ secret = "<name>" }` |
| `endpoint.listen`、`console.listen`、`file_relay.listen/public_base/dir` | `${ACB_*_LISTEN}` | 字面量，或由 Nix 生成到配置分片 |
| `storage.path`、`worktrees_root` | `${...}` | 字面量，或 Nix 生成 |
| `delivery_limit`、`turn_timeout_secs`、布尔量 | `${{...}}` | 字面量 |
| session `model` | `${ACB_MODEL_DSH}` | 字面量（模型名不是密钥） |
| 测试 fixture | `${PATH}` 等 | 删除插值测试；密钥 fixture 改用来源 |

仓库内 `config.toml` 本身没有 `${}`（都是字面量），所以仓库内影响主要是文档、
示例和 4 个单测；**真正的影响在生产部署的 nix-config 生成链路**：它需要把
`ACB_ENDPOINT_LISTEN` 等端口/model/path 直接写成具体值（Nix 已经知道这些值），
或者写进 `config.d/` 分片。

> 注意：无 systemd 路径今天连「非密钥插值」也是坏的——`env -i` 同样丢掉
> `ACB_ENDPOINT_LISTEN`。删除插值后两端行为反而一致。

### 4.3 字面量 token：已确认立即硬拒绝

仓库里有 **65 处** `token = "..."` 形态（38 个文件，绝大多数是测试 fixture），
另外控制台表单和 `start-drivers.sh` / `acb-up.sh` 都假设配置文件里有明文 token。

已确认走**严格路径（A）**，不设兼容窗口：同一次改动里拒绝字面量 token，所有
fixture、控制台、脚本一起迁移。配置文件里永远没有密钥本体，从第一天成立。
测试 fixture 用 `Config::parse_fixture` / `Config::load_fixture`：只允许
`{ secret = "<name>" }`，未声明的来源按名字自身解析；生产路径仍然严格 fail-closed。

---

## 5. `file` 来源细节

### 5.1 文件格式

**raw（默认）**：整个文件就是一个值。

- 只剥掉末尾一个 `\n` 或 `\r\n`；不 trim 空格（避免静默改变 token）；
- 剥掉后空串或纯空白 → 错误；
- 含 NUL → 错误；
- 文件大小上限 64 KiB（防误指到大文件）。

**dotenv**：兼容现有 agenix env 文件。

- 逐行：空行、`#` 开头注释忽略；
- `KEY=VALUE`，KEY 两侧空白忽略；VALUE 两侧空白忽略；
- 不做引号处理：`KEY="value"` 读出来的值就是带引号的 `"value"`。
  这是已确认的严格子集（不做转义、不做续行、不做变量展开）；
- 重复 KEY → 错误（不静默取最后一个）；
- 缺 `=` 的非注释行 → 错误；
- 找不到 `key` 指定的键 → 错误；
- 空 value → 错误。

不实现 shell 求值（`export`、`$()`、反引号、行内 `#` 注释），避免把配置文件
变成脚本。

### 5.2 路径

- **只接受绝对路径**；相对路径直接报错。理由：`Config::parse` 没有配置文件
  路径上下文，cwd 又不可控；绝对路径让错误可定位、行为可复现。
- 允许符号链接（agenix 常见），但校验针对**解析后的目标**。
- 只接受普通文件；目录、FIFO、设备文件一律拒绝（避免阻塞或语义不明）。

### 5.3 权限与归属校验

在 Unix 上：

- 用 `File::open(path)` 拿到句柄后，对**同一个句柄**取 metadata 并读取内容，
  避免 `stat` 后重新 open 的 TOCTOU；
- `mode & 0o077 != 0` → 错误（不允许 group/other 任何权限，要求 `0400`/`0600`）；
- owner 必须是「当前进程 euid」或 `0`；进程以 root 运行时要求 owner 为 `0`；
- 不符合时错误信息包含 name、path、实际 mode/uid，但**不包含值**。

非 Unix 平台跳过归属/权限检查（文档注明）。

### 5.4 空值与错误语义

| 条件 | 结果 |
|---|---|
| env 变量未设置 | 配置错误（`MissingSecretEnv`） |
| env 变量为空串 | 配置错误（同上，视为未设置） |
| 文件不存在 / 无权限打开 | 配置错误（`BadSecretFile`） |
| 非普通文件 | 配置错误 |
| 权限/属主不合规 | 配置错误（`InsecureSecretFile`） |
| 文件为空 / 仅空白 | 配置错误（`EmptySecret`） |
| dotenv 语法错误 / 缺 key / 重复 key | 配置错误，带行号或 key |
| 未知 kind / 缺必填字段 / 字段互斥冲突 | 配置错误 |
| ref 指向未知 name | 配置错误（`UnknownSecret`） |
| name 重复（跨分片） | 配置错误（`DuplicateSecret`） |

一律不打印值；启动摘要只打印「N 个来源、M 个引用、kind/path」。

---

## 6. 修改落在哪些文件

### 6.1 core（`crates/kernel`，必须改）

- `crates/kernel/src/config.rs`
  - 删除插值：`expand_env_vars`、`escape_toml_string`、两处调用、四个单测；
  - 新增 `RawConfig.secrets: Option<RawSecrets>`；
  - 新增 `RawSecretSource`、`RawSecretValue`（untagged：`String` | `{ secret }`）；
  - `RawConnection.token`、`RawSession.token`、`RawConsole.token`、
    `RawNotifications.token/topic` 改成 `RawSecretValue` / `Option<RawSecretValue>`；
  - 新增 `SecretStore` / `resolve_secret`（env、file raw、file dotenv）；
  - 新增 `merge_secrets`（`sources` 追加、`default_kind` 覆盖）；
  - `apply_console_overlay` 拒绝 `secrets` 键；
  - `check_duplicate_ids` 增加 source name 跨文件查重；
  - `from_raw` 在构造 `Config` 前解析所有 source 与 ref；
  - 错误类型新增 `ConfigError::*Secret*`。
- `crates/kernel/src/lib.rs`：按需导出新类型（`SecretRef` 等，若外部要用）。
- `crates/i18n/locales/zh-CN.json` / `en.json`：新增错误与启动摘要文案
  （仓库现有约定：错误文案走 i18n，不硬编码中文）。

如果决定给 token 加 newtype（防 `Debug` 泄露），消费点只有 §1.2 列出的
`link.rs` / `endpoint.rs` / `console.rs` / `notifier.rs` 四处；默认先保持
`String` 以降改动量。

### 6.2 bridge（`crates/bridge`）

- `main.rs`：启动摘要里打印 source 数量/kind（不打印值）；错误映射到 78 已存在
  （`EXIT_CONFIG = 78`，见 `main.rs:55, 83-88`）。
- `console.rs`：
  - 服务端视图继续遮掉 token（已有 `mask_secret`，:478/:504）；
  - 由于解析后 `Config` 里只剩值、没有 name，控制台编辑视图应改从
    `merged_toml`（控制台本来就用它做 patch）读取 `{ secret = "<name>" }` 并展示
    name，而不是展示遮掉的值；运行时仍用 `Config` 里的解析值；
  - 表单保存路径在严格模式下拒绝字面量 token；兼容模式下保留但返回弃用提示。
- `config_patch.rs`：`session` 新建（:116-135）/ `connection` 新建（:404-408）的
  token 参数改为 `{ secret = "<name>" }` 或 `secret_name`；错误文案指向 §3.2。
- `console.html`：把「令牌」输入框改成「密钥名」输入（严格模式）；兼容模式
  保留现有输入并显示弃用提示。

控制台的 UX 取舍：

- 推荐（严格模式）：控制台**不写密钥本体**，也不创建 source；新建会话要求
  运维先准备好 secret 文件/环境变量并声明 name。网页里只填 name。
- 兼容模式（v1）：保留现有「生成随机 token 写进配置」的行为，但标记弃用；
  v2 前必须改成 name。

### 6.3 CLI（`crates/acb`，建议，不是最小必需）

- `acbctl secrets check --config <path> [--config-dir <dir>]`：
  加载配置并解析所有来源，成功打印 `name kind origin`（不含值），失败非零退出。
  用于迁移验证、无 systemd 路径验证。
- `acbctl secrets resolve <name> --config <path>`：
  把值打印到 stdout，供 `deploy/start-drivers.sh` / `deploy/acb-up.sh` 取 token。
  文档注明「输出即敏感，禁止重定向到日志」。
- 注意：`acbctl grants` 等现有命令走 `Config::load` 后会开始解析密钥；这是
  fail-closed 的自然结果，需在帮助文档里说明。

### 6.4 部署与示例

- `deploy/start-drivers.sh:57-65`：删掉 awk 提取 token；改为按会话名
  `acbctl secrets resolve`（或从约定的 name 读）。
- `deploy/acb-up.sh:106`：console token 同理。
- `deploy/units/systemd/*.tmpl`：
  - `acb-bridge` / `acb-launcher` 的 `EnvironmentFile` 在「全部切 file 来源」后
    可以去掉；在此之前保留（env 来源仍需要）；
  - `dsh-acb` 的 `EnvironmentFile` **必须保留**（它要 `NEW_API_*` / `ACB_MODEL_KEY`）。
- `deploy/acb.env.example`：注明哪些键现在只给 dsh/opencode 用，哪些已迁到
  `[secrets]`。
- `examples/deploy/no-systemd-run.sh`：不再假设桥从 acb.env 拿密钥；保留
  `ACB_UNIT_*` 白名单，注释说明 file 来源由桥直接读。
- `examples/contract` 测试不变（不新增非白名单导出）。

### 6.5 文档与示例

- `docs/guide/config.md`：删除「环境变量注入」，新增「密钥来源」章节；
- `config.example.toml`：把 `token = ""` / `"change-me"` 改成 ref 示例；
- `docs/plan/nix-config-migration.md`、`docs/plan/config-storage-audit.md`：
  更新密钥清单的口径（只列 name/kind/path）。

### 6.6 外部仓库（nix-config，迁移前置）

不在本仓库，但必须同步，否则生产配置里的 `${ACB_*}` 会直接解析失败：

- `acb.env.age`、`ntfy-acb-topic.env.age` 保留；
- 生成 `config.active.toml` / 配置分片时：
  - 端口/model/path 写具体值；
  - token/topic 写 `{ secret = "..." }`，并在 base 或分片里声明对应
    `[[secrets.sources]]`（先用 `kind = "env"`，与 `EnvironmentFile` 一致）；
- 之后可选择把 `kind = "env"` 换成 `kind = "file"`，指向 `/run/agenix/*`。

---

## 7. 无 systemd 路径

现状问题：`no-systemd-run.sh` 先 source `deploy/acb.env`，然后用
`env -i "${EXPORTS[@]}"` 只导白名单 `ACB_UNIT_*`，桥进程拿不到
`ACB_SNOWLUMA_TOKEN` 等变量，`${...}` 插值必然失败。

新模型下的处理：

1. **file 来源不需要 env**：桥进程自己 `File::open("/run/agenix/...")`（或用户
   0600 文件），`env -i` 不影响。无 systemd 部署改用 `kind = "file"` 即可。
2. **env 来源仍然依赖进程环境**：无 systemd 下要用 `kind = "env"`，就必须真正
   把变量传进 `acbctl` 子进程（当前白名单不允许）。这是预期内的 fail-closed，
   不是 bug；文档写清。
3. 不需要给 `ACB_UNIT_*` 白名单加密钥键；也不建议加——那等于把密钥重新变成
   环境变量。
4. 验证：用 `env -i` 只带 `ACB_UNIT_*` 起 `acbctl run`，配置里用 file 来源，
   确认桥能加载并连上平台。

边界：launcher 的 `spawn_child` 不 `env_clear`（`run.rs:375-397`），所以
systemd 路径下现有 `EnvironmentFile` 仍会到桥进程；这与 file 来源并存不冲突。

---

## 8. 迁移与验证

### 8.1 迁移顺序（先行为不变，再换来源）

1. **配置引用化，来源仍用 env**：
   - `config.toml` 里 `token = "${ACB_SNOWLUMA_TOKEN}"` →
     `token = { secret = "snowluma-token" }`；
   - 加 `[[secrets.sources]] kind = "env"`；
   - `EnvironmentFile` 保留。
   - 此时行为与旧版完全一致（同一个环境变量、同一个值），但配置里不再有插值。
2. **验证一致性**（见 §8.2）。
3. **无 systemd 部署切换为 file 来源**：
   - 把 agenix 文件按「一文件一值」拆成独立 secret，或继续用 `dotenv + key`；
   - 确认运行用户能读（权限/属主校验通过）。
4. **最后再考虑移除桥单元的 `EnvironmentFile`**（dsh 单元的必须保留）。

### 8.2 验收清单

正向：

- [ ] `acbctl secrets check` 输出所有 name/kind/path，不含值；
- [ ] 桥启动成功，控制台用原 token 登录成功，`/api/status` 正常；
- [ ] SnowLuma WS 握手成功（`Authorization` 头用的是解析后的 token）；
- [ ] 至少一个 harness 驱动用原会话 token 连上并完成一轮；
- [ ] ntfy 通知能发出（topic/token 都走来源）；
- [ ] 控制台保存（热重载）后行为不变；配置分片/覆盖层合并顺序不变；
- [ ] 无 systemd：`env -i` + file 来源能启动并连上。

负向（fail-closed）：

- [ ] env 变量未设置 / 为空 → 桥以 78 退出，systemd 不进重启循环；
- [ ] 密钥文件缺失 / `chmod 644` / 属主不对 → 以 78 退出，日志指出 name+path；
- [ ] ref 写错 name → 配置错误；
- [ ] 两个分片声明同名 source → 配置错误；
- [ ] 控制台覆盖层写 `[secrets]` → 保存被拒绝；
- [ ] 空文件 / 空 dotenv 值 → 配置错误，不会变成空串；
- [ ] 配置里再写 `${...}` → 不会被展开（按字面量/类型错误失败）。

### 8.3 回滚

- 保留旧 `config.active.toml` 和旧二进制；新配置解析失败时服务本来就不会
  切过去（78 退出）。
- 回滚只需恢复旧配置 + 旧版本二进制；旧版本仍支持 `${}` 和 `EnvironmentFile`。
- 密钥文件本身不删；回滚不影响 agenix。

---

## 9. 分期与待确认决策

### 9.1 分期

- **P1（本设计确认后，一次改动）**：
  - 删除插值；
  - `[secrets]` schema + env/file 两种来源 + 解析 + 错误；
  - 配置分片合并、控制台覆盖层规则；
  - 单测：env、file raw、file dotenv、权限、缺失、重名、未知 ref、合并；
  - 文档/示例；
  - 生产部署先用 `kind = "env"` 切换（行为不变）。

- **P2（同一次改动）**：
  - 无 systemd / 单机部署切 `kind = "file"`；
  - `deploy/start-drivers.sh`、`deploy/acb-up.sh` 改为 `acbctl secrets resolve`；
  - `acbctl secrets check`。

- **P3（后续，非本设计）**：
  - 再讨论「把桥解析出的密钥注入子进程环境」提供给 dsh/opencode（默认不做）。

### 9.2 已确认的四个点

1. **字面量 token 的兼容窗口**：A（立即硬拒绝，fixture、控制台、脚本一次改完）。
2. **`file` 是否要支持 `dotenv`**：支持，但只收严格子集（逐行 KEY=VALUE、
   `#` 整行注释、空行；不处理引号/续行/变量展开）。
3. **绝对路径限定**：只收绝对路径，相对路径算配置错误。
4. **无 systemd 的目标范围**：只覆盖桥自己消费的密钥；dsh/opencode 的 API key
   仍由各自的环境/EnvironmentFile 提供。
