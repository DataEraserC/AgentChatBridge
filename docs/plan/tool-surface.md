# 工具面三层：常驻 / 目录 / 过滤

状态：**设计 + 最小实现（2026-11-05）**。工具总数不变（29），不新增
`tools_describe`；目录入口复用 `capabilities`，因此 `capabilities` 的计数与文档
不需要改数字。

## 1. 三层

```text
常驻层（RESIDENT_TOOLS，10 个，显式白名单）
  └─ 每条 harness 启动就注入；高频路径不搞两阶段
目录层（capabilities scope=catalog）
  └─ 长尾工具的 name + 一句话用途 + 何时用 + 可见性/可申请性
过滤层（capabilities scope=surface）
  └─ 按会话/场所/身份算出本轮 visible + hidden；harness 用它选工具
```

### 常驻白名单（固定 10 个）

`send_message` / `send_message_to` / `list_rooms` / `capabilities` /
`platform_call` / `access_request` / `access_ticket_get` / `ack_messages` /
`fetch_records` / `get_history`。

- `ack_messages` 是结束轮次前的确认工具；它和 `access_request` /
  `access_ticket_get` / `capabilities` 一样**永不被过滤**。
- 白名单在 Rust 里是 `crates/kernel/src/tools.rs` 的 `RESIDENT_TOOLS`；在 JS 里是
  `harnesses/shared/tools.mjs` 的 `RESIDENT_TOOL_NAMES`。两边由
  `tool_schema_consistency.rs` 锁住，禁止反向定义（「除长尾外都常驻」会让新工具
  自动膨胀）。

### 目录层入口

**入口名：`capabilities`，`scope=catalog`。** 不加新工具，理由：

- `capabilities` 已经在常驻白名单里，模型永远能调到；
- 不新增工具就不会改变 `TOOL_NAMES` / `tools.mjs` / `capabilities` 计数；
- 目录和权限判定天然应该在一起。

返回（条目只在不可直调时多带权限字段）：

```json
{
  "scope": "catalog",
  "resident": ["send_message", "..."],
  "tools": [
    {
      "tool": "notes",
      "purpose": "房间共享笔记（op = list / set / del）",
      "when": "需要跨轮记住某个房间的约定或状态时"
    },
    {
      "tool": "extensions_list",
      "purpose": "列动态扩展模块",
      "when": "管理员排查扩展时",
      "direct_executable": false,
      "requestable": false,
      "ticket_required": false,
      "reason": "admin_only"
    }
  ],
  "count": 19
}
```

可执行条目不再带 `available: true` / `reason: null` 这类空字段；没有
`direct_executable=false` 就是可执行。`scope=session` / `scope=tool` 仍给完整字段。

`capabilities scope=tool name=<工具>` 返回单个工具/op 的完整权限字段。**工具的完整
JSON schema 由 harness 本地补**：dsh / opencode 的 `callBridge` 在收到桥的结果后
用 `withLocalSchema` 把 `shared/tools.mjs` 里的 `description` / `schema` 拼进去。
core 不复制第二份 schema，也不做第二份权限判定。

### 过滤层入口

**入口名：`capabilities`，`scope=surface`。** 返回（这是 harness 每轮拉取的内部视图，
只放选工具真正要用的字段）：

- `resident`：常驻白名单（只有名字，永远全部可见）；
- `catalog`：全部非驻留工具的索引。可执行条目只有 `tool` + `purpose`；
  不可执行条目只有 `tool` + `direct_executable=false` + `requestable` +
  `ticket_required` + `reason`（本轮不会注入，所以不带 `purpose`）；
- `allow_destructive`：当前房间连接的破坏性放行状态；
- `counts`：total / resident / catalog / visible / hidden。

不再重复列 `visible` / `hidden` 数组：可见集 = `resident` ∪ `catalog` 里不带
`direct_executable=false` 的条目。这样同一事实只有一个来源，也省掉每轮重复的
29 个工具名。`when`、完整权限字段留给 `scope=catalog` / `scope=tool`。

harness 只把可见集里的工具注入本轮工具面：常驻工具用完整 schema，长尾工具用
一个最小占位 schema（`additionalProperties: true`），描述里指向
`capabilities scope=tool`。

## 2. 不变量（测试锁死）

**过滤只影响「常驻可见性」，绝不缩小可申请范围。**

1. 被过滤掉的工具/op 仍能通过 `capabilities` 查到，含 `requestable` /
   `ticket_required`。`scope=session`、`scope=catalog`、`scope=tool` 都不做过滤。
2. 被过滤掉的 op 仍能走 `access_request` 申请票据，批准后照常执行。过滤层不碰
   ticket store、不消费票据。
3. `access_request`、`access_ticket_get`、`capabilities`、`ack_messages` 永不被过滤：
   常驻白名单无条件进 `resident` / `visible`，即使当前 `available=false`。
4. 过滤判定复用既有 helper，不写第二份权限逻辑：
   - `tool_unavailable_reason` → `probe_room_reason` → `check_access(..., probe=true)`；
   - 平台 op 的 `direct_executable` / `allow_destructive` → `connection_allows_destructive`
     （现有 `cap_op_json` 已经这么算）；
   - `requestable` → `TICKET_OPS`（`notes:*`、`platform_call:<op>` 是词表里已有的模式）。

`direct_executable` 和 `requestable` 是**展示字段**，不是新的闸门：`direct_executable`
只是 `available` 的别名；`ticket_required = requestable && !direct_executable`。

## 3. 两阶段成本

常驻层 10 个工具、长尾 19 个。启动时只注入常驻层，避免每条 harness 一开机就付
19 份完整 schema。长尾工具在本轮被判定为可直接执行时，用最小占位 schema 注入；
模型真的需要参数时再调 `capabilities scope=tool name=...` 拿完整 schema。

