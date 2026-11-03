# 回调 panic 策略（2026-11-04）

状态：Observer / Route / ToolProvider 的 catch 已实施；PromptProvider 已有线程隔离；
动态库 / FFI 仍需插件自己 catch。测试见 `callback_panic.rs` 与 `panic_stress.rs`。

范围：`RecordObserver` / `RouteProvider` / `ToolProvider` / `PromptProvider`。

## 0. 结论摘要

| 回调 | 建议 | 失败语义 | 理由 |
|---|---|---|---|
| `RecordObserver` | **必须 catch** | 跳过该观察者，记录继续调度 | 记录已经落库；panic 不能挡住唤醒/水位，也不能毒化 `Mutex<Bridge>` |
| `RouteProvider` | **必须 catch** | 按 `Pass` 处理并记错误 | 路由是命令/匹配的优化层，不是安全闸门；丢消息比漏命令更糟 |
| `ToolProvider` | **必须 catch** | 返回 `[tool_panic]` 错误，本轮继续 | 工具调用由 agent 发起；插件 panic 不能把桥或轮次带走 |
| `PromptProvider` | **已经隔离** | 跳过该来源 | 已在线程里调用，channel 断开即视为失败 |
| 动态库 `.so` 回调 | **不能靠 core catch** | 进程退出，由 supervisor 拉起 | panic 不能跨 `extern "C"` 边界安全 unwind；必须由插件自己在 FFI 入口 catch |

原则：**能安全恢复的进程内回调 catch；跨 FFI 的 panic 让它退出。**

---

## 1. 不变量风险

### 1.1 `RecordObserver`

调用点在 `ingest` 里、`store.append` 之后、唤醒调度之前（`engine.rs`）。
panic 的后果：

- 记录已经落库，但本轮没有排后续唤醒 → 消息“写进去了却没人被叫醒”，直到下一条
  消息或时钟轮才可能补上；
- panic 穿过持有 `Mutex<Bridge>` 的调用栈，锁被 poison（当前所有取锁点用
  `into_inner` 恢复，但状态可能停在半路）；
- 观察者是只读旁路，core 没有它能写的账本，所以 catch 后不需要回滚数据。

catch 后必须保证：记录已落库、其余观察者继续、唤醒调度继续、没有新的
`pending_*` 被观察者写进去。

### 1.2 `RouteProvider`

路由在入库前决定 `Pass` / `Drop` / `Handle`。panic 可能发生在：

- 匹配阶段：还没改任何 core 状态；
- 返回动作阶段：core 只在收到完整 decision 后才写 `pending_route_actions` /
  store；provider panic 不会留下半个动作。

catch → `Pass` 的理由：`ReceiveProvider` 才是入站安全闸门，路由是命令/规则的
匹配层；把 panic 当成 `Drop` 会静默丢消息，违背“Drop 必须留痕”的可观测性要求。
记录一行带 provider id 的错误日志，下一轮仍能看到消息。

### 1.3 `ToolProvider`

`plugin_tool_call` 在 `tool_call` 里同步调用，持有 `Mutex<Bridge>`。panic 后果：

- 锁 poison；当前恢复方式能继续跑，但本轮可能停在“调用已发起、结果没回来”的
  中间态；
- 插件自己的外部副作用无法回滚（core 不替它记账）；
- agent 收到的是连接断开或笼统失败，无法区分“插件坏了”和“桥坏了”。

catch 后返回 `ToolError::Internal`，消息带 `[tool_panic] <provider>: <tool>`。
core 在调用前没有写 pending 动作，catch 后只需保证轮次仍 active、票仍有效、
其它工具可用。

### 1.4 `PromptProvider`

已经在线程里调用，provider panic 使 channel 断开，core 记
`kernel.prompt.panicked` 并跳过该来源。这条路径不需要额外 catch；它不是本轮
新引入的风险。

### 1.5 FFI / 动态库

`.so` 回调通过 `extern "C"` 函数表进入插件。Rust panic 跨 C ABI 展开是未定义行为，
现代 Rust 默认会 abort。core **无法**在调用侧 catch：

