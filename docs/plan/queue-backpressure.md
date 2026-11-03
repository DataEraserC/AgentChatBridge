# 入站与路由队列上限策略（设计，待拍板）

状态：只出设计，未改代码。
范围：`EventQueue`（适配器 → drain 线程）与 `RouteWorker`（drain → 路由动作线程）。
相关代码：`crates/kernel/src/event_queue.rs`、`crates/kernel/src/route_worker.rs`。

## 0. 结论摘要

| 队列 | 推荐上限 | 超限默认动作 | 内存上界（估算） |
|---|---|---|---|
| 入站 `EventQueue` | **4096 条** | **拒绝最新（reject newest）**，返回明确错误 + 计数 | 典型 < 20 MiB；单条按 32 KiB 最坏算约 128 MiB |
| 路由 `RouteWorker` | **1024 个动作** | **排队最多等 5 秒，仍满则拒绝最新** + 计数 | 典型 < 10 MiB；单动作按 32 KiB 最坏算约 32 MiB |

两条队列都保留 `0 = 无界` 作为显式逃生口，但默认有界。**不丢消息**的语义需要同步
改写成「已成功入队的不丢；超限是显式拒绝，不是静默丢失」。

---

## 1. 现状

### 1.1 入站队列

`event_queue.rs:35-44`：

```rust
let (tx, rx) = channel();          // std mpsc，无界
pub fn submit(&self, draft: RecordDraft) -> bool {
    self.tx.send(draft).is_ok()     // 只有接收端消失才返回 false
}
```

- 生产端：每条适配器连接一个线程，平台事件直接 `submit`。
- 消费端：core 一个 drain 线程，`ingest` 里写 SQLite、路由、唤醒。
- 当前唯一的“背压”是接收端断开；平台洪峰或 SQLite 变慢时，内存无上限增长。

### 1.2 路由队列

`route_worker.rs:29-44`：

```rust
let (tx, rx) = channel();          // std mpsc，无界
self.busy.fetch_add(1, ...);
self.tx.send(action)...
```

- 生产端：drain 线程（`ingest` 返回后 `worker.enqueue(actions)`）。
- 消费端：一个路由动作线程，执行发送/任务，可能等平台 I/O（最长 `REPLY_TIMEOUT`）。
- 路由动作来自插件命令；积压时 drain 线程仍会继续接收新记录，队列只增不减。

---

## 2. 候选值与内存上界

单条记录/动作的大小取决于文本与元素数量。平台文本上限常见 4k 字符（约 16 KiB
UTF-8），加上元素结构与房间/发送者字段，取 **32 KiB/条** 作为偏保守的最坏估算。

| 上限 | 典型占用（按 1 KiB/条） | 最坏占用（按 32 KiB/条） | 适用场景 |
|---|---|---|---|
| 1024 | ~1 MiB | ~32 MiB | 小群/低流量，最保守 |
| **2048** | ~2 MiB | ~64 MiB | 入站建议下限 |
| **4096** | ~4 MiB | ~128 MiB | 入站推荐默认 |
| 8192 | ~8 MiB | ~256 MiB | 高流量公网机器人 |
| 16384 | ~16 MiB | ~512 MiB | 不推荐作为默认 |

路由动作通常比入站记录小，但 `PreparedSend` 带 `Arc<Config>` 与元素列表：

- 推荐默认 **1024**（典型 < 10 MiB）。
- 若平台 I/O 偶发 10 秒超时、命令量大，可上调到 2048；不建议默认超过 4096。

**取值依据**：队列的作用是吸收突发，不是存档。桥的持久化在 SQLite；队列只需要覆盖
「一次网络抖动 / 一次慢查询」的时间窗。4096 条在 100 条/秒的洪峰下约 40 秒，足够
drain 线程恢复；再大只会把失败模式从「明确超限」变成「缓慢 OOM」。

---

## 3. 超限语义

### 3.1 拒绝最新（`reject`，推荐默认）

- 生产者 `try_send` 失败 → 返回明确错误（入站：`submit` 返回 `SubmitError::Full`；
  路由：`enqueue` 返回 `Full`）。
- 优点：已入队的消息保持时间顺序；不会出现「丢了中间一段但水位继续前进」的隐性洞。
- 缺点：最新消息进不了流水。对入站而言，适配器必须把失败当连接级错误处理
  （记录日志、断开/重连，让平台按自己的机制重发），否则就是丢消息。
- 对 `不丢消息` 的影响：把承诺缩小为「**接收并成功入队的消息不丢**」；超限必须可观测，
  不能静默。若要求端到端绝对不丢，只能选阻塞（§3.2）。

