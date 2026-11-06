# ACB 配置组合与授权模型（调研记录）

状态：2026-11-06。**已被 [docs/design/acb-policy-store.md](../design/acb-policy-store.md) 取代**；
本页保留 cordis patch 调研、事故复盘与早期模型，不作为实现依据。实现以
`acb-policy-store.md` 为准。

范围是 `config.d` 分片组合、`90-console.toml` 覆盖层和 `grants` 求值；不涉及票据存储、
审批通知和适配器。

---

## 0. 结论先行

- 今天 9 个群的故障不是“少了一条规则”，而是求值器把“规则匹配了、但没列出这个能力”
  当成否决：`evaluate_rules` 返回 `Some(deny)`，`evaluate_access` 只在返回 `None` 时
  才回退订阅边界。于是 plugin-only 规则把同房间的读写和票据申请一起压掉了。
- 目标模型是集合运算，不是规则优先级：

  ```text
  effective = (floor ∪ grants) − deny
  ```

  `floor` 是订阅/结构边界；`grants` 只做并集加法；`deny` 必须显式书写，且永远在并集之后
  相减。匹配但不列出某能力的规则，对那个能力既不授予也不否决。
- `cordis.patch.yml` 值得借的是“按稳定 id 寻址、显式标记、冲突要报错”的纪律，不是它的
  patch 语言。它没有删除语法，`disabled` 是行级不加载，`~` 是 Koishi 旧配置的 key 前缀，
  三者不要混为一层。
- `90-console.toml` 的文件名特权应改成片内 `[fragment]` 声明；`config.d` 默认组合不变，
  移除仍然只有“删片”一种语法，不引入隐式收回。
- 迁移分两步：先加 `grants.mode=additive` 和 `acbctl grants explain/matrix`，再把 70 片
  临时补的 `room_read/room_write/access_request` 删回 `["plugin"]`。删回后 9 个群仍正常，
  才算证明求值不再隐式重置。
- 删回还带来一个安全收益：未订阅该房间的会话不再因为显式房间规则拿到三项能力。当前
  四能力补丁在“未订阅”场景是越权，不是修复的终点。

---

## 1. 现状与事故复盘

### 1.1 三层组合/求值