- 对 `.so` 插件，catch 必须发生在插件自己的 `acb_plugin_entry` / `invoke` 内部，
  把 panic 转成 wire error；
- core 侧只能接受“进程退出 + supervisor 重启”的结果；
- 不能让 core 用 `catch_unwind` 包住 FFI 调用并假装安全——那只是把未定义行为
  藏起来。

因此动态库的 panic 策略是：**退出**。恢复由 supervisor（退出码/重启策略）承担，
插件作者负责在自己的 FFI 边界 catch 并返回错误。

---

## 2. 可观测性

catch 之后不能静默。每个 panic 至少记录：

- 回调类型（observer / route / tool / prompt）；
- provider id / 工具名 / 房间 / 会话；
- panic 文本（`catch_unwind` 的 `Box<dyn Any>` 尽量 downcast 成 `&str` / `String`）；
- 处理结果（skipped / pass / tool error）。

当前默认 panic hook 会把 panic 打到 stderr → journald；catch 后额外加一行结构化日志，
避免只有“某处 panic”而不知道是哪个插件。计数可以后续进 `plugin_errors` 或
控制台健康状态，本轮不要求。

---

## 3. catch 后必须断言什么

1. **锁没有中毒**：catch 发生在 guard 内层，不能让 panic 穿过 `MutexGuard` 的析构；
   catch 点必须在调用 provider 的同一函数内，而不是更外层。
2. **没有半完成的 core 写入**：
   - Observer：记录数只增加了这一条；没有新增 `pending_*`；唤醒调度继续。
   - Route：`Pass` 路径不写 store；没有 `pending_route_actions` / `pending_interrupts`。
   - Tool：轮次仍 active，票仍有效；没有 `pending_notices` / 路由动作。
3. **其它 provider 不受影响**：Observer 循环跳过坏的那一个，继续后面的；
   Route 只影响当前这一条；Tool 只影响这一次调用。
4. **可重试/可观测**：下一次 ingest / tool_call 能正常工作；错误日志能定位到插件。
5. **不能假装成功**：Observer 不能报告“已观察”；Route 不能返回 `Handled`；
   Tool 不能返回空值或成功。

---

## 4. 不建议 catch 的情况

- **动态库 / FFI 回调**：见 §1.5；catch 只能由插件自己做。
- **core 自身的 panic**（不是插件回调）：应当退出，暴露 bug，不要在生产里吞掉。
- **panic 后需要回滚外部副作用**：core 没有插件的账本，catch 只能保证 core 状态；
  如果插件要求“要么全做要么全不做”，那属于插件自己的事务设计，不是 core 的 catch
  能解决的。

---

## 5. 实施状态（2026-11-04）

1. Observer：已包 `catch_unwind`，panic 跳过该来源并记日志；测试覆盖记录入库、唤醒、
   其它观察者、再次调用、锁不中毒。
2. Route：已包 `catch_unwind`，panic 按 `Pass` 处理并记日志；测试覆盖下一条路由继续、
   入库/唤醒、`pending_route_actions` 为 0。
3. Tool：已包 `catch_unwind`，返回 `[tool_panic]` 错误；测试覆盖同轮其它工具可用、
   再次调用仍返回错误、下一轮工具索引可用。
4. Prompt：保持现有线程隔离；`prompt_flow` 固定 panic 后跳过。
5. 动态库：不提供 core catch；插件开发文档需写清“FFI 入口必须自己 catch_unwind，
   返回 wire error；否则进程退出由 supervisor 重启”。
6. 压力验证：`panic_stress.rs` 连续 200 次 Observer+Route+Tool panic，断言
   `queued_turns` / `pending_route_actions` 无残留、RSS 增长在宽松上限内、provider
   生命周期与 `lastError` 不变、压力后仍可服务。
7. `manager.instantiate`：所有解析/实例化入口已改走 `instantiate_safe`；panic 只把
   该 provider 标为 `Health::Unhealthy` 并写 `lastError`，其它 provider 不受影响。
   被 `config.toml` 引用的坏 provider 会在 `validate_plugins` 启动检查里返回可行动错误。
   测试见 `instantiate_panic.rs`（失败不新增注册、好 provider 照常、lastError 可见）。
