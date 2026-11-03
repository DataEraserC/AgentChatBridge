# 「平台概念不进 core」的边界提案

状态：提案（2026-11-04）。**只出文档，不改代码、不改 ADR 原文。** 等待用户确认后再决定是否
修订 ADR / 调整代码。

## 1. 问题：这条规矩字面执行会妨碍多平台适配

现有表述（愿景书 / ADR-0027 大意）：「凡是只在某个平台或某个 harness 上成立的东西，都不进
核心。」这句话方向正确，但字面执行容易滑向两个错误：

1. **把「平台无关的能力抽象」也赶出 core**：例如房间、内容元素、事件形状、能力目录、
   发送/撤回/表情的通用动作表。这些不是平台实现，而是所有适配器接入 core 必需的
   公共语言；赶出去只会让每个适配器自带一套桥，耦合更重。
2. **把「平台身份」写进通用插件 trait**：例如给通用插件接口加 `platform()`，core 或
   通用调度层据此分支。这才是真正要禁止的「平台概念进 core」。

两者混为一谈，就会得到错误结论：「为了不碰平台，core 连能力协商都不该有」。

## 2. 正确拆分：两件事必须分开

| 禁止 | 必须留在 core |
|---|---|
| 平台协议实现（OneBot/KOOK/Discord 的解码、编码、鉴权、重连） | 平台无关的抽象：房间标识、内容元素、事件形状、能力目录 |
| 平台特有语义分支（`if platform == "qq"`、`match event_name`） | 能力协商：适配器声明支持哪些动作/字段/事件标签 |
| 平台特有的策略（例如「QQ 拍一拍叫醒」） | 策略机制的接口（wake/access/route/... 的 trait 与上下文数据） |
| 平台身份进入通用插件 trait（`DynPlugin::platform()`） | 平台身份作为**数据**传给需要它的策略/适配器 |
| 平台特有的参数名/样例硬编码进 core 语义路径 | 通用参数类型、目标校验、方法目录 schema |

一句话：**平台实现和平台策略不进 core；平台无关的接口、类型与能力协商必须留在 core，
否则适配器无法接入、反而更耦合。**

## 3. 可执行判据

> 新增一个平台，能否只写适配器 + 完成注册，不改 core 业务文件？

- **能**：抽象足够。核心文件（`engine.rs`、`plugin.rs` 的策略机制、`storage` 的通用形状、
  `adapter.rs` 的注册表）零改动，新增平台只增加一个 adapter crate + 配置 + 注册。
- **不能**：缺抽象，而不是规矩错。应按缺口类型补：
  - 缺通用内容元素 → 加元素（兼容演进）；
  - 缺通用动作 → 加能力协商 / 方法目录字段；
  - 缺通用事件形状 → 加事件标签/载荷约定；
  - 只有某个平台才成立的策略 → 写策略插件，不改 core。

判据落地方式（建议后续做，不在本轮）：
1. 新建一个最小 fake adapter crate，只实现 `PlatformAdapter` + manifest，注册后跑现有
   kernel 测试；若必须修改 kernel 业务代码才能通过，记录缺口。
2. CI 可加一条「adapter-only 变更」检查：新增 `crates/adapters/<platform>/` 与注册点
   之外，不允许改 `crates/kernel/src/engine.rs` 等文件（白名单除外）。

## 4. 反例与正例

### 4.1 应禁止：平台身份进入通用插件 trait

```rust
// 反例（假设形态）：通用插件接口自带平台身份
trait DynPlugin {
    fn platform(&self) -> &str;
    fn decide(&self, context: &Context) -> Decision;
}
```

问题：
- 通用调度层会开始按 `platform()` 分支；
- 第三方策略插件被迫声明一个它并不拥有的平台；
- 新增平台要改 core 的匹配逻辑，判据 3 直接失败。

正确形态：平台身份只出现在**上下文数据**里（`WakeContext` / `RouteContext` /
`ReceiveRequest` 的 `platform` 字段），由需要它的策略插件读取；core 不据此做业务分支。
这也是 ADR-0027 已确认的「平台独有策略是策略插件，可以读 `platform` + `payload`」。

### 4.2 应保留：`PlatformAdapter` + 能力协商

- `PlatformAdapter` 在 core 外，拥有 I/O、编解码、方法目录。
- core 只保留注册表（`AdapterRegistry` / `PluginManager::register_adapter`）和
  manifest 形状。