| 层 | 位置 | 当前语义 |
|---|---|---|
| base + `config.d` 普通片 | [config.rs](../../crates/kernel/src/config.rs#L1291-L1355) | 按文件名字典序加载；数组追加；普通表/标量后出现者整体覆盖 |
| `90-console.toml` | [config.rs](../../crates/kernel/src/config.rs#L1303-L1311)、[config.rs](../../crates/kernel/src/config.rs#L3188-L3201) | 文件名特判，最后加载；它包含的每个顶层 key **整体替换**已合并值 |
| `grants` 求值 | [grants.rs](../../crates/kernel/src/grants.rs#L711-L792) | 顺序遍历；第一条“匹配且包含请求能力”的规则立即 allow；匹配但缺能力留下 deny 候选；只有完全没有规则匹配才回退 `base_access` |

`config.d` 的细粒度行为：

| 值类型 | 行为 | 实现 |
|---|---|---|
| 数组（`[[sessions]]`、`[[grants.rules]]` 等） | 追加 | [merge_entry](../../crates/kernel/src/config.rs#L3131-L3142) |
| 普通表/标量 | 后出现者整体覆盖 | 同上 |
| `grants.rules` | 追加 | [merge_grants](../../crates/kernel/src/config.rs#L3144-L3169) |
| `grants.rooms` / `grants.accounts` | 按键合并；同键后出现者覆盖 | [merge_keyed_table](../../crates/kernel/src/config.rs#L3171-L3186) |
| `90-console.toml` | 顶层 key 整体替换；若写了 `[grants]`，下层全部 grants 消失 | [apply_console_overlay](../../crates/kernel/src/config.rs#L3188-L3201) |

两个容易忽略的事实：

- `RawConfig` 没有 `deny_unknown_fields`（[config.rs](../../crates/kernel/src/config.rs#L2595-L2640)），
  顶层字段拼错目前是静默忽略；`grants` 内部则严格（`GrantsConfig` 有
  `deny_unknown_fields`）。这也解释了为什么 `[grants] bogus` 会在加载期报错，而顶层
  `fragement` 这种拼写不会。
- `load()` 和 `load_with_console_overlay()` 是两条并行实现，前者用于启动，后者用于控制台
  保存前校验（[console.rs](../../crates/bridge/src/console.rs#L598-L665)）。任何组合语义
  改动都必须让两条路径共用同一个组合函数，否则控制台预览和实际启动会漂移。

### 1.2 求值链为什么把 plugin-only 规则变成读写 deny

当前代码等价于：

```text
evaluate_rules(query):
  denied = None
  for rule in rules:
    if subject/place/scope match:
      if capability in rule.capability:
        return Some(allow rule:N)
      denied = Some(deny rule:N)      # 注意：只因为“匹配了”
  return denied                        # Some(deny) 或 None

evaluate_access:
  return evaluate_rules(...).unwrap_or_else(base_access)
```

问题在最后一行：`unwrap_or_else` 只在 `None` 时执行。只要有一条规则匹配了
subject/place/scope，哪怕它只授予 `plugin`，`evaluate_access` 也拿不到 `base_access` 的
订阅放行。`evaluate_access_request` 和 `evaluate_tool` 的管理员分支同样复用了这个
`evaluate_rules`（[grants.rs](../../crates/kernel/src/grants.rs#L727-L759)），所以票据申请
和插件工具都被同一条规则影响。

`scope_matches` 对 `scope="room"` 的匹配也不要求主体已订阅该房间，只比较 target
（[grants.rs](../../crates/kernel/src/grants.rs#L839-L868)）。这解释了为什么四能力临时补丁
会让未订阅会话也拿到该房间的读写：规则本身不查订阅。

### 1.3 9 个群的事故链

现场配置（本机）：

```text
/home/nixos/.local/share/acb/config/config.active.toml        rules 1   (rule:0, 私信基线)
/home/nixos/.local/share/acb/config/config.d/70-grants-issue.toml  rules 9
/home/nixos/.local/share/acb/config/config.d/80-issue-tools.toml   rules 0
effective rules: 10
```

启动摘要见 `/home/nixos/.local/state/acb/bridge.log` 第 802-805 行；`bridge running` 在
第 844 行。

70 片覆盖的 9 个房间：`158296748`、`973928169`、`498587179`、`962657331`、
`1134811973`、`769843907`、`1093015329`、`1124995534`、`384353273`。

9 条规则形状相同：

```toml
[[grants.rules]]
subject = "system_admin"
place = "group"
scope = "room"
target = { kind = "room", id = "qq:group/769843907" }
capability = ["plugin"]        # 事故前的值
```

合并后，`769843907` 是第 6 条 70 片规则；base 的私信规则占 `rule:0`，所以审计里是
`rule:6`。一次群内轮次请求 `get_history`（`room_read`）时：

```text
[access] subject=system_admin place=group capability=room_read
         target=房间 qq:group/769843907 result=deny basis=rule:6
         reason="rule:6 does not grant RoomRead for system_admin group" session=dsh-group2
```

`send_message`（`room_write`）同样：

```text
[access] subject=system_admin place=group capability=room_write
         target=房间 qq:group/769843907 result=deny basis=rule:6
         reason="rule:6 does not grant RoomWrite for system_admin group" session=dsh-group2
```

票据申请走 `evaluate_access_request`，命中同一条规则：

```text
[access-request] denied ... code=access_request_denied
                 detail="rule:6 does not grant AccessRequest for system_admin group"
```

最终工具错误是 `RoomNotSubscribed`（[engine.rs](../../crates/kernel/src/engine.rs#L6803-L6806)），
但这是统一错误码，不代表房间真的没订阅。`check_access` 在 deny 时统一返回它，真正的
原因只留在审计行的 `basis=rule:6` 和 `reason` 里（[engine.rs](../../crates/kernel/src/engine.rs#L6621-L6807)）。

9 个群全部中招，因为 70 片 9 条规则都是 plugin-only；不是单个房间的配置错误。

### 1.4 临时补丁与它的边界

当前临时把 9 条规则改成：

```diff
- capability = ["plugin"]
+ capability = ["room_read", "room_write", "access_request", "plugin"]
```

这能让现场恢复，但它做的是“让规则显式授予三项能力”，不是修求值器：

- 对已订阅会话：结果正确，但审计会显示 `basis=rule:6`，看不出哪部分来自订阅、哪部分
  来自规则。
- 对未订阅会话：`scope="room"` 不查订阅；只要工具允许把 target 指到该房间（至少票据申请
  可以跨房间），规则就会把三项能力授出去。这是越权面。
- 70 片的文件头注释仍写着“room read/write capabilities are intentionally unchanged”，
  临时补丁期间与内容不一致；删回 `["plugin"]` 后注释会自动恢复准确。

所以临时补丁应视为**迁移前的止血**，不是目标模型。

### 1.5 相邻问题：控制台保存不报告 grants 影响

[reload_impact](../../crates/bridge/src/console.rs#L1244-L1307) 会列出 sessions、harnesses、
personas、memories、retention、turn error、delivery、timeout、file relay 等影响，但没有
`grants`。控制台保存一份改了 grants 的配置时，返回的 `hot_reload` 和 `needs_restart`
都可能是空，实际却已经 `stage_config`。这会让“我改了什么、什么时候生效”变得不清楚。
建议在语义切换的同一批改动里把 `grants` 归类到 hot（下一轮生效）并给出提示。

---

## 2. 目标模型

### 2.1 公式

对一次查询 `(subject, place, target, capability)`：

```text
effective = (floor ∪ grants) − deny
```

- `floor`：结构边界给出的基线能力。
- `grants`：所有匹配的 allow 规则的 capability 并集。
- `deny`：所有匹配的 deny 规则的 capability 并集，最后相减。
- 顺序无关：不是“第一条匹配赢”，而是把所有匹配的规则都算进来再做集合运算。
- 一条规则匹配了 subject/place/scope，但 capability 列表里没有请求的能力：它对这个
  能力**什么都不做**，既不 allow 也不 deny。

### 2.2 floor 的来源

| 能力 | floor |
|---|---|
| `room_read` / `room_write` / `connection_read` / `platform` / `destructive` / `critical` / `account_read` / `account_secret` | `base_access`：target 是否被当前会话订阅可达（[grants.rs](../../crates/kernel/src/grants.rs#L794-L837)） |
| `plugin` | 普通主体：`base:plugin`，沿用上游 scope 检查；`system_admin` / `bot_admin`：floor 为空，必须由规则显式授予（[grants.rs](../../crates/kernel/src/grants.rs#L745-L759)） |
| `access_request` | 管理员在私信/控制台：`base:admin_private`；其余主体：订阅可达（[grants.rs](../../crates/kernel/src/grants.rs#L727-L741)） |

`trust` 现在只参与群内审批身份判定（[engine.rs](../../crates/kernel/src/engine.rs#L2817-L2843)），
不参与读写能力求值。目标模型里也不应把它偷偷变成读写 floor：如果以后要“不可信群禁止
某些能力”，应写成显式规则或显式的 floor 条件，而不是在 `base_access` 里加隐藏判断。
这一点列为待评审决策（§6.6）。

`destructive` / `critical` / `account_secret` 仍然受票据门槛约束；floor 允许不等于可以
直调，风险映射和票据检查保持现状。

### 2.3 grants

- 规则字段保持 `subject` / `place` / `scope` / `target` / `capability`。
- 新增 `effect = "allow" | "deny"`，默认 `"allow"`，保证旧配置不改也能解析。
- 可选 `id`：稳定标识，供 `grants explain`、审计和差异清单引用；同一份配置内必须唯一。
- allow 规则：匹配则把 `capability` 里列出的能力加入并集。多条 allow 规则相加，不互斥。
- `capability = []` 的 allow 规则是无意义配置，加载期报错；不允许“空列表表示全部”。
- deny 规则：匹配则把 `capability` 里列出的能力加入 deny 集合；`capability = []` 同样
  加载期报错。deny 不写 `effect` 以外的特殊语法。

### 2.4 deny

- deny 的优先级恒定：先算 `floor ∪ grants`，再减去 `deny`；不因规则顺序、分片顺序或
  `90-console` 层而变化。
- deny 可以减掉 floor 的能力（例如显式禁止某会话读某房间），但必须写出来；没有“匹配
  不全就默认拒绝”的隐式 deny。
- 审计：任何 deny 都写；任何有 grant 来源的 allow 都写；纯 floor allow 不写。建议
  `basis` 变成来源列表：
  - 纯 floor：`basis=base:subscription` / `base:not_subscribed`；
  - floor + 规则：`basis=base:subscription,rule:6`；
  - 显式 deny：`basis=deny:rule:12`。
  现有 `should_audit` 的判据相应改为“有 `rule:` / `deny:` 来源，或最终 deny”。

### 2.5 配置组合

- `config.d` 默认组合保持不变：数组追加、表/标量后出现者覆盖、`grants.rules` 追加、
  `grants.rooms/accounts` 按键合并。不引入通用 patch 语言。
- `90-console.toml` 的覆盖特权改为片内 `[fragment]` 声明（§4）。
- 移除只有“删片”一种语法：删除一个分片文件，或从 base 里删掉对应内容。不引入 `~`、
  `disabled`、`delete` 之类的隐式收回标记。
- grants 的“减”由显式 deny 规则表达，不由删片表达；删片会连同该片的 allow/deny 一起
  消失，这是配置来源的变化，不是求值器的隐式收回。

---

## 3. cordis 参照点

### 3.1 一手调研结果

`cordis.patch.yml` 不是 cordis 核心的文件约定，而是 DSH profile/bundle 层的约定。
上游 `@cordisjs/plugin-include` 的 patch 机制是 `patches` 配置项（另有 path/initial 等普通
字段）；DSH 在
`@deepseek-ai/dsh-app-boot` 里把 bundle 层、profile 层、home 层、`--patch` 和遥测补丁
拼成一个扁平 patch 列表，再用同一个 `applyEntryPatches` 应用。

实际语义（按本机 dsh 0.2.1-rc.2 的源码）：

| 维度 | 实际行为 |
|---|---|
| 文件形态 | 顶层 YAML 数组；每项要么 `insert:`，要么按 `id` 覆盖已有条目 |
| 寻址 | 只按 `id`；嵌套 group 的条目递归建索引；不按路径、不按数组下标 |
| id 冲突 | 没有唯一性校验；后出现的同 id 会覆盖索引里的前一个 |
| 覆盖粒度 | 逐顶层字段赋值；`config` 整个对象替换，嵌套字段和数组都不深合并 |
| 追加 | `insert` 追加到根列表或指定 group 的 `config` 数组；同一列表里后面的 patch 能命中前面 insert 的行 |
| 删除 | 没有删除语法；只能 `disabled: true`（或 `!!js`）；group 行的 disabled 被忽略 |
| `~` | 不是 cordis 语法。Koishi 旧配置里它是 plugin map 的 key 前缀；YAML 里 `~` 是 null，Loader 的 entry 更新路径会清掉对应字段，但不删行 |
| 顺序 | 有影响：bundle 顺序 → profile patch → home patch（home 覆盖 profile）→ `--patch` argv → 遥测；同字段后者赢 |
| 未命中 | 只 warning 跳过；格式错误才启动失败 |
| 冲突 | 没有检测；多处 patch 同一 entry 时后者静默赢；可选的 `name` 断言只能发现名字不一致 |
| 生效 | 启动时组合；启用 dsh-hmr 时监听 profile manifest 和两个用户 patch 文件；web 模板 live reload，其他模板只在启动应用 |
| 错误定位 | 解析错误带文件路径；未命中只有 id；`--dump-config` 才按来源文件分组并报告未命中 |
| 只减不加 | 表达不了；只能 disable 行，或整段 `config` 替换 |

空文件或只有注释的 patch 文件会被 YAML 解析成 `null`，DSH 要求顶层数组，所以会启动失败；
要禁用这一层必须写 `[]`。这与我们的空分片是合法 no-op 不同，迁移时不要照搬。

没有找到官方 `koishi.patch.yml`；Koishi 4.x 的前身是单文件 `koishi.yml`：
`plugins` 是 map，key 前加 `~` 表示不加载，`$if` / `$filter` 是条目元数据；loader 会原地
重写配置文件，插件自 dispose 时把 key 改成 `~key`。它没有分层 patch，也没有
“保留下层、只覆盖一层”的概念。

### 3.2 采纳/不采纳

| 机制 | cordis/dsh | 我们的决定 | 理由 |
|---|---|---|---|
| 按 id 寻址 | 按 entry id | **直接采纳**，但只用于片/规则的稳定引用 | `grants explain`、审计、差异清单都需要稳定身份；不发展成任意字段 patch |
| 显式标记 | `disabled: true` / `insert` | **改造后采纳**：显式 `effect="deny"`；行删除用删片 | `disabled` 是“不加载这一行”，不是“减掉这个能力”；两者作用层不同 |
| 顺序无关 | 否，后者赢 | **只对 grants 采纳** | grants 是集合并集，顺序无关才能避免“后面一条规则覆盖前面”的隐性行为 |
| 对象替换 | `config` 整体替换、不深合并 | **不采纳** | 我们的 `config.d` 数组本来就要追加；整对象替换会重演 `90-console` 的问题 |
| 数组追加 | `insert` 追加 | **采纳** | 与现有 `config.d` 数组追加一致 |
| 删除语法 | 没有 | **不采纳“发明一种”** | 删除分片已经足够；grants 的减法用显式 deny |
| 未命中静默跳过 | warning 后继续 | **不采纳** | 我们的配置错误应带文件名和规则 id 硬失败；未命中不能只是日志 |
| 冲突静默后者赢 | 是 | **不采纳** | 片级 `replace` 声明冲突应加载期报错；grants 冲突由集合语义消解 |
| HMR 监听 | dsh-hmr | **不采纳** | 我们已有控制台的 hot/restart 影响清单；不再加一个文件监听器 |
| `name` 断言 | 可选校验 | **暂不采纳** | 对 grants 需要的是 `id` 唯一和 target 断言，不是整行 name；等 explain 有需求再说 |

### 3.3 判断

- 借“id + 显式 + 失败要响”的纪律，不借 patch 语言。
- cordis 的“后者赢”对配置覆盖有效，但对授权不合逻辑；授权必须集合化。
- `disabled`、`~`、`deny` 分别是行级、key 级、能力级三种不同的减法，不要混用。
- `config.d` 是组合模型，不是 overlay 模型；保持追加，把整段替换收窄到显式声明。

---

## 4. `90-console.toml` 特权显式化

### 4.1 现状

- `Config::load` 按文件名把 `90-console.toml` 从普通片里拆出来，最后加载
  （[config.rs](../../crates/kernel/src/config.rs#L1303-L1311)）。
- 它包含的每个顶层 key 直接 `table.insert` 覆盖已合并值
  （[apply_console_overlay](../../crates/kernel/src/config.rs#L3188-L3201)）。
- 如果它写了 `[grants]`，下层所有 grants（包括 70 片的 9 条规则）都会消失；代码注释
  已明确这一点（[config.rs](../../crates/kernel/src/config.rs#L1331-L1340)）。
- 文件名特权没有任何自描述：别人写 `91-*.toml` 会以为在追加，实际普通片确实是追加，
  但 `90-console.toml` 是覆盖；同一个目录里两种语义靠文件名区分。

### 4.2 提案：片内 `[fragment]`

在分片顶部增加一个保留表：

```toml
[fragment]
id = "console"                 # 稳定标识，默认用文件名；同一目录内唯一
replace = ["grants"]           # 可选；声明本片对这些顶层 key 做整体替换
```

语义：

1. 没有 `[fragment]` 的片：完全沿用现有组合（数组追加、表/标量后出现者覆盖）。
2. 有 `[fragment]` 但没有 `replace`：只提供稳定 id，组合行为不变。
3. `replace = ["grants"]`：本片的 `[grants]` 整体替换下层已合并的 grants；本片没有
   定义的 key 不受影响。
4. `replace = ["*"]`：本片包含的每个顶层 key 都整体替换；本片没有包含的 key 保持下层。
   这是 `90-console.toml` 现在的行为，只是从文件名改成显式声明。`*` 必须单独出现。
5. `[fragment]` 在合并前被剥离，不进入 `RawConfig`；`RawConfig` 应加
   `deny_unknown_fields`，否则 `[fragement]` 这种拼写会被静默忽略。
6. 校验：
   - `id` 为空或重复 → 加载期报错，带两个文件路径；
   - `replace` 里的 key 不在顶层白名单 → 报错；
   - 声明了 `replace` 但片里没有定义该 key → 报错，不做“声明即删除”；
   - 两个片声明同一个 `replace` key（包括 `*`）→ 报错，不允许静默后者赢。

示例：

```toml
# 90-console.toml
[fragment]
id = "console"
replace = ["*"]

[grants]
# 控制台保存的完整 grants；因为 replace=["*"]，这里整体覆盖下层
```

```toml
# 91-issue-extra.toml
[fragment]
id = "issue-extra"
# 没有 replace：普通追加/覆盖，和以前一样
```

### 4.3 与“移除只有删片”的关系

`replace` 是**替换值**，不是删除 key：

- 片里定义了 `[grants]`，就用它的值替换下层；
- 片里没有定义 `[grants]`，下层 grants 保留；
- 想彻底移除某个片贡献的 key，只能删掉那个片（或改 base），没有“声明 replace 但不写
  内容”的隐式删除。

这与 cordis 的“整段 config 替换”不同：我们只允许显式声明的顶层 key，且不允许通过
声明来删除不存在的值。

### 4.4 控制台保存路径

`persist_console_config` 继续写 `90-console.toml`（[console.rs](../../crates/bridge/src/console.rs#L1311-L1333)），
但文件名不再决定语义；控制台在生成文本时写入 `[fragment] id = "console" replace = ["*"]`。
`load_with_console_overlay` 必须和 `load` 共用同一个组合函数，否则保存前校验会与实际
启动走两条不同的规则。

旧文件迁移：如果 `90-console.toml` 没有 `[fragment]`，建议加载期直接报错并提示
“请让控制台重新保存一次，或手工加 `[fragment]`”；不要再用文件名偷偷补默认值，否则
“显式化”又退回去了。

---

## 5. 迁移与验证

### 5.1 迁移步骤

1. 实现 additive 求值器，放在 `grants.mode` 后面：
   ```toml
   [grants]
   mode = "legacy"   # legacy | additive；未知值加载期报错
   ```
   初始默认保持 `legacy`，现有行为不变。
2. 实现 `acbctl grants explain` 和 `acbctl grants matrix/diff`（可以先只做 explain，
   matrix 用测试脚本代替）。
3. 用一份配置副本把 `mode` 切到 `additive`，跑全矩阵差异；确认差异只出现在预期行
   （§5.2）。
4. 在 70 片把 9 条规则删回：
   ```diff
   - capability = ["room_read", "room_write", "access_request", "plugin"]
   + capability = ["plugin"]
   ```
   `.pre-rooms` 备份继续保留，不删。
5. 重启 bridge，确认启动摘要仍是 9 条规则 / effective 10，且 `grants mode: additive`
   出现在启动日志；对 9 个群逐个跑 explain。
6. 观察一个版本后，把 `additive` 设为默认，删除 legacy 分支和 `mode` 开关；`explain`
   继续保留。

### 5.2 行为差异清单

做法：枚举
`subject × place × room × capability × subscribed/unsubscribed`，用同一组
`GrantQuery` 分别跑 `legacy` 和 `additive`，只输出 allow/deny 或 basis 发生变化的行。
建议固定以下输入，避免矩阵被运行时数据干扰：

- subject：`system` / `system_admin` / `bot_admin` / `user`；
- place：`private` / `group` / `console`；
- room：9 个 70 片房间 + 一个不在 70 片的群 + 一个私信房间；
- capability：`GrantCapability` 的 10 个值；
- subscription：已订阅 / 未订阅两种。

矩阵是求值器层面的直查，不代表每个组合都能从当前工具面触发；它用于证明
floor / allow / deny 的组合语义，不代表运行时一定有相应的轮次。

预期差异（70 片已删回 `["plugin"]`）：

| # | subject | place | room | 已订阅 | capability | legacy | additive | 说明 |
|---|---|---|---|---|---|---|---|---|
| 1 | `system_admin` | `group` | `769843907` | 是 | `room_read` | deny `rule:6` | allow `base:subscription,rule:6` | 事故场景修复 |
| 2 | `system_admin` | `group` | `769843907` | 是 | `room_write` | deny `rule:6` | allow `base:subscription,rule:6` | 同上 |
| 3 | `system_admin` | `group` | `769843907` | 是 | `access_request` | deny `rule:6` | allow `base:subscription,rule:6` | 票据申请不再被压掉 |
| 4 | `system_admin` | `group` | `769843907` | 否 | `room_read` | deny `rule:6` | deny `base:not_subscribed` | 结果同、理由变；规则不再越权 |
| 5 | `system_admin` | `group` | `769843907` | 是（有轮次） | `plugin` | allow `rule:6` | allow `rule:6` | plugin 仍由规则授予 |
| 6 | `system_admin` | `group` | 任一 70 片房间 | 是 | 上述三项 | deny `rule:N` | allow `base:subscription,rule:N` | 9 个群同构 |
| 7 | 任一主体 | `group` | 不在 70 片的群 | 是 | `room_read` | allow `base:subscription` | allow `base:subscription` | 不变 |
| 8 | 任一主体 | `group` | 不在 70 片的群 | 否 | `room_read` | deny `base:not_subscribed` | deny `base:not_subscribed` | 不变 |
| 9 | `system_admin` | `private` | 私信房间 | 是 | `room_read` | allow `rule:0` | allow `rule:0,base:subscription` | 结果同；来源更完整 |

当前临时补丁（四能力）和删回后的对照，是这次迁移的核心证据：

| 场景 | 70 当前（四能力），legacy | 70 删回（只 plugin），additive |
|---|---|---|
| `769843907` 已订阅，`room_read` | allow `rule:6` | allow `base:subscription,rule:6` |
| `769843907` 已订阅，`access_request` | allow `rule:6` | allow `base:subscription,rule:6` |
| `769843907` 未订阅，`room_read` | allow `rule:6`（越权） | deny `base:not_subscribed`（收紧） |
| 任一 70 群已订阅，`plugin` | allow `rule:N` | allow `rule:N`（不变） |

如果删回后第 1-3 行不再是 allow，说明 floor 没有生效或规则仍在隐式否决，迁移失败。

### 5.3 `acbctl grants explain` 输出形状

命令建议：

```text
acbctl grants explain --session dsh-group2 --room qq:group/769843907 \
  --capability room_read --json
```

文本模式：

```text
mode=additive
query: subject=system_admin place=group room=qq:group/769843907 capability=room_read session=dsh-group2 subscribed=true
floor: allow base:subscription (session subscription)
grants:
  allow rule:6 source=70-grants-issue.toml#6 id=issue-group-769843907 adds=[plugin]
deny: none
effective: allow (base:subscription+rule:6)
```

JSON 模式：

```json
{
  "mode": "additive",
  "query": {
    "session": "dsh-group2",
    "subject": "system_admin",
    "place": "group",
    "room": "qq:group/769843907",
    "capability": "room_read",
    "subscribed": true
  },
  "floor": {
    "allow": true,
    "basis": "base:subscription",
    "source": "config.active.toml#sessions[dsh-group2].subscriptions"
  },
  "grants": [
    {
      "effect": "allow",
      "allow": true,
      "basis": "rule:6",
      "source": "config.d/70-grants-issue.toml",
      "rule_index_in_source": 5,
      "id": "issue-group-769843907",
      "capabilities": ["plugin"],
      "adds": []
    }
  ],
  "deny": [],
  "effective": {
    "allow": true,
    "basis": "base:subscription+rule:6",
    "reason": "floor grants room_read; rule:6 adds plugin; no deny"
  }
}
```

字段约定：

- `basis` 里的 `rule:N` 是合并后的全局序号（base 是 `rule:0`）；`rule_index_in_source`
  是 0-based 的片内序号，便于回到文件定位。
- 规则没有 `id` 时输出 `"id": null`；建议新规则都写 `id`。
- `grants` 只列匹配的规则；未匹配但 subject/place 相同的规则可以放在 `ignored` 里，
  默认不输出，`--verbose` 再展开。
- `effective` 必须给出“为什么”：floor 是否满足、哪些规则加了什么、deny 是否命中。
- 纯 floor allow 不写审计，但 explain 仍要显示。

差异清单可以基于同一 JSON：`acbctl grants diff --before=legacy --after=additive --json`
输出 `key` + `before` + `after`，只列变化行；key 用
`subject|place|session|room|subscribed|capability` 固定排序。

#### 5.3.1 已实现的只读 CLI（P2 第一小片）

上面的输出形状是设计草图；实际落地按「配置快照 + `CorePolicy`」实现，命令面如下：

```text
acbctl grants explain system_admin --place room:qq:group/769843907 \
  --capability plugin --config config.active.toml
acbctl grants matrix --config config.active.toml --subject system_admin \
  --capability room_read,room_write,access_request,plugin --format tsv
acbctl grants diff a.toml b.toml --format json
```

- 源抽象：`PolicySource`；本期唯一实现 `ConfigSource`（`Config::load` 读 base TOML +
  同级 `config.d`）。`policy.db` 上线后新增 `DbSource`，命令面不变。
- 输出必带 `source`（如 `config:/path/config.toml+config.d`）；`explain` 同时打印
  `policy_version`。静态 CLI 的 `policy_version` 默认 0，运行时版本由 bridge 内存计数器
  给出，因此**静态解释不等于 live bridge 实时判定**，只保证同配置 + 同 `CorePolicy` 语义。
- `diff` 两侧来源可以不同（config vs db/导出快照），用于迁移前后对照；只在单侧出现的
  格子记为 `<missing>`。
- 纯只读：不写 `config.toml` / `config.d` / `policy.db`，没有 `fix`/`normalize`/`import`
  或自动补 `id` 的命令。权限数据的唯一写入口仍是 policy API（bridge 独占写）。
- 退出码：0 成功 / diff 无差异，2 用法，3 未知 capability，4 规则或房间不存在，
  5 配置解析失败，6 diff 有差异。

### 5.4 `grants.mode`

- `legacy`：今天的 `evaluate_rules` 语义，用于回滚和对照。
- `additive`：`floor ∪ grants − deny`。
- 默认值先保持 `legacy`；迁移验证通过后改成 `additive`，再在一个版本后删除开关。
- 未知值加载期报错；启动摘要和 `grants explain` 都打印当前 mode。
- 两套求值器不能长期并存：开关只是迁移工具，最终只保留 additive。

### 5.5 端到端验证清单

1. `769843907`：让会话发出回应（只 @ 的 `#1096`），再用 `get_history` 拉
   `#934–#1046` 里的一段；确认不再是 `forbidden` / `RoomNotSubscribed`。
2. 审计应出现：
   ```text
   [access] ... capability=room_read ... result=allow basis=base:subscription,rule:6 ...
   [access] ... capability=room_write ... result=allow basis=base:subscription,rule:6 ...
   ```
   票据申请应出现 `access_request` 的 allow 来源（floor 或规则）。
3. 另一个群（`1134811973` 或 `158296748`）验一次读或写，确认不再
   `room_not_subscribed`。
4. 负向验证：让一个未订阅 9 个房间的会话尝试读其中一个，确认 additive + 删回后是
   deny `base:not_subscribed`；如果仍 allow，说明规则还在越权。
5. 如果 `access_request` 打开后出现新的越权审计或异常，停止，不扩大能力。

当前现场状态：70 临时补丁已生效，启动摘要为 9 条规则 / effective 10；但端到端 allow
审计尚未取得。最近的 `dsh-group2` 轮次在 `selectModel` 阶段遇到
`ECONNREFUSED 127.0.0.1:3080`，只调用了 `ack_messages`，没有 `send_message` /
`get_history`。dsh 恢复后按上面清单补验，不能把“启动成功”当成 e2e 通过。

### 5.6 回滚

- 配置回滚：把 `70-grants-issue.toml.pre-rooms` 复制回
  `70-grants-issue.toml`，重启 bridge；如果已经切到 additive，同时把
  `[grants] mode` 改回 `legacy`。
- 删片回滚：直接删 `70-grants-issue.toml` 会连 plugin 授予一起消失；要回到“只有
  plugin”的状态，用 `.pre-rooms`，不要凭记忆重写。
- 代码回滚：additive 求值器应独立提交；revert 后 legacy 仍是默认，旧配置行为不变。
- `.pre-rooms` 在迁移观察期结束前不要删。

---

## 6. 待评审决策点

### 6.1 是否引入 `effect` 字段

建议：**引入**，`effect = "allow" | "deny"`，默认 `"allow"`。

- 优点：一套 schema、一条规则列表、显式可读；deny 与 allow 使用同样的匹配字段。
- 风险：混合列表需要工具在展示时区分 effect；`capability = []` 必须报错。
- 替代方案：单独的 `[[grants.deny]]` 列表。更分隔，但字段重复，且容易让人以为
  deny 是另一套匹配器。若评审更看重“deny 不混在 allow 里”，再改成分表。

### 6.2 是否保留“无匹配才回退 base”

建议：**additive 模式不保留**；floor 始终参与，规则只在并集里加法。

- 保留“无匹配才回退 base”等于保留“部分匹配会压掉 floor”的根因；只是把触发条件
  从 plugin 扩到所有能力。
- `legacy` 模式可以保留旧行为用于回滚，但迁移完成后删除。
- 如果担心“所有规则都不匹配时行为变了”，用矩阵差异证明：没有规则匹配时，
  additive 与 legacy 的 floor 结果完全一致。

### 6.3 未知 capability 是否加载期硬失败

现状已经是：`GrantCapability` 是封闭枚举，serde 在解析期拒绝未知值；`GrantsConfig`
还有 `deny_unknown_fields`。建议保持，并补三类测试：

- 未知 capability；
- 未知 `effect`；
- 未知 `[fragment]` 字段 / `replace` key / `grants.mode`。

另外建议给 `RawConfig` 加 `deny_unknown_fields`，否则顶层 `[fragement]` 拼错不会报错，
新语法会静默失效。

### 6.4 删片以外的移除语法是否真的需要

建议：**不需要**。

- 配置层面：删片已经是最明确的移除；再加 `~` / `disabled` / `delete` 会把“行是否加载”
  和“能力是否授予”混在一起。
- 授权层面：显式 deny 已经能表达“减”，包括减掉 floor；不需要为某条规则单独发明
  “取消规则”语法。
- 如果未来确实需要“只取消下层某条规则、不删整个片”，也应做成带 `id` 的显式 deny，
  而不是通用 patch 删除标记。

### 6.5 `[fragment] replace=["*"]` 是否可接受

建议：**只给控制台使用**；其他片用按 key 的 `replace`，默认不写 `replace`。

- 控制台保存的是完整配置，逐 key 列出会很长；`*` 精确复刻它现在的“每个顶层 key
  整体覆盖”行为。
- `*` 必须单独出现；与具体 key 混用、或两个片都声明 `*`，加载期报错。
- 如果评审不接受 `*`，退化为逐 key 声明；控制台需要列出它写的所有顶层 key。

### 6.6 trust 是否进入 floor

建议：**暂不进入**。保持 trust 只做群内审批身份判定；如果以后要“不可信群禁止读写”，
写成显式规则或显式的 floor 条件，并在差异矩阵里单独列出来。
把 trust 隐式折进 `base_access` 会让“为什么这个群不能读”更难解释。

### 6.7 规则是否需要 `id`

建议：**可选，但新规则建议写**；同一份配置内唯一，重复加载期报错。

- `id` 用于 explain、审计和差异清单，不用于任意字段覆盖。
- 旧规则没有 `id` 时，explain 输出 `id: null`，用 `source + rule_index` 定位。

### 6.8 `grants.mode` 的寿命

建议：**只保留一个迁移版本**。迁移完成后默认 `additive`，删除 `legacy` 分支和
`mode` 字段；不要让它变成长期双语义。

---

## 7. 调研来源

本机 dsh 0.2.1-rc.2（Nix store 中的只读包）：

- `@deepseek-ai/dsh-app-boot@0.2.1-rc.2`
  - `lib/index.js`：`applyEntryPatches`（patch 的唯一应用点）、`composeEntries`、
    `readProfilePatches`（层顺序）、`parsePatchList`（顶层数组/错误定位）。
  - `README.md` / `README.zh.md`：profile/bundle patch 契约、home 层优先级、HMR、
    “用户 patch 替换整个 config、不做深合并”的已知限制。
- `@deepseek-ai/cordis-plugin-loader@1.0.6`
  - `src/config/entry.ts`：`Entry.disabled`（group 恒为 enabled）、`Entry.update`
    （null 删除字段、整体替换 options）。
  - `src/config/tree.ts`：按 id 解析、嵌套 id 分隔符、create/remove 运行时 API。
- `@deepseek-ai/cordis-plugin-include@1.0.10`
  - `README.md`：`patches` 配置项、insert/override 的官方描述。
  - `src/index.ts`：entries 文件读写。
- 上游 cordis：`cordiverse/cordis`，`cordis@4.0.0-rc.8`，
  `@cordisjs/plugin-loader@1.0.0-rc.9`。
- Koishi：`@koishijs/loader@4.6.11`，仓库 `koishijs/koishi` 的
  `packages/loader/src/shared.ts`：`plugins` map、`~` 前缀、`$if`、`writeConfig`。
  没有找到官方 `koishi.patch.yml`；搜索 “koishi.patch.yml” 无结果，Koishi 4.x 是单文件
  配置而不是分层 patch。

参考链接：

- <https://github.com/deepseek-ai/deepseek-harness>
- <https://github.com/cordiverse/cordis>
- <https://github.com/koishijs/koishi/tree/master/packages/loader>
- <https://www.npmjs.com/package/@koishijs/loader>

版本说明：以上语义按本机安装的 dsh 0.2.1-rc.2 源码核对；上游版本升级后，
`applyEntryPatches` 的字段覆盖/insert/disabled 行为需要重新核对。