实测（2026-11-05，29 个内置工具，JSON 紧凑序列化）：

| 形态 | 字节数 | 粗估 token（按 4 B/token） |
| --- | ---: | ---: |
| 改动前：29 份完整 schema 常驻 | 32,785 | 8,196 |
| 改动前：10 份常驻 schema + 旧 surface（6,303 B） | 22,154 | 5,538 |
| 改动后：10 份常驻 schema + 新 surface（1,762 B） | 17,613 | 4,403 |

surface 本身从 6,303 → 1,762 字节（-72%）；“空轮次”总成本比 29 份全量常驻
少 46%。修改工具目录或 purpose 文案时，`surface_response_stays_within_budget`
会卡住 2 KiB 上限。

对高频调用（发消息、列房间、票据、能力自检）不搞两阶段：它们本来每轮都可能用，
多一次 schema 查询是净亏。对长尾工具（`extensions_*`、`plugin_*`、`memory_*`、
`notes`、`tasks`、`feedback`）多一次查询通常可接受，尤其是它们本来就低频。

## 4. 两个 harness 的落地位置

**dsh**（`harnesses/dsh`）：

- `src/driver.mjs` 的 `TurnDriver` 持有 `this.surface`；每轮 `#one` 在投话前调
  `callBridge(link, "capabilities", {scope:"surface"}, {internal:true})`。失败时
  **退回常驻白名单**（`residentSurface()`，10 个），把 `surfaceStatus` 记成
  `surface_fallback` 并记日志；不保留上一轮的长尾工具，也绝不退回 29 个全量。
  `internal` 不计入 `turnCalls`，避免把内部查询误报成「模型调了工具」。
- `src/mcp-core.mjs` / `src/mcp-server.mjs` 的 `tools` 参数支持**数组或函数**；
  `main()` 传 `() => driver.toolDefs()`，所以 `tools/list` 每次按当前 surface 算。
- `toolDefsForSurface` 用 `shared/tools.mjs` 的 `toolsForSurface`，与 opencode 同源。

**opencode**（`harnesses/opencode`）：

- `plugin/index.js` 的 `setup` 先只 `ctx.tool.transform` 注册常驻层；
- `TurnDriver.#refreshSurface` 拿到 surface 后调用 `applySurface`，它再次走
  `ctx.tool.transform`：删掉不在可见集的旧工具，补上新的长尾工具；surface 查询
  同样标 `internal`，不计入本轮的模型工具调用数。失败时先 `applySurface(residentSurface())`
  把工具面缩回常驻层，再记 `surfaceStatus = "surface_fallback"`。
- `toolSpecsForSurface` 与 dsh 的 `toolDefsForSurface` 共用 `toolsForSurface`，
  一致性测试直接跨 harness 比对两边的工具名。

## 5. 测试

- `crates/kernel/src/tools.rs`：`resident_set_is_explicit_and_bounded`、
  `catalog_covers_every_non_resident_tool`。
- `crates/kernel/tests/capabilities_tool.rs`：
  `surface_splits_resident_catalog_and_hidden`、
  `surface_response_stays_within_budget`（≤ 2 KiB，且锁住瘦身字段规则）、
  `catalog_scope_is_the_long_tail_index`、
  `resident_entry_points_are_never_filtered_out`，以及 session/tool scope 的
  `requestable` / `ticket_required` 断言。
- `crates/kernel/tests/access_ticket_flow.rs`：
  `critical_is_refused_by_allow_destructive_and_runs_only_via_ticket`
  先断言 critical op `direct_executable=false` / `requestable=true`，再走
  `access_request` → 管理员批准 → 执行，证明过滤没缩小申请范围。
- `crates/kernel/tests/tool_schema_consistency.rs`：
  `resident_names_match_the_shared_schema`（Rust/JS 常驻白名单一致），
  以及原有的 `tool_names_match_the_shared_schema` /
  `access_request_description_lists_exactly_the_kernel_ticket_ops`。
- `harnesses/dsh/test/tool-surface.test.mjs`：
  同一 surface 下 dsh 与 opencode 选出的工具名完全一致；surface 收窄时两边一起收窄。
- `harnesses/dsh/test/driver.test.mjs` 与
  `harnesses/opencode/test/plugin.test.mjs`：
  `surface_failure_falls_back_to_resident_only`（先成功补上长尾，再让假桥拒绝
  surface，断言工具面缩回 10 个常驻、不含长尾也不含全量）；
  启动只挂常驻白名单；开轮后补可见长尾、隐藏不注入；
  `capabilities scope=tool` 在 harness 侧补完整 schema。
- `harnesses/dsh/test/mcp-server.test.mjs`：
  `tools/list` 支持函数形式的工具面（同一 server 两次 list 能返回不同集合）。

## 6. 边界

- 过滤只决定「本轮注不注入」；`tools/call` 仍接受任何已知工具名，桥照常做权限判定。
  因此隐藏工具不会因为看不见而变成不可调用——只是不在模型上下文里出现。
- `capabilities scope=surface` 是一次读操作，不消费票据、不改变状态；harness 拉取
  失败时退回常驻白名单（状态 `surface_fallback`），不影响本轮对话，也不扩大工具面。
- 目录层只覆盖内置工具。平台 op 的目录仍在 `capabilities scope=room` /
  `platform_call` 里，按同样的 `cap_op_json` 给出 `direct_executable` /
  `ticket_required` / `allow_destructive`。
- 不新增 `tools_describe`；如果未来目录需要独立工具名，再显式改
  `TOOL_NAMES` / `tools.mjs` / 计数测试，而不是悄悄加。
