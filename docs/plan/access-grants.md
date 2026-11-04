# 访问授权（Access Grants）设计

> 状态：阶段一设计，**不实现授权表、票据系统或审批流**。阶段一 B 的两个洞已单独实现并测试。
> 目标：在不引入完整 RBAC 的前提下，明确「谁、在什么场所、什么范围、能做什么、对哪个目标」，并让默认拒绝、豁免和临时授权可审计。

---

## 0. 结论先行

- 模型是**四元组 + 目标**：`(主体, 场所, 能力范围, 能力) + 目标`。不做角色继承、资源树、权限合并。
- 三档：**机器人管理员**（私信入口、已订阅范围、全部平台动作 + 完整 `plugin_tool_call`）、**系统管理员**（单独配置、私信/控制台、任意目标、任意操作）、**其余**（现有 room access，行为不变）。
- **权限跟随本轮触发者**，不跟随 agent。定时任务、自发唤醒、无人类主体的轮次 `subject = none`，不继承任何常驻豁免或票据。
- **群内常驻权限为零**：即使群里发言的账号是管理员本人，也不自动提权。群里要动危险操作，只能**提出申请**；审批必须在**私信**里进行。
- 权限分两层：**常态授予**（配置写死的几条，带场所限制）与**一次性票据**（绑定本轮 + 单目标房间 + 具体 op 列表 + 过期时间；出轮即废、用后即废、可撤销、不跨轮）。
- 授权判定只允许**一个实现**：内置四元组评估器，或一个外部策略（RBAC / 外接）。同一时刻不允许两套判据并存；插件缺席用内置实现，解析失败 fail closed。
- `MethodSpec.risk = Destructive` + `allow_destructive` 与授权是**叠加**关系：授权只扩大范围，不替代 destructive 闸门。系统管理员也不能绕过 `allow_destructive`。

---

## 1. 现状事实（设计边界）

### 1.1 现有三层判定

