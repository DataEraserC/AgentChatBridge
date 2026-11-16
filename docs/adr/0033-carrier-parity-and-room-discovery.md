# ADR-0033 插件载体的能力对等：房间发现 + Process 载体补齐扩展点

来源：用户裁定（2026-11-16）：「外部插件也能提供房间列表」；「一次把 wire 也扩了」；
「调查一下插件生态中缺少的部分，多种插件形式中不对等的能力（我们需要保证各种类型的
插件均能实现对等功能）」。本文标「用户裁定」的是用户原话；其余为实现默认，可推翻。

## 这条决定了什么行为

两件事：

1. **房间发现成为平台适配器的一项一等能力**，并且三种适配器载体
   （内置 / 外置进程 / 动态库）都能提供它。
2. **Process 载体从「只会 access」补齐到与 Lua 对等的策略扩展点覆盖**。

## 背景：现存的不对等（2026-11-16 实测）

判据是 `grep -rn "impl .*Provider for" crates/ | grep -v test`，按 crate 归组：

| 载体 / crate | Wake | Access | Clock | Receive | Route | TurnError | Tool | ToolAccess | Prompt | 平台适配器 |
|---|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|:-:|
| `kernel`（内置） | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — |
| `plugins/lua`（Script） | ✅ | ✅ | — | ✅ | ✅ | ✅ | — | — | — | ❌ |
| `adapters/dynamic`（.so） | ✅ | ✅ | ✅ | ✅ | — | ✅ | — | — | — | ✅ |
| `plugins/process-provider`（Process） | **✅** | ✅ | **✅** | — | **✅** | **✅** | — | — | — | — |
| `plugins/process-tools` | — | — | — | — | — | — | ✅ | — | — | — |
| `plugins/trusted` | ✅ | — | — | — | — | — | — | — | — | — |
| `wasm-host` | — | — | — | — | — | — | — | — | — | ❌ |

加粗 = 本 ADR 落地后新补的（Process 载体原来只有 Access）。矩阵仍是**现状快照**，
不是承诺；未实现的那几格写在下面的「已知不对等」里，带原因。

三处最刺眼：

- **Process 载体只支持 access**（`plugins/process-provider/src/loader.rs:3` 注释原文：
  「当前最小面只支持 `access`。载体本身不认识 wasm」）。同样说「进程 + JSON 行协议」，
  `adapters/process` 那条路却支持 Send / Call / Catalog / Connections——**两条 Process
  路径用了完全不同的协议**。
- **wasm-host 一个扩展点都没接**。
- **ToolAccess 只有 kernel 实现**，任何外部插件都无法参与工具授权判定。

## 决策一：房间发现走「core 主动问，适配器回答」

与已有的连接注册（`CoreMessage::ConnectionsList` → `WireResult::Connections`）
**同构**，不发明新模式：

```rust
// adapter-api/src/lib.rs
pub enum CoreMessage {
    // …已有的 Describe / Decide / Send / ConnectionsList / Call / Shutdown
    /// 让适配器枚举这条连接上可选的房间。适配器用 `WireResult::Rooms` 回。
    /// `query` 原样透传给适配器（可选的前缀/分页等平台私有语义），core 不解释。
    RoomsList {
        id: u64,
        #[serde(default)]
        query: Value,
    },
}

pub enum WireResult {
    // …已有变体
    /// 房间枚举的回执（对 `CoreMessage::RoomsList`）。
    /// `supported = false` 表示这个适配器不提供枚举；`rooms` 为空不等于不支持。
    Rooms {
        supported: bool,
        #[serde(default)]
        rooms: Vec<WireRoom>,
    },
}

/// 一条可选的房间。`label` 只给人看（群名 / 昵称），不参与路由。
pub struct WireRoom {
    pub room: RoomId,
    #[serde(default)]
    pub label: String,
    /// 平台侧的分类提示，例如 `group` / `private`。core 只透传给界面。
    #[serde(default)]
    pub kind: String,
}
```

**为什么加 `query`**：不同平台的可选过滤不同（QQ 有群/好友两套）。让适配器自己解释
`query`，core 只转发——与 ADR-0020「平台能力：一个目录 + 一个通用调用口」同一个思路：
core 认识「有查询这回事」，不认识查询的内容。

**为什么 `supported` 是显式布尔而不是「空数组」**：两者语义不同。某个号真的一个群都没有
是「支持但为空」；适配器没实现是「不支持」。合并会让控制台把「这个平台不支持」显示成
「你没有群」。

## 决策二：动态库不需要 ABI v3

`adapter-api/src/lib.rs:28` 的注释原文：「所有跨 ABI 的文本都是 JSON，扩展点靠 `op` /
`type` 字段演进，不改 ABI 形状。」

新消息类型只走 `invoke` 里那条 JSON，`AdapterV1` 函数表一个字段都不加。所以：