### 3.2 阻塞生产者（`block`）

- 生产者 `send_timeout` 等一小段时间；仍满则按 `reject` 处理。
- 入站阻塞会卡住适配器读循环 → WS/TCP 接收缓冲上升 → 平台侧最终断连或丢事件；
  这不是 core 能控制的不丢，只是把内存压力推给平台。
- 适合作为可选模式（如 `overflow = "block"`, `block_timeout_ms = 5000`），不建议默认。

### 3.3 丢最旧（`drop_oldest`）

- 需要自建环形队列（`Mutex<VecDeque>` + `Condvar`），`std mpsc` 无法从生产端弹出旧项。
- 优点：保留最新内容，适合「宁可少看历史也要看眼前」的场景。
- 缺点：会丢中间记录，且必须保证不产生水位洞；实现与测试成本最高。
- 不推荐作为默认；可作为后续可选。

### 3.4 对现有不变量的影响

- 流水模型（ADR-0005/0017）：记录入库后才算事实。队列满被拒绝的记录**从未入库**，
  不产生编号、不影响水位、不会被重投；因此不会造成「水位洞」。
- 代价是这部分消息对 agent 不可见。必须通过计数器、日志与健康状态让运维看见。
- 路由动作被拒绝不影响流水（命令本身可能已入库或按路由策略未入库），但对应命令不会执行；
  应写回执/日志，不能让插件以为已执行。

---

## 4. 可观测性

最低要求（不引入 metrics 依赖）：

| 指标 | 暴露位置 | 内容 |
|---|---|---|
| `inbound.queued` / `inbound.capacity` | 控制台 `/api/status`、`/healthz` | 当前深度与上限 |
| `inbound.rejected_total` | 同上 | 累计拒绝条数 |
| `inbound.blocked_total` | 同上 | 阻塞模式下等待过的次数 |
| `route.queued` / `route.capacity` | 同上 | 同路由队列 |
| `route.rejected_total` | 同上 | 累计拒绝动作数 |
| `route.busy` | 已有 `RouteWorker::busy()` | 在途 + 排队动作数 |

日志：首次拒绝与之后每 N 条（如 100 条）记一行，带房间/连接与当前深度，避免刷屏。
健康状态：连续拒绝超过阈值（如 1 分钟 1000 条）时把 `/healthz` 标为 `degraded`，
但不自动退出——按 supervisor 合同，退出只用于启动失败或显式重启。

---

## 5. `config.toml` 覆盖

建议新增独立根表，避免散落在 `[storage]` / `[endpoint]`：

```toml
[queues]
# 0 = 无界（旧行为，仅作为逃生口）
inbound_capacity = 4096
route_capacity = 1024
# reject | block | drop_oldest
inbound_overflow = "reject"
route_overflow = "reject"
# 仅 block 模式生效
block_timeout_ms = 5000
```

校验规则：

- `capacity = 0` 允许，但启动日志必须明确警告「无界队列」；
- `block_timeout_ms` 上限（如 60000），避免把 drain 线程挂死；
- 未知 `overflow` 值在启动时拒绝；
- 默认值写进 `Default`，现有配置不需要改一行。

---

## 6. 实施拆分（若批准）

1. **入站**：把 `std mpsc` 换成 `sync_channel` + `try_send`；`submit` 返回三态
   （`Ok` / `Full` / `Closed`）；适配器遇 `Full` 记日志并按连接错误处理。
2. **路由**：`sync_channel` + `send_timeout`（默认 5s）；`enqueue` 返回失败计数；
   drain 不因路由满而阻塞入库超过 `block_timeout_ms`。
3. **配置与观测**：新增 `QueuesConfig`、控制台字段、`/healthz` degraded 阈值。
4. **测试**：满队列拒绝、计数、恢复后可继续入队、路由满时不阻塞超过超时。
5. **不变量文档**：在 ADR/协议文档里把「消息不丢」改写为「成功入队不丢」。

成本：约 2-3 个独立提交；`EventQueue` 的 `submit` 签名变化会影响所有适配器调用点
（process / onebot11 / dynamic），但都是机械改动。

---

## 7. 需要拍板的点

1. 入站默认值选 **4096** 还是 **2048**（内存上界保守 vs 突发吸收）。
2. 入站超限默认 `reject`（推荐）还是 `block`（更接近端到端不丢，但会卡平台读循环）。
3. 是否接受**不在本阶段**实现 `drop_oldest`（推荐暂不做）。
4. `/healthz` 的 degraded 阈值是否接受「1 分钟 1000 条拒绝」这一实现默认。
