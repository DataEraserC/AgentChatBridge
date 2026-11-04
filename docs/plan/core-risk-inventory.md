# core 风险清单（2026-11-04）

范围：`acb-kernel` / `acb-proto` / `acb-bridge` / `acb-adapter-api` 及 adapters 的 core 路径；
不含仅测试用的 fixture cdylib（`adapters/dynamic-fixture`、`plugins/wake-fixture`）。
行号对应当前工作树（clippy 清零 + manager panic 兜底之后）。

优先级定义：

- **P0**：当前就可能让整进程不可用或损坏持久状态，需要立即修。
- **P1**：规模化、异常输入或坏插件下会拖垮服务、长期泄漏或无法自愈。
- **P2**：局部、低概率或只影响可诊断性；可在相关改动时顺手做。

**结论**：本次没有发现 P0。原 P1 共 6 项；2026-11-04 跟进已修 4 项（relay 锁内复制、
process `stdin` 持锁写、relay 过期目录清理、动态库句柄超时隔离），剩余 2 项待拍板
（入站队列上限、路由队列上限，见 `queue-backpressure.md`）。动态库方案 D 只隔离句柄，
**卡住的插件线程仍无法回收**，作为已知残留保留 P1。

---

## 1. unsafe 清单

`acb-kernel` **零 unsafe**；`acb-proto` **零 unsafe**。

| 位置 | 用途 | 安全前置条件是否文档化 | 评估与动作 | 优先级 |
|---|---|---|---|---|
| `crates/bridge/src/main.rs:55` | `libc::signal` 注册 SIGTERM/SIGHUP | 是：上方 SAFETY 注释说明 handler 只做原子写 | handler 只 `store` 原子量，满足 async-signal-safe。无需改 | — |
| `crates/adapter-api/src/lib.rs:43-56` | C ABI 函数指针类型别名 | 部分：模块头说明 JSON 形状；`HostV1` 文档要求插件复制回调表。单个 `unsafe fn` 指针的调用前置条件未逐条写 | 建议补一段 ABI 安全契约（指针生命周期、字符串所有权、回调线程约束）。属文档债 | P2 |
| `crates/adapters/dynamic/src/lib.rs:75-89` | `dlopen`、取入口符号、读函数表 | 部分：模块头 + 紧随其后的 ABI 版本/结构大小/标志校验 | 校验在解引用后立刻做，但 `*table_ptr` 本身要求插件入口返回合法指针；这是插件契约。建议在模块头明确“插件入口必须返回进程内有效、静态生命周期的表” | P2 |
| `crates/adapters/dynamic/src/lib.rs:135,151` | 调 `manifest()` / `create()` | 否（逐块） | 前置：插件按 ABI 返回 NUL 结尾字符串、create 返回非空 handle。失败路径已处理，但契约未写 | P2 |
| `crates/adapters/dynamic/src/lib.rs:166,178` | `UnloadSafeLibrary::invoke` | 是（本次补 `# Safety`） | handle 必须来自同一 loader 的 `create` 且未 destroy、串行使用 | — |
| `crates/adapters/dynamic/src/lib.rs:186,188` | `UnloadSafeLibrary::destroy` | 是（本次补 `# Safety`） | handle 只 destroy 一次，之后不得再用 | — |
| `crates/adapters/dynamic/src/lib.rs:199-211` | `take_c_string`：读插件返回字符串并由插件 `free_string` 释放 | 否 | 前置：指针必须由插件分配、NUL 结尾、只能 free 一次。错配会 UB；建议在函数上补注释，并把 `free_string == None` 视为加载期拒绝 | P2 |
| `crates/adapters/dynamic/src/lib.rs:440,584-589,627-668` | v1/v2 loader 探测与 manifest | 部分（同 75-89） | 同上 | P2 |
| `crates/adapters/dynamic/src/lib.rs:720,945,1329` | `create` 传入 `HostV1` 指针 | 部分：`DylibOutbound` 字段注释说明 `_context` 保证 ctx 有效 | 前置：`host` 在句柄存活期内一直有效；`Arc<HostContext>` 字段已保证。建议在该字段注释里显式写“销毁顺序：先 destroy handle，再 drop context” | P2 |
| `crates/adapters/dynamic/src/lib.rs:1794` | `unsafe impl Send for AdapterHandle` | **否**，无注释 | `AdapterHandle` 含裸指针，Send 的依据是“所有访问都在 `Mutex<AdapterHandle>` 内”。这是真实的安全断言，建议补 SAFETY 注释 | P2 |
| `crates/adapters/dynamic/src/lib.rs:1920` | 持 handle 锁调插件 `invoke` | 是：注释说明 handle 非线程安全、destroy 必须排队 | 安全性成立；性能/挂死风险见 §3 | — |
| `crates/adapters/dynamic/src/lib.rs:1962-1964` | `take_owned_string` + `free_string` | 否（同 199-211） | 同上 | P2 |
| `crates/adapters/dynamic/src/lib.rs:1998` | `destroy` 裸指针 | 部分 | handle 存活判断在锁内；契约同上 | P2 |
| `crates/adapters/dynamic/src/lib.rs:2026-2092` | `extern "C"` 回调 `submit_event` / `should_stop` / `log` | 部分是：空指针检查 + `catch_unwind` | 回调不能 panic 跨 FFI；当前已 catch。`should_stop` 不 catch 但只做原子读，安全 | — |
| `crates/plugins/wake-fixture/tests/policy.rs:91+` | `std::env::set_var`（Rust 2024 unsafe） | 是：文件头说明单测试文件串行 | 测试专用 | — |