- **不新增 `ABI_VERSION_V3`，不改 `AdapterV1` / `AcbPluginV2` 结构**
- 旧 .so 收到不认识的 `CoreMessage::RoomsList` 时，按既有约定回 `WireResult::Error`；
  core 把 `Error` 当作 `supported = false` 处理（并在日志里记一次，避免静默）

这条要在实现时**核实**旧适配器确实会回 Error 而不是 panic——见「验证」一节。

## 决策三：Process 载体补齐，但不合并两套协议

`plugins/process-provider` 从「只会 access」补齐到 Wake / Clock / Route / TurnError / Tool。

**不合并** `adapters/process` 与 `plugins/process-provider` 这两套协议。理由：

- 前者是**平台适配器载体**（`Send` / `Call` / `Catalog` / `ConnectionsList`），一个连接
  一条管道，生命周期跟着连接；
- 后者是**策略 provider 载体**（`Decide { point, options, request }`），一个插件回答多种
  决策点，无连接概念。

合并会强迫每个写策略插件的人实现连接语义，反之亦然。`CoreMessage::Decide` 的 `point`
已经是字符串，**Process 载体补扩展点不需要新协议变体**，只需要在 process-provider 里
补对应的 manager——这是本次工作量最小的路径。

## 决策四：本轮不做的两件

- **wasm-host 接扩展点**：需要 wasm ABI 设计（导入表、内存约定、错误传播），是一次大
  设计，混进来会拖住房间发现。单独开题。
- **PromptProvider 外部化**：适合外部化，但没有真实用例，先不做。

这两条**登记为已知不对等**，写进本文的矩阵里并标 `未实现`，避免下一个人以为已经对等。

## 验收标准（这就是「对等」的判据）

1. 矩阵里 Process 那行从 `access` 扩到与 Lua 同列（Wake / Clock / Route / TurnError / Tool）。
2. 房间发现三种载体都能提供：
   - 内置 onebot11：真实调 `get_group_list` / `get_friend_list`
   - `adapters/process`：转发 `RoomsList`，用例里用假子进程回 `Rooms`
   - `adapters/dynamic`：不新增 ABI 字段，用例里用假 .so 回 `Rooms`
3. 旧适配器（不认识 `RoomsList`）不会让桥崩：回 `Error` 或超时 → `supported = false`。
4. 控制台 `GET /api/rooms?connection=X` 返回 `{supported, rooms}`，缓存 N 分钟
   （N=0 不缓存，**用户裁定**），前端有列表给下拉、没有保留手输。
5. `deploy/verify.sh` 全绿。

## 落地记录（2026-11-16）

分五个提交，一格一提交（便于单独回滚与 review）：

| 提交 | 内容 |
|---|---|
| `7527879` | 第一层：房间发现打通 wire / `Outbound::list_rooms` / 内置 adapter |
| `888ab28` | Process: wake（关键：`channel.decide` 的 `point` 不再写死） |
| `7f56f05` | Process: clock |
| `acb1027` | Process: route（这一格真的扩了 wire：`WireRouteDecision` / `WireRouteAction`） |
| `62db559` | Process: turn_error（wire 早就有，缺的只是载体侧的接线） |

三条实测出来的判断，写下来省下一次：

1. **补扩展点大多不需要新协议**。`CoreMessage::Decide { point, options, request }`
   的 `point` 本来就是字符串，换个值就是换扩展点。wake / clock / turn_error
   三格协议零改动。
2. **只有 route 需要真扩协议**，因为它的判定带**动作**（发什么、跑哪个 handler、
   打断谁）。把它压成 `decision: String` 会把协议变成解析题，那种形状没法给
   第三方插件用。
3. **「不对等」的形态常常是「缺接线」而不是「缺设计」**。turn_error 的 wire
   形状齐全，但没有任何载体实现它——只看设计文档会以为这个能力已经有了。

## 已知不对等（本 ADR 未解决，带原因）

| 缺口 | 为什么本轮不做 |
|---|---|
| `wasm-host` 零扩展点 | 需要 wasm ABI 设计（导入表、内存约定、错误传播），是一次独立的大设计，混进来会拖住房间发现 |
| `PromptProvider` 只有内置 | 适合外部化（人格提示、上下文注入），但没有真实用例；等有人真的要从外部注入提示词再做 |
| `Tool` / `ToolAccess` 外部化 | `Tool` 目前只能通过 `process-tools` 这个专用 crate；`ToolAccess` 只有 kernel 实现。外部插件无法参与工具授权判定 |
| Lua 缺 Clock | Lua 载体的 `ClockProvider` 没实现，而 dynamic 与 process 都有 |

## 已知代价

- 每次打开下拉可能要打一次平台 API；群多/被限流时慢。缓存 N 分钟是缓解，N 由部署决定。
- `WireRoom.label` 里的群名是对外数据，控制台展示时按不可信文本处理（不拼 HTML）。