| 层 | 位置 | 作用 |
|---|---|---|
| `ToolAccessProvider` | [plugin.rs](../../crates/kernel/src/plugin.rs#L663-L665)，调用 [engine.rs](../../crates/kernel/src/engine.rs#L4112-L4164) | 按工具名/参数/轮次上下文做插件级否决；只能 Deny，不能 Allow 覆盖 core |
| `AccessProvider` | [plugin.rs](../../crates/kernel/src/plugin.rs#L767-L769)，调用 [engine.rs](../../crates/kernel/src/engine.rs#L4258-L4373) | 按会话策略判定房间（Read/Send）或连接（Connection）访问 |
| `Risk::Destructive` | [engine.rs](../../crates/kernel/src/engine.rs#L3875-L3888)，`allows_destructive` [engine.rs](../../crates/kernel/src/engine.rs#L6591-L6597) | 破坏性平台动作的独立闸门，按连接 `allow_destructive` 放行 |

当前实际行为：

- `send_message` 对当前轮房间原先**不**复检 access；阶段一 B 已加 `ensure_can_send` 复检。
- `plugin_tool_call` 原先没有房间/连接权限检查；阶段一 B 已要求显式 `scope`，core 按 scope 检查。
- `ToolAccessProvider` 在仓库内没有生产内置实现；无 provider 时 `check_tool_access` 返回 `None` → 放行。
- `AccessRequest` 只有 `session / connection / room / origin_room / action / subscriptions`，**没有 origin_sender，也没有场所**；实现按主体/场所判定需要扩展接口（见 6.1）。

### 1.2 阶段一 B 已补的两个洞

- `send_message` 在 `tool_call` 与 `begin_tool_call` 两条路径都先 `ensure_can_send(当前房间)`，失败返回 `RoomNotSubscribed`。
- `plugin_tool_call` 必须带 `scope`；未声明直接拒绝；房间作用域按写操作检查，连接作用域按连接可读检查。

---

## 2. 模型：四元组 + 目标

```
(subject, place, scope, capability) + target
```

- **subject**：本轮触发者，不是 agent，也不是"当前会话"。
- **place**：本轮发生在哪类场所。它是独立维度，不能折进 subject：
  - `private`：私信房间；
  - `group`：群聊房间；
  - `console`：控制台/运维入口（未来接入）；
  - `none`：没有人类主体的轮次（定时、时钟、自发唤醒）。
- **scope**：能力范围，建议 `any` / `subscribed` / `specific`。
- **capability**：粗粒度能力，封闭枚举：
  - `read`：读流水、事件、笔记、只读平台方法；
  - `send`：发消息、写类平台方法；
  - `destructive`：破坏性平台动作；
  - `plugin_tool`：调用插件工具。
- **target**：具体目标房间或连接。

**场所为什么独立**：同一个管理员账号，在私信里和在群里必须是两套权限。私信是"本人在说话"的可信入口；群聊里的账号可能被群友诱导、被上下文污染，不能等同于本人下指令。控制台是第三种入口，未来由操作员身份决定。

**不做**：角色、角色继承、资源树、权限合并、跨实例联邦。一个请求只命中一条明确规则，不做多条规则的并集。

---

## 3. 三档与判定点

### 3.1 机器人管理员（bot admin）

- 配置指定平台账号；**入口限私信**：只有该账号在私信房间触发的轮次才获得这一档。
- 范围 = **已订阅目标**。不能因为这一档访问未订阅房间。
- 能力 = 全部平台动作 + 完整 `plugin_tool_call`；但 `destructive` 仍要过 `allow_destructive`。
- 判定点：
  - 轮次开始：`origin_sender` 命中 bot admin 配置，且 `origin_room` 是私信房间。
  - `place = group` 时，即使账号相同，也不获得这一档。
  - 授权判定：目标在会话订阅内 → Allow；否则 Deny。
  - `plugin_tool_call` 的 scope 仍要过房间/连接检查。
- 日志：不要求逐次审计；拒绝走现有 access/tool_access 日志。

### 3.2 系统管理员（system admin）

- 单独一条配置，通常是用户本人账号；**不是** `connections[].admin_qqs`（后者只用于标记群角色，不是全局权限）。
- 常驻场所 = **私信 / 控制台**；**群聊不自动提权**，即使说话的是同一个账号。
- 范围 = **任意目标，含未订阅房间/连接**（仅在允许的场所内）。
- 能力 = 任意操作；`destructive` 仍要过 `allow_destructive`。
- 启动时打醒目日志，例如：
  - `[access] system_admin configured connection=qq account=10002; standing grant: private/console only`
- **每次使用记审计**：主体 / 场所 / 能力 / 目标。建议结构化一行：
  - `[access] subject=system_admin place=private capability=platform.send target=qq:group/999 room=qq:private/10002 session=main`
  - 被 destructive 闸门拒绝时也记 `capability=destructive-denied`。
- 判定点：轮次开始时解析 subject/place；授权判定对允许场所内的任意目标 Allow；`Risk::Destructive` 仍生效。

### 3.3 其余

- 保持现有 room access：订阅内允许，订阅外拒绝；`elevated_in_rooms` / `privileged_origins` / `all_rooms` 等现有策略继续按各自语义工作（但见 5.3 无主体轮次）。
- 不新增隐式能力。
- 群内普通成员/管理员账号只有正常 room access，没有常驻提权。

---

## 4. 必须遵守的原则

### 4.1 权限跟随「本轮触发者」

- `TurnSubject` 在轮次开始时计算一次，整个轮次使用同一快照；配置热重载不改变在途轮次。
- 来源是轮次的触发记录：`origin_room` + `origin_sender` + 该连接配置。
- agent 是谁、agent 自称什么，都不影响 subject/place。
- 定时任务、自发唤醒、没有人类发送者的轮次：`subject = none`、`place = none`。

### 4.2 默认拒绝 + 显式授予

- 没有命中明确 grant 的请求一律拒绝。
- 只有配置里显式列出的账号才获得 bot admin / system admin。
- 系统管理员档必须单独配置、启动告警、逐次审计；不能和 bot admin 混在一个列表里。
- 群内常驻权限为零；群里的危险操作只能走"申请 → 私信审批 → 票据"。

### 4.3 只有一个授权实现

- 接口只定义：

```text
evaluate(subject, place, scope, capability, target) -> allow/deny + reason
```

- 内置实现是四元组评估器；第三方可以整体替换成 RBAC、外接策略、数据库驱动策略。
- **同一时刻只生效一个实现**。配置了外部实现，就不并行运行内置实现；内置实现与外部实现不能同时给出判据（否则审计无法回答"是谁批准的"）。
- 插件缺席或加载失败 → 用内置实现并 fail closed；不要出现"没有 provider 所以放行"。
- 现有 `AccessProvider` / `ToolAccessProvider` 在阶段二收敛为**调用适配器**：授权结论只由唯一 evaluator 产生；`ToolAccessProvider` 若保留，只能做非授权的工具过滤/否决，不能成为第二套授权来源。

### 4.4 与 `Destructive` / `allow_destructive` 的关系：叠加

- 授权只决定"能不能到这个目标/用这个能力"；`Risk::Destructive` 是第二道独立闸门。
- 结论：**叠加，不替代**。
- 即使 system admin，`group.kick` / `group.ban` / `message.recall` 等仍要求目标连接配置了 `allow_destructive`（或方法白名单）。
- 不把 `allow_destructive` 并入 grant 表，也不因为 grant 放行就跳过它。

---

## 5. 配置形状与评估顺序

### 5.1 TOML 草案

```toml
[grants]

# 机器人管理员：入口限私信；范围仅限该会话已订阅的目标。
[grants.bot_admin]
accounts = [
  { connection = "qq", account = "10001" },
]

# 系统管理员：只能一条；私信/控制台常驻；群聊不自动提权。
[grants.system_admin]
connection = "qq"
account = "10002"

# 常态授予规则：四元组 + 目标。
[[grants.rules]]
subject = "bot_admin"
place = "private"
scope = "subscribed"          # any / subscribed / room / connection
capability = ["room_read", "room_write", "plugin"]

[[grants.rules]]
subject = "system_admin"
place = "private"
scope = "any"
capability = ["room_read", "room_write", "connection_read", "platform", "destructive", "plugin"]
```

`[grants.tickets]` 等票据配置在阶段二再加；当前解析器不认识它，写了会按未知字段启动失败。

约束：

- `bot_admin.accounts` 可多条，但每条必须带 `connection` 和 `account`。
- `system_admin` 只允许一条；重复配置在解析期拒绝。
- `scope = "room"` / `"connection"` 必须给 `target`；`any` / `subscribed` 不许给 `target`。
- `capability` 不能为空、不能重复，值域是封闭枚举。
- `[grants]` 省略 = 只有订阅边界；未知字段/未知枚举在启动解析期直接报错。
- 外部 evaluator 通过 `[sessions.access] provider = "..."` 整体替换；此时该会话不跑内置 grants。
- 不把 system admin 自动加进 `connections[].admin_qqs`；两者语义不同。

### 5.2 评估顺序

1. **tool_access 兼容否决层**：只做工具级 deny；它不是第二套授权实现，不能 Allow 覆盖 core。
2. **唯一 evaluator**：core 先从本轮触发记录解析 `(subject, place)`，再调用
   `evaluate(subject, place, scope, capability, target)`：
   - `subject=system_admin` + `place=private|console` → 任意目标允许；
   - `subject=bot_admin` + `place=private` → 仅已订阅目标；
   - `subject=system` / `place=none`、其他主体，或 `place=group` 且没有票据 → 走现有 room access；
   - 票据：命中本轮 + 单目标房间 + op 列表 + 未过期 → 允许该 op。
3. **destructive**：`MethodSpec.risk == Destructive` → 查连接 `allow_destructive`；未放行则拒绝。
4. **平台 I/O**。

`plugin_tool_call` 走 1 → 2（用显式 `scope`）→ 4；它没有 `MethodSpec.risk`，所以不经过 core 的 destructive 表。插件工具内部如果自己调平台，core 看不到；这是插件信任边界，见 9.2。

### 5.3 无主体轮次

- `subject = system`、`place = none`：定时任务、时钟唤醒、没有人类发送者的自发轮次。
- 不继承 bot admin / system admin，也不继承 `all_rooms` / `elevated_in_rooms` / `privileged_origins` 等非订阅豁免。
- 不继承任何票据；票据绑定本轮，无主体轮次不会持有票据。
- 仍保留订阅内的正常访问；`send_message` 当前房间复检、`plugin_tool_call` scope 检查都继续执行。
- 这样避免"定时任务在特权房间跑一次，就获得全局权限"。

### 5.4 旧 access provider 的兼容映射

旧配置继续可用，但统一映射成 `system` 主体的常态授予，并在启动时打印弃用提示：

| 旧 provider | 映射 |
|---|---|
| `acb.access.all_rooms` | `system` + `place=none` + `scope=any`，所有能力 |
| `acb.access.elevated_in_rooms` | `system` + 每个 `access.rooms` 房间一条授予；连接能力按该连接是否有列出房间 |
| `acb.access.privileged_origins` | `system` + 仅当 `origin` 在 `access.origins` 时 `scope=any`，所有能力 |
| `acb.access.subscribed` | 不生成授予，直接走订阅边界 |

**「群内零常驻」的边界**：它约束的是**人类主体**在群场所被自动提权；不约束运维显式写入的 `[grants]` 或旧 provider 兼容映射。也就是说：

- 群里发言的 system admin 账号不会自动获得任何授予；
- 运维显式写的 `[grants]` 规则（包括 `subject=system` 或旧的 `all_rooms` 兼容映射）仍然生效；
- 唯一需要人类在私信批准的是**临时票据**（阶段二），不是常态配置。

---

## 6. 权限分两层：常态授予 + 一次性票据

### 6.1 常态授予

只有配置写死的几条，带场所限制：

| 主体 | 场所 | 范围 | 能力 |
|---|---|---|---|
| `system_admin` | `private` / `console` | `any` | `read` / `send` / `destructive` / `plugin_tool` |
| `bot_admin` | `private` | `subscribed` | `read` / `send` / `destructive` / `plugin_tool` |
| 其他 | 任意 | 现有 room access | 现有能力 |

- 常态授予不随 agent 的自我声明变化。
- `destructive` 能力仍受 `allow_destructive` 叠加限制。
- 群内没有常态授予行；`place=group` 的 bot/system admin 自动落回普通 room access。

### 6.2 一次性票据

```text
Ticket {
  id,
  turn_id,          // 绑定申请/使用的那一轮
  target_room,      // 单目标房间
  ops: [op_id],     // 具体 op 列表，不是"房间全权限"
  expires_at,
  used: bool,
  revoked: bool,
}
```

- **绑定本轮**：票据只在申请它的那一轮有效；出轮即废。
- **单目标房间**：一张票据只覆盖一个目标房间，不覆盖整个连接、不覆盖全局。
- **具体 op 列表**：只允许列表内的 op；不是"这个房间能做的所有事"。
- **过期时间**：默认短 TTL（草案 300 秒），到点即废。
- **用后即废**：一次成功执行后票据失效；多步操作需要重新申请，或拆成多张票据。
- **可人工撤销**：私信或控制台可撤销未使用的票据。
- **不跨轮继承**：下一轮不会自动带上上一轮的票据。

**粒度取舍**：按 op 列表 + 单目标房间是最小可用粒度，滥用时影响面被限制在"一个房间、一组明确 op、一次使用"。代价是审批可能变多；不建议放宽成"按房间全权限"或"全局票据"，那会让一次误批准等于长期授权。

### 6.3 申请与审批

1. 群里 agent 只能提出申请：说明动作、目标房间、op 列表、期望 TTL。申请本身不执行任何平台动作。
2. core 把申请送到 **system admin 的私信**；审批必须在私信里完成，群聊消息不能批准。
3. 批准后生成票据：绑定申请轮次 + 目标房间 + op 列表 + TTL。
4. 群里 agent 在同一轮内使用票据；用后即废。
5. 审计四条记录：申请、批准、使用、超时/撤销。

未来工具名（草案，不实现）：`access.request`、`access.approve`、`access.revoke`。它们不是新的权限入口，只是票据生命周期操作。

---

## 7. 接口与实现前置

### 7.1 `AccessRequest` 看不到触发者

现有 [AccessRequest](../../crates/kernel/src/plugin.rs#L743-L757) 没有 `origin_sender`，也没有 place。阶段二必须二选一：

- 扩展 `AccessRequest`，加入 `subject` + `place`（推荐，策略不需要自己重算）；
- 或加入 `origin_sender` + `origin_room_kind`，由 evaluator 自己判断。

推荐 `(subject, place)`：主体和场所只在 core 做一次，外部 evaluator 只读结果，避免每个实现重复实现"私信入口"等规则。

### 7.2 `ToolAccessContext` 的 `is_admin` 不是全局管理员

`ToolAccessContext.is_admin` 来自 `Config::is_admin(room, sender)`，即连接 `admin_qqs`，不是 bot admin / system admin。阶段二需要新增：

- `subject` + `place`，或
- `is_system_admin` / `is_bot_admin` 两个布尔。

不能复用 `is_admin` 字段，否则会把群管理员误当成系统管理员。

### 7.3 唯一 evaluator 的注册点

- `PluginManager::with_native_builtins()` 目前只注册 4 个 access provider，没有 tool_access 默认 provider。
- 阶段二应注册 core 默认 `acb.access.default`，并把"无 provider → 放行"改为"无 provider → 拒绝"。
- 配置了外部 evaluator 时，不并行运行内置 evaluator；启动期校验只允许一个。

---

## 8. 审计与可观测性

- **启动告警**：配置 system admin 时打醒目一行，包含连接和账号，但不含 token。
- **system admin 逐次审计**：主体 / 场所 / 能力 / 目标 / 房间 / 会话。
- **票据审计**：申请、批准、使用、超时/撤销各一条；带 ticket id、turn id、target、ops。
- **拒绝审计**：授权拒绝与 destructive 拒绝分别记录 reason；evaluator 返回的 `reason` 必须可读。
- **隐私**：日志不带消息正文、token、平台响应 body。

---

## 9. 威胁模型

### 9.1 群友注入 / 上下文污染

- 群友可以诱导 agent 生成任意工具调用，但群里**没有常驻提权**。
- 危险操作的最高上限是**提出一个申请**；不能直接执行。
- 审批在私信里由人类完成；审批 UI/消息只展示 core 规范化后的 op 列表、目标房间、动作摘要，不直接渲染 agent 的自由文本。
- 因此攻击者能拿到的是"让管理员看到一条申请"，不是"让管理员批准"或"让 agent 执行"。

### 9.2 插件工具边界

- `plugin_tool_call` 的 scope 检查只约束声明；插件拿到 `ToolCallContext` 后仍可自己做任意 I/O。
- 插件工具内部如果直接连平台端口，core 看不到；这是现有信任模型，不在本设计内解决。
- 阶段二可以选择：只允许受信任插件注册工具，或给插件工具加 `risk` 元数据；当前不做。

### 9.3 其他

- 被攻陷的 system admin 私信账号可以批准任意票据；这属于账号失陷，超出本设计。
- 申请刷屏需要限流/审计；草案里不做自动批准。
- 控制台入口未来需要独立的操作员身份；当前只作为 place 占位。

---

## 10. 风险与未知

1. **现有策略的兼容性**：`all_rooms` / `privileged_origins` / `elevated_in_rooms` 都是非订阅豁免；无主体轮次必须不生效，阶段二需要把这些策略纳入 place/subject 门控，或明确重定义。
2. **票据与轮次生命周期**：如果审批耗时超过本轮，票据随轮次作废；需要明确"申请后是否保持本轮等待"。当前设计是保持本轮等待、超时即失败。
3. **多 op 票据**：一次使用即废，意味着多步操作要多次申请；这是安全取舍，不是遗漏。
4. **外部 evaluator**：第三方 RBAC 必须自行实现 place/subject 规则；core 只能在接口层要求，无法证明其内部正确。
5. **系统管理员配置**：只有一条，避免多个全局管理员扩大攻击面；如果未来需要多人，必须新增显式审批设计。
6. **平台账号匹配**：按 `connection + account` 匹配，不做跨平台身份统一；同一 QQ 号在不同连接上出现两次时分别匹配。

---

## 11. 阶段二实施拆分（待批准，不在本次实现）

1. **配置 + subject/place**：`[access]` 解析、轮次快照、启动告警。
2. **唯一 evaluator**：core 内置四元组实现；外部实现整体替换；无 provider fail closed。
3. **接口扩展**：`AccessRequest` / `ToolAccessContext` 增加 subject/place；旧 provider 兼容为 subject=none。
4. **票据**：申请/批准/使用/过期/撤销，绑定 turn + target + ops + TTL。
5. **审计**：system admin 逐次、票据四事件、destructive 拒绝。
6. **测试**：三档 × 四场所矩阵、无主体轮次、票据生命周期、群内零常驻、destructive 叠加、fail closed。

---

## 12. 明确不做

- 角色、角色继承、资源树继承。
- 权限合并/并集。
- 跨实例联邦、远程授权。
- 逐 action ACL 矩阵。
- 把 `allow_destructive` 合并进 grant 表。
- 群内自动提权（即使账号是管理员本人）。
- 长期/全局票据。