**结论**：没有未文档化到“可能立刻误用”的程度；缺口集中在动态库 ABI 的逐条契约与 `unsafe impl Send` 注释（P2 文档债）。

---

## 2. unwrap / expect / panic! / 索引越界

统计口径：`crates/**/src/*.rs` 中 `#[cfg(test)]` 之前的代码视为生产路径；集成测试与
src 内测试模块单列。

### 2.1 生产路径

| 位置 | 内容 | 影响 | 建议动作 | 优先级 |
|---|---|---|---|---|
| `crates/kernel/src/plugin.rs:170-215` | 10 个内置 provider id 常量用 `.expect("built-in provider id must be valid")` | 常量写错会在首次访问时 panic；属于编译期可验证的常量 | 保持；可改为 `const` 解析或启动自检。当前不构成运行期风险 | P2 |
| `crates/kernel/src/plugin.rs:1711` | `with_native_builtins()` 中 `install_loader(...).expect(...)` | 内置注册失败 = 代码 bug，启动即 panic（fail-fast） | 保持；在启动路径由 supervisor 退出码兜底 | P2 |
| `crates/kernel/src/plugin.rs:2823` | `collected.last().expect("just pushed")` | 刚 push 后取末元素；不可达 | 保持或用 `if let Some` 消掉 | P2 |
| `crates/kernel/src/engine.rs:2309,2331` | `pending.remove(index).expect("just found")` | index 来自同一次 `find`，安全 | 保持 | P2 |
| `crates/kernel/src/engine.rs:3183` | `active.get(session).expect("a ticket always has an active turn")` | `by_ticket` 与 `active` 只在 `open_turn` / `end_turn` 同步增删，当前不变量成立 | 建议改成返回 `ToolError`，把“票有但轮次没了”降级为可诊断错误，避免未来改状态机时炸 | P2 |
| `crates/kernel/src/engine.rs:3770` | `Ticket::new(...).expect("ticket must not be empty")` | 拼接串至少含两个 `-`，不会空 | 保持 | P2 |
| `crates/kernel/src/engine.rs:1679` | `unreachable!("pass must not be placed in handled")` | `handled` 只在非 Pass 分支赋值，安全 | 保持 | P2 |
| `crates/kernel/src/storage.rs:870` | `feedback_keys` 命中的 id 必须在 `feedback` 向量里 | 删除路径同时清理两边，当前成立；若未来加裁剪易破 | 建议改用 `get_mut` + 缺失时重建/记日志，或加 debug 断言 | P2 |
| `crates/kernel/src/endpoint.rs:759` | `reader_of` 中 `try_clone().expect(...)` | 文档写明“给测试用”，但函数在 lib 中公开 | 可保持；若要更稳，返回 `io::Result` | P2 |
| `crates/kernel/src/engine.rs:2302-2304` | `self.pending[index]` | index 来自 `0..len()`，安全 | 保持；可改 `get(index)` | P2 |
| `crates/kernel/src/engine.rs:3660-3678` | 同 `(session, room)` 去重后入队 | 队列上限 = 配置里的订阅对数，**不是**无界 | 无需改 | — |