- 能力协商建议补齐为显式 flags（当前 `AdapterCapability` 只有
  `ReceiveEvents` / `Send` / `Catalog`；`WireManifest` 另有 `supports_at` /
  `supports_reply` / `events` / `event_tags`）：

| 能力 flag（建议） | 含义 | core 怎么用 |
|---|---|---|
| `edit` | 能编辑已发消息 | 方法目录展示 + 发送动作可选路径 |
| `recall` | 能撤回 | 同上 |
| `reaction` | 能加/删表情回应 | 同上 |
| `thread` | 支持线程/子频道 | 房间/目标校验可选分支 |
| `member_list` | 能列成员 | 能力目录 |
| `typing` | 能发输入状态 | 可选动作 |

这些 flag 只是**数据**，core 不按平台名分支；缺 flag 时动作报「不支持」，不静默降级。

### 4.3 需要复核的现有代码位置

| 位置 | 现状 | 判断 |
|---|---|---|
| `crates/kernel/src/adapter.rs` `AdapterManifest.platform` | 适配器自报平台名 | 允许：注册数据，不是业务分支 |
| `crates/kernel/src/plugin.rs` `WakeContext.platform` / `RouteContext.platform` | 上下文数据 | 允许：供策略插件读取 |
| `crates/kernel/src/config.rs` `Connection.platform` + `validate_wake_events` | core 校验 `qq.*` 事件前缀 | 可接受但需盯住：这是「平台命名空间声明」，不是平台语义；若继续长平台规则，应改为适配器提供事件名校验 |
| `crates/proto/src/element.rs` `Face { platform, id }` / 表情等 | 平台特有内容以不透明载荷携带 | 允许：core 只搬运，不解释 id 含义 |
| `crates/kernel/src/outbound.rs` `sample_string`（`group_id` / `uin` / `qq` 样例值） | core 给参数生成示例 | **可疑但非语义**：只影响目录展示；建议后续由 `MethodSpec` 自带 examples，core 不再猜平台参数名 |
| `crates/adapter-api/src/lib.rs` `WireManifest` | 适配器声明 platform/capabilities/events/tags | 允许：能力协商数据 |
| `crates/kernel/src/engine.rs` `platform_of` | 连接名 → 平台名，展示用 | 允许：展示数据 |
| 任意 `if platform == "..."` / `match event` | 未发现业务分支 | 保持禁止 |

## 5. 按四项口径评估

- **稳态运行成本**：能力 flags / method catalog 是 manifest 数据，不在每轮热路径；
  不能让 core 为每个平台能力加分支或额外往返。
- **可观测性**：缺能力时必须明确报「不支持」，不能静默降级；适配器 manifest 与
  `describe_plugins` 要能看到声明与实际。
- **接口面增长速度**：优先一个通用的 `capabilities`/`MethodSpec` 数据面，而不是
  core 为每个平台新增入口；新平台只长适配器 crate。
- **可变更性**：能力 flags 一旦发布就会被外部适配器依赖，名字要窄且语义稳定；
  宁可用通用 flag（edit/recall/reaction/thread），不要把平台名写进 schema。

开发工作量不作为否决理由；上面五项复核的代码位置见 4.3。

## 6. ADR 措辞修订建议（只给文本，不改原文）

建议把愿景书 / ADR-0027 的那句话替换为两条：

> **平台实现与平台策略不进 core。** 平台协议解码、编码、鉴权、重连，以及只在某个平台
> 成立的唤醒/路由/发送策略，都由适配器或策略插件实现。core 不按平台名或平台事件名做
> 业务分支。
>
> **平台无关的抽象必须留在 core。** 房间标识、内容元素、事件形状、能力目录、目标校验、
> 策略接口与能力协商是适配器接入 core 的公共语言；把这些赶出 core 会让每个适配器重复
> 实现桥，耦合更重。判断标准：新增平台能否只写适配器 + 注册、不改 core 业务文件。

并建议在 ADR-0027「技术细节」增加一条：

> `platform` 只能作为数据出现在 manifest / context / record 中；不得出现在通用插件 trait
> 的方法签名里，也不得成为 core 的分支条件。平台能力以 flags / method catalog 表达，
> 缺失能力时明确报不支持。

## 7. 需要用户拍板

1. 是否接受上述两句话作为愿景书/ADR-0027 的修订方向（本轮不改原文）。
2. 能力 flags 是否先补 `edit` / `recall` / `reaction` / `thread` 四项，还是等真实适配器
   需求再补。
3. `sample_string` 的平台参数样例是否移到 `MethodSpec.examples`（会改 schema，属于
   后续小改动，不在本轮）。
