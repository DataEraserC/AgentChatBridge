# 配置来源审计（F1 前置）

状态：2026-11-09。对象是**当前这台真机**（`~/.local/share/acb/`）+ 这个 commit 的代码。
只查事实与影响，不改行为；结论用于决定 F1 第一步切哪里。
设计依据是 [acb-policy-store.md](../design/acb-policy-store.md)（「控制台写库，`90-console` 只做
export/seed」），本页不重复设计理由，只回答「今天实际是谁说了算」。

## 0. 结论（五条）

1. **真源是 `config.d/90-console.toml`，不是 `config.active.toml`。** 覆盖层含 14 个顶层键里的
   13 个（除 `[secrets]`），另加 `[tickets]`；合并后 base 只剩 `[secrets]` 一棵树，普通分片只剩
   `80-issue-tools.toml` 的 `[[tool_plugins]]` 有贡献。运维改 base 里的连接/会话/人格 = 白改。
2. **覆盖层按顶层 key 整体替换**（`apply_console_overlay`），不做 id 级合并、不做数组追加。
   被覆盖对象不会报错、不会警告、也不会出现在任何「已忽略」列表里。
3. **这条语义已经在真机上吞掉了权限，不是纸面风险**：`70-grants-issue.toml` 的 9 条 `plugin`
   规则自覆盖层出现起就没进过内存（`--check` 报 `effective rules: 1`）。F3 首次导入照抄了这份
   残缺策略（日志 `[policy] 首次导入完成：allow=1 deny=0 rooms=7`），此后实测 **12 次
   `admin:no_plugin_grant`** 拒绝：system_admin 在群里和私信里都调不动插件工具。
4. **写入面只剩 4 条，其中 2 条已经是库**：`/api/policy`、`/api/model-fallbacks` → 库；
   `/api/config/toml`、`/api/config/patch` → 覆盖层。策略（F3）和模型链（F7）已单源，
   连接 / 会话 / 人格 / 记忆 / harness 还挂在文件上。
5. **影响清单有洞**：`[[tool_plugins]]` 改了要重启但不提示；`notifications` 不提示；
   `[grants]` 现在改 TOML 完全无效也不提示（它只是库的导入源）。

## 1. 加载链（实测）

`acb-bridge --check --config ~/.local/share/acb/config/config.active.toml` 的原样输出：

```text
config source: .../config.active.toml                 (connections 1, sessions 6, rules 1)
config source: .../config.d/70-grants-issue.toml      (connections 0, sessions 0, rules 9)
config source: .../config.d/80-issue-tools.toml       (connections 0, sessions 0, rules 0)
config source: .../config.d/90-console.toml           (connections 1, sessions 6, rules 1)
effective rules: 1
```

四个来源、声明了 1+9+0+1 = 11 条规则，**生效 1 条**。`/api/status` 的 `config_fragments` 报同一组
数字（`rules` 也是 1）。

## 2. 合并语义（代码事实）