生产路径**没有**直接 `panic!` / `unwrap()`（除上述 expect 与常量）；`crates/bridge/src/*`
的生产代码零 `unwrap/expect`，全部集中在 `#[cfg(test)]` 之后。

### 2.2 测试代码

- `crates/kernel/src/*.rs` 测试模块约 280 处 `expect/unwrap`，均在 `#[cfg(test)]` 内。
- 集成测试 `crates/kernel/tests/*.rs`、`crates/adapters/*/tests/*.rs` 里的 `expect/panic!`
  属于测试断言，不进入发布二进制。
- 结论：测试用 panic 不计入运行风险；不在本清单逐条展开。

### 2.3 索引

- 生产路径未发现可直接越界的裸索引；唯一一处 `self.pending[index]` 由同长度 `find`
  产生（见上表）。
- `crates/kernel/examples/core_bench.rs:134` 的 `sorted[index]` 是基准示例，索引来自
  排序后的枚举，非核心路径。

---

## 3. 锁：中毒与持锁 I/O

### 3.1 现状

- 没有 async 运行时（全仓库无 `async fn` / `tokio` / `.await`），因此**不存在跨 await
  持锁**；对应风险是“持锁做阻塞 I/O”。
- 生产代码没有 `lock().unwrap()` / `read().unwrap()`；所有取锁点统一
  `unwrap_or_else(|poisoned| poisoned.into_inner())`，中毒后继续用。
- 全局锁是 `Arc<Mutex<Bridge<S>>>`：`kernel/endpoint.rs:690`、`kernel/event_queue.rs:63`、
  `kernel/route_worker.rs:236`、`bridge/console.rs:852`、`bridge/main.rs`。

| 位置 | 问题 | 影响 | 建议动作 | 优先级 |
|---|---|---|---|---|
| `crates/kernel/src/engine.rs:4145,4366`（`begin_tool_call` → `relay_elements` → `FileRelay::publish`） | **持全局 Bridge 锁做 `fs::copy`**（单文件上限 128 MiB） | 一次大文件中继会阻塞入库、开轮、控制台；文件大或磁盘慢时表现为整桥卡顿 | **已修（c912a63）**：锁内只登记 `PreparedSend` / `PreparedPlatformCall`，endpoint / route worker 锁外先 `relay()` 再执行；publish 用临时文件 + rename | 已修 |
| `crates/kernel/src/engine.rs:3041,4392`（`platform_call` 参数中继） | 同上 | 同上 | 同上（同一提交） | 已修 |
| `crates/bridge/src/console.rs:706`（以及 `:852` 的 `lock`） | 持全局锁执行 SQLite 读（feedback/history 等） | 慢查询或大库会阻塞 ingest；控制台高频轮询放大 | 控制台查询先 clone 所需状态或使用只读连接；至少避免在锁内做多页查询 | P2 |
| `crates/kernel/src/outbound.rs:734-762` | `OutboundHandle` 的每连接 `Mutex` 覆盖 `send/call` 全程（含网络 I/O） | 单连接串行是刻意设计；但慢平台/挂死平台会让该连接所有发送与 `platform_call` 排队，直到适配器超时 | 保持串行语义；给适配器层补明确超时与“连接不可用”状态（process/dynamic 已有部分） | P2 |
| `crates/adapters/process/src/lib.rs:737-813` | `stdin` 锁持有期间做阻塞 `write_all` + `flush` | 子进程不读 stdin 时写满管道，锁不释放；随后 shutdown 的 `write_line` 也卡住，只能等 kill | **已修（f0ea047）**：stdin 交给独立 writer 线程，调用方只入有界队列；队列满超过 5s 标记 `write_failed`，supervise 杀子进程并重启 | 已修 |
| `crates/adapters/dynamic/src/lib.rs:1911-1922` | handle 锁持有期间调插件 `invoke`（FFI 可能永不返回） | 该适配器后续所有调用、destroy 全部排队；`destroy_handle` 2s 后放弃，句柄泄漏 | 保持“插件必须可重入/可返回”的契约；超时后标记该 handle 不可用并停止再调用（见 §4） | P1 |
| 所有 `into_inner()` 恢复点（如 `kernel/endpoint.rs:54,64,80,693`） | panic 中毒后继续使用，可能读到半更新状态 | 当前回调 panic 已 catch，但未来新增 panic 路径仍可能留下不一致状态 | 建议在 `into_inner` 分支统一打一行“锁中毒，已恢复”日志，便于事后定位；关键状态机可加一致性断言 | P2 |
| `crates/extensions/src/lib.rs:301-327` vs `:380-404` | `reload` 持 `status` 锁再取 `live` 写锁；`details` 先克隆 `live` 读锁（立即释放）再取 `status` | 当前读锁在语句结束即释放，不构成反向持锁；但写法脆弱 | 建议统一锁序（先 status 后 live，或先取快照再取 status），并在注释里写明 | P2 |

### 3.2 结论

- 中毒：全部恢复，不 panic；风险是“恢复后状态可能半更新”，属可观测性/一致性债。
- 跨 await：不存在。
- 持锁 I/O：**两处 P1**（文件 relay、process stdin）；其余为可接受或有界。

---

## 4. 无界集合与长期增长