| 来源 | 语义 | 实现 |
|---|---|---|
| base + `config.d` 普通分片 | 字典序加载；数组追加；表/标量后出现者覆盖 | [merge_document](../../crates/kernel/src/config.rs#L3949)、[merge_entry](../../crates/kernel/src/config.rs#L3970) |
| `grants.rules` / `deny` | 追加 | [merge_grants](../../crates/kernel/src/config.rs#L3983) |
| `grants.rooms` / `accounts` | 按键合并，同键后者覆盖 | 同上 |
| `90-console.toml` | **顶层 key 整体替换**；写了 `[grants]` 则下层 grants 全消失 | [apply_console_overlay](../../crates/kernel/src/config.rs#L4054) |
| 覆盖层豁免 | 不与下层做重复 id 检查，只查自身内部 | [config.rs:1531](../../crates/kernel/src/config.rs#L1531)、[1566](../../crates/kernel/src/config.rs#L1566) |

「豁免重复 id 检查」这条是覆盖层**存在的原因**：控制台保存的是整份文档，不豁免就会和 base 里
同名 id 冲突，控制台连已有会话都编辑不了（[docs/guide/config.md](../guide/config.md) 也这么写）。
代价就是上面第 2 条：谁后写谁把整类对象端走。

## 3. 写入面清单

| 入口 | 落点 | 生效时机 | 覆盖对象 |
|---|---|---|---|
| `POST /api/config/toml`（编辑器） | `config.d/90-console.toml` | 轮次边界 | 文本里出现的全部顶层 key |
| `POST /api/config/patch`（结构化表单） | 同上（当前文档 = 覆盖层） | 轮次边界 | session / subscription / persona / memory / harness / connection / global 八个 op |
| `POST /api/policy` | 库（`policy_*` 四表） | 轮次边界 | grants 全集 |
| `POST /api/model-fallbacks` | 库（`model_fallbacks`） | 轮次边界 | 模型后备链 |
| `POST /api/plugins/{reload,unload}` | 仅运行时 | 立即 | 不落盘 |
| `POST /api/session/{id}` | 仅运行时（重置映射） | 立即 | 不落盘 |
| 人手改 base / `config.d/*` | 文件 | **重启** | 除覆盖层已占的 key 外 |

`console_document()`（[console.rs:198](../../crates/bridge/src/console.rs#L198)）在覆盖层存在时
就展示覆盖层、补丁也打在覆盖层上 → 控制台「配置」页看到的那份 TOML **不是** `config.active.toml`。

## 4. 对象 × 真源矩阵

| 对象 | 今天谁说了算 | 备注 |
|---|---|---|
| `connections` | 覆盖层（与 base 内容相同） | 改了要重启；`/api/status` 的 connections 取自合并结果 |
| `sessions` | 覆盖层 | 同上；F2 要做的事 |
| `personas` | 覆盖层 | 热重载 |
| `memories` | 覆盖层 | 热重载 |
| `harnesses` | 覆盖层 | 热重载 |
| `grants` | **库**（启动时同步，覆盖层无效） | 文件只剩「首次导入源」；改文件不生效且不报错 |
| `model_fallbacks` | **库** | 全局，无 per-session |
| `tool_plugins` | `80-issue-tools.toml` | 只在启动时注册（[main.rs:213](../../crates/bridge/src/main.rs#L213)） |
| `notifications` / `in_band` | 覆盖层 | 每轮读，天然热；但 `reload_impact` 不提示 |
| `tickets.pending_ttl_secs` | 覆盖层 | 只在这里存在，base 没有 |
| `secrets` | base（唯一） | 覆盖层写 `[secrets]` 直接被拒（`ConsoleSecretSource`） |
| `endpoint` / `console` / `storage` / `worktrees_root` / `file_relay` | 覆盖层 | 改要重启（`file_relay.listen` 单独一条） |
| `retention` / `delivery_limit` / `turn_timeout_secs` / `default_on_turn_error` | 覆盖层 | 热重载 |
| 会话 token / 连接 token | base 的 `[secrets]` 引用 + `secrets/*.env` / agenix | 不进覆盖层 |

## 5. 实测证据

**E1 覆盖层与 base 内容完全相同（0 个叶子键不同）。** 除 `[secrets]`（base 有、覆盖层没有）和
`[tickets]`（覆盖层有、base 没有）外，13 个顶层键逐叶子比对无差异。也就是说这份覆盖层是「整份
配置的副本 + 一处 drift 空间」，两份文件靠人手同步，随时会分叉。

**E2 覆盖层把 9 条规则吃掉了。** `--check` 报 `effective rules: 1`；
`/api/status` 的 `rules` 也是 1；而 `70-grants-issue.toml` 声明 9 条。原因是覆盖层写了
`[grants]` → 整棵 grants 被替换成覆盖层里那 1 条（`base-system-admin-private`）。

**E3 新增分片完全无效（审计探针）。** 在 `/tmp` 的配置副本里加
`config.d/85-probe.toml`（1 个新会话 `audit-probe`）：

```text
config source: /tmp/acb-audit/config.d/85-probe.toml   (connections 0, sessions 1, rules 0)
...
sessions: 6            # audit-probe 不在里面
effective rules: 1
```

来源列表承认这个文件被读到了（sessions 1），生效配置里却没有它——因为覆盖层的
`[[sessions]]` 整组替换了合并结果。

**E4 权限损失已经在真实消息里发生。**

```text
[policy] 首次导入完成：allow=1 deny=0 rooms=7
[access] subject=system_admin place=group   capability=plugin target=房间 qq:group/769843907 result=deny basis=admin:no_plugin_grant
[access] subject=system_admin place=private capability=plugin target=房间 qq:private/323622442 result=deny basis=admin:no_plugin_grant
[access] subject=system_admin place=group   capability=plugin target=房间 qq:group/384353273 result=deny basis=admin:no_plugin_grant
```

`bridge.log` 全文件 52 次 `no_plugin_grant`，其中 **12 次发生在策略入库之后**，涉及 4 个房间
（含管理员私信）。判据在 [policy.rs:84](../../crates/kernel/src/policy.rs#L84)：admin 的
`plugin` 能力没有 base floor，必须靠显式规则；规则没了就是全拒。

**E5 库里的策略确实只有 1 条。** `policy_rules` = 1、`policy_rooms` = 7、`policy_admins` = 1、
`model_fallbacks` = 3、`meta.policy_initialized` = 2026-11-09 13:40。7 个房间正好是覆盖层那 7 个
（比 base 多一个 `qq:group/769843907`），进一步说明首次导入用的是覆盖层那份。

## 6. 对 F1 的切割（建议，未执行）

**第一步（止血，不动表、不动合并语义）**：把 plugin 授权补回库。用 `/api/policy` 加**两条**
规则即可覆盖现有 9 条房间规则——`subject=system_admin`、`scope=subscribed`、`capability=["plugin"]`，
`place` 分别取 `group` 与 `private`（`subscribed` 的可达性就是「本会话订阅的房间」，
见 [grants.rs:862](../../crates/kernel/src/grants.rs#L862)）。随后把 `70-grants-issue.toml` 的
`[grants]` 删掉，否则它会继续当「看起来生效、实际无效」的假来源。

**第二步（F1 本体）**：让控制台不再产出覆盖层。两条路，改动量差一个量级：

- A. 覆盖层改「按 id 合并的片段」（`[[sessions]]`/`[[connections]]`/`[[personas]]` 按 id
  替换或追加，删除走 `delete` op 显式标注）。改的是 `apply_console_overlay` 一处 + 补测试，
  文件仍是真源。
- B. 控制台写库（连接/会话/人格建表），文件降级为 export/seed。这是设计文档的终局，
  但等于把 F2 提前，且要解决「库改完怎么重启驱动」。

**第三步**：`90-console.toml` 归档（留 `.bak`），base 重新成为唯一文件真源；
`config_fragments` 在 UI 上标出「被上层覆盖」，别再让运维对着被覆盖的分片调半天。

## 7. 需要用户裁定

1. 止血的 plugin 规则要「两条 subscribed」还是「照原样 9 条 room」？（前者覆盖面更大，
   后者与历史一致）
2. F1 走 A（改合并语义）还是 B（直接写库）？A 是一天量级，B 要连带 F2。
3. 覆盖层里 `tickets.pending_ttl_secs = 1800` 这条要迁到哪：base、库、还是保持控制台独占？

## 8. 落地（2026-11-09 晚，用户裁定走 B、不止血）

裁定：**F1 走 B —— 控制台直接写库**；不做临时补规则（`F1a` 挂起）。因此这一节
记录「已经改了什么」与「真机上还差哪几步」。

### 8.1 代码（schema 15）

| 位置 | 改动 |
|---|---|
| `crates/kernel/src/config_objects.rs`（新） | 五类对象的形状：`ConfigObjectKind` / `ConfigObject{id, toml}` / `ConfigObjects`；`from_merged` / `splice_into` / 渲染与解析 |
| `crates/kernel/src/storage.rs` + `storage/sqlite.rs` | `ConfigObjectsStore` trait、`config_objects` 表、`meta.config_objects_initialized` 标记 |
| `crates/kernel/src/config.rs` | `load_with_objects` / `load_with_console_overlay(.., objects)`：合并完覆盖层后用库里的五类数组**整类替换** |
| `crates/bridge/src/config_objects.rs`（新） | 启动对齐（首次导入一次 / 之后以库为准）、覆盖层文档渲染与回拆、已入库键识别 |
| `crates/bridge/src/main.rs` | 开库提前到配置加载之后；先 `sync` 再装配适配器/依赖表 |
| `crates/bridge/src/console.rs` | 补丁分两组：对象类写库、`global` 写覆盖层；TOML 编辑器拒绝已入库键；纯对象改动不碰任何文件 |

存的是**每一条的 TOML 原文**，不是结构体 JSON。这样解析、默认值、`token = { secret = ... }`
的密钥引用全走原来那条路——密钥真值仍然只在 env/agenix，库里只有引用名。

### 8.2 真机迁移步骤（待执行）

1. 换代（schema 15）：`profile add` → `switch` → 重启桥；日志应出现
   `业务对象首次导入完成：连接 1、会话 6、人格 2、记忆 2、harness 2`。
2. 核对：`/api/config` 与旧值逐项对比；`--check` 的 `effective rules` 不受影响（仍是 1）。
3. 把 `[tickets] pending_ttl_secs` 挪进 base（覆盖层里只有这一条是 base 没有的），
   然后把 `90-console.toml` 归档到 `config.d/history/`（改后缀，加载器自然忽略）。
4. 再重启一次，确认启动日志变成 `业务对象取自库：…`，且不再出现「配置里还有已入库的段」
   之外的差异。
5. 归档后 `config.active.toml` 可以删掉那五段（留着也行：会被忽略并打警告）。

### 8.3 遗留

- 策略侧那 9 条 `plugin` 规则仍然不在库里（`F1a` 挂起，用户裁定不止血）。真机验收后
  建议一次性补录——现在补是「迁移收尾」，不是止血。
- 会话增删之后驱动不会自动重启（`acb-drivers` 仍按启动时的会话表起进程），这条是
  F2 的原生前置，未在本轮解决。