| 位置 | 集合 | 当前边界 | 增长场景 | 影响 | 建议动作 | 优先级 |
|---|---|---|---|---|---|---|
| `crates/kernel/src/event_queue.rs:41-44` | 入站 `mpsc::channel`（`EventQueue`） | **无界**（注释明说“队列无界，不阻塞平台读循环”） | 平台洪峰 / SQLite 写入变慢 / drain 线程卡住 | 内存持续上涨直到 OOM；适配器线程不背压 | 上限与超限语义设计见 `queue-backpressure.md`（待拍板） | P1 |
| `crates/kernel/src/route_worker.rs:19,29,36-44` | 路由动作队列 | **无界** | 路由插件批量产生 send/task，或平台 I/O 慢 | 同上 | 上限与超限语义设计见 `queue-backpressure.md`（待拍板） | P1 |
| `crates/adapters/dynamic/src/lib.rs:2056-2087` | 每次调用 `thread::spawn` 一个工作线程等插件返回 | 超时后不再新增调用，但已 spawn 的线程无法取消 | 插件挂死后，第一次超时线程永久卡在 FFI | **D 已实施（10791f8）**：超时标死句柄，后续 `send/call` 立即失败、catalog `Unhealthy`、新 handle 恢复；**残留**：卡住线程与库引用泄漏到进程退出 | P1（残留） |
| `crates/kernel/src/relay.rs:27,165-219` + `crates/bridge/src/main.rs:36-38,264-291` | 收下的文件目录 | 启动扫描 + **每小时**定期清理（保留 24h） | 长期运行、频繁发文件 | 磁盘无界增长；最坏写满分区 | **已修（ba39c88）**：过期文件最多多留 1h；删除失败只记日志 | 已修 |
| `crates/kernel/src/engine.rs:72,811` | `tasks: VecDeque<TaskRecord>` | 200 条，超出丢最旧 | 正常 | 旧任务查询丢失；内存有界 | 已知取舍；需要保留时落库（见 §5） | P2 |
| `crates/kernel/src/engine.rs:458,2400-2401` | `ended: HashMap<Ticket, EndedTurn>` | 每次 `end_turn` 按 `TURN_GRACE`（600s）清理 | 轮次持续结束 | 有界（最多 600s 内的结束轮次） | 可保持；若并发会话极多可再加硬上限 | P2 |
| `crates/kernel/src/engine.rs:452,3660-3678` | `pending: VecDeque<Pending>` | 按 `(session, room)` 去重，上限 = 订阅对数 | 配置订阅很多 | 有界于配置 | 无需改 | — |
| `crates/kernel/src/engine.rs:495-498,2084-2096` | `wake_audit` / `wake_rates` | 审计 100 条；限流窗口按时间清理，map 键按 provider 数 | 正常 | 有界 | 无需改 | — |
| `crates/kernel/src/engine.rs:486,490` | `plugin_errors` / `manager_errors` | 按 provider / namespace 数 | 正常 | 有界 | 无需改 | — |
| `crates/kernel/src/storage.rs:588-590,920` | `MemoryStore::feedback` / `feedback_keys` | 5000 条（`FEEDBACK_CAP`） | 仅测试/嵌入式 | 有界 | 无需改 | — |
| `crates/kernel/src/storage.rs:580-592` | `MemoryStore::records` / `watermark` / `deliveries` | **无界** | 仅测试/嵌入式长跑 | 仅开发场景 | 明确标注“仅测试/嵌入式”；生产用 `SqliteStore` | P2 |
| `crates/kernel/src/endpoint.rs:41` | `ControlRegistry::senders` | 按在线会话 | 连接注册/注销 | 有界（断连注销） | 无需改 | — |
| `crates/adapters/dynamic/src/lib.rs:1911-1922` | 每个适配器的 handle 锁 | 每适配器 1 | 插件挂死 | 调用排队而非内存增长 | 见上表线程项 | P1 |

---

## 5. 已知“暂时没做”、规模化会出问题的地方

| 项 | 位置/文档 | 规模化后的表现 | 当前处置 | 优先级 |
|---|---|---|---|---|
| 任务记录只在进程内 | `engine.rs:72,811`；`docs/plan/decision-brief.md` | 重启丢任务历史；200 条外查不到 | 已知取舍；需要时落 SQLite（与 feedback 表并列） | P2 |
| SQLite 单连接、与全局锁串行 | `kernel/engine.rs`（`store` 字段）+ `event_queue.rs` drain | 写入吞吐上限 = 单线程；大库查询会拉长锁持有时间 | 先做 §3 的控制台查询出锁；需要更高吞吐再考虑连接池/读写分离 | P2 |
| 控制台/健康检查与 ingest 共用全局锁 | `bridge/console.rs:852`、`endpoint.rs:690` | 高频轮询或大响应会抢锁 | 已把 plugin `health()` 做成 catch；查询侧见 §3 | P2 |
| `ws-server` 反向 WebSocket 未实现 | `crates/kernel/src/config.rs:244-245` | 只能正向 WS / process；平台侧起服务时不可用 | 配置能解析，但适配器 manifest 不声明；启动时拒绝（`config.rs:2721`） | P2 |
| memory 远端同步未做 | `docs/plan/decision-brief.md` | 多机共享记忆需手工 git push/pull；无自动收敛 | 用户已明确暂不开工 | — |
| 扩展工厂/插件分发未做 | `docs/plan/decision-brief.md` | 只能手工放文件/配置；无安装器/市场 | 保留 `extensions_*` + 控制台；用户未拍板 | — |
| 动态库 tool 载体 ABI 未定 | `docs/plan/decision-brief.md` | tool 目前只能 in-process（`ToolLoader`）；跨语言 tool 需进程方案 | 短期 in-process 可用；中期需在 `.so` 与 subprocess 之间选型 | — |
| 自动回滚/ADR-0030 D3 未实现 | `docs/plan/native-reload-rollback-options.md`；`dynamic/src/lib.rs:355-358` 注释 | 同路径 reload 失败后只能保持 unloaded；旧实现已 `dlclose` | 维持手动；可信健康判据未定 | P2 |
| 插件 manager 的 `manifest()` / `children()` / `details()` / `root()` 未 catch | `plugin.rs:1246-1282` 接口；调用点 `plugin.rs:2666/2823` 等 | 坏 manager 在这些只读接口 panic 会让安装/控制台路径 unwind（可能毒锁） | 本次已 catch `descriptors()` / `health()`；其余接口成本相仿，建议按同一 helper 补齐 | P2 |
| FFI 内 panic 无法由 core 安全 catch | `docs/plan/callback-panic-policy.md` §1.5 | 插件 panic 跨 `extern "C"` → abort/进程退出 | 策略已定：插件自己在 FFI 入口 catch；supervisor 拉起。属已知限制 | P2 |
| 动态库调用超时后线程无法安全取消 | `dynamic/src/lib.rs:58-70,2056-2087` | 超时线程仍卡在插件 FFI，无法强制中断 | **D 已实施（10791f8）**：句柄隔离 + 快速失败 + catalog 可见 + 新 handle 恢复；线程残留到进程退出（见 `dynamic-call-timeout.md`） | P1（残留） |
| 无入站限流/配额 | `event_queue.rs`/`engine.rs` | 单个平台/账号刷屏会挤占所有会话的入库与唤醒 | 可先做队列上限（§4），再评估按房间速率限制 | P2 |

---

## 6. `descriptors()` / `health()` panic 兜底（本次已做）

- 实现：`crates/kernel/src/plugin.rs:47-74`
  - `call_manager(namespace, "descriptors", ...)`：`catch_unwind` → `PluginError::Manager`；
  - `manager_health(namespace, manager)`：panic → `Health::Unhealthy`。
- 接入点：
  - 安装预检 `collect_manager`：`plugin.rs:2710`，panic 变成安装错误，且不落半棵子树；
  - 目录/健康聚合：`plugin.rs:1977`、`plugin.rs:2875`。
- 测试（`plugin.rs` 内）：
  - `panicking_descriptors_becomes_install_error`：安装返回错误、错误含 `descriptors` 与
    panic 文本、`manager_health()` 为空；
  - `panicking_health_is_reported_not_propagated`：`manager_health()` 返回
    `Unhealthy`，连续读两次仍正常（结构/锁未被破坏）。
- 证据：`cargo clippy --workspace --all-targets -- -D warnings` 通过；
  `cargo test --workspace` 54 个 suite 全部 `ok`。
- 未做：`manifest()` / `children()` / `details()` / `root()` / `reload()` / `shutdown()`
  仍未 catch。不是高成本，而是本次按用户指定范围只收口 `descriptors()` / `health()`；
  列为 §5 的 P2，可在下一次插件健壮性改动时用同一 helper 覆盖。

---

## 7. 建议处理顺序

1. **P1（已修）**：文件 relay 移出全局锁（c912a63）。
2. **P1（已修）**：process `stdin` 写入超时/独立写线程（f0ea047）。
3. **P1（已修）**：relay 过期清理挂到周期线程（ba39c88）。
4. **P1（待拍板）**：入站队列与路由队列上限，设计见 `queue-backpressure.md`。
5. **P1（已做 D，残留）**：动态适配器超时隔离句柄（10791f8）；卡死线程仍无法回收。
6. **P2**：补动态库 ABI 安全契约与 `unsafe impl Send` 注释；把 manager 其余只读接口
   纳入 `call_manager`；控制台查询出锁。
