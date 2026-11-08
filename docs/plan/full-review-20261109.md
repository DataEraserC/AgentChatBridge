# 全面审查（2026-11-09）：代码质量、决策复核与协作流程

> 审查基线：`2c372f3`（dev_20261025），工作区干净。
> 配套文件：`docs/plan/code-review-prompt.md`（交给外部审查 agent 的提示词）、
> `docs/plan/ai-collab-playbook.md`（由本报告第 6 节展开的人机协作工作法）。
> 审查方式：四组只读审计（架构与规模、正确性与失败面、测试与验证、安全与信任边界）
> 并行读码，再由本人对**最关键的断言逐条回读源码复核**。凡未亲自读到的一律标注。
> 参考对象：https://github.com/mzdluo123/webmux（Go + Svelte 的单二进制终端，521 提交，
> 有完整的 agent 协作规范与机器看守的架构约束）。

---

## 0. 一句话结论

代码的**可读性**并不差（没有 `unwrap` 遍地、没有 `todo!`、注释写得比多数商业项目认真），
真正的问题不在「代码丑」，而在三件结构性的事：

1. **验证链路是断的**：`nix build` 已经不再跑测试（`doCheck = false`），CI 只有文档站，
   1093 个测试靠人自觉——而交接文档还写着「nix build 会自动跑 cargo test」。
   实测（`cargo test --workspace --no-fail-fast`）：**1080 通过 / 8 失败**，红的
   **全部在「安装 / 启动 / 监管」这层新代码**，核心 crate（kernel、adapters、proto、
   bridge）全绿；8 条里 2 条是沙箱限制、1 条并发偶发、5 条是真问题（测试不 hermetic
   + 配置 fixture 过期）。也就是说：**能绿的都绿了，但没人知道它是绿的**。
2. **决策与实现之间没有强制关系**：`docs/design` 写了 2062 行的 policy-store，代码里
   `PolicyStore` 零命中；`ACB_KEY` 零命中；库表里没有 token 列。ADR 是叙事，不是断言。
3. **少数几条路径会丢消息或卡死全桥**：审批执行在持有引擎锁时同步发消息（最长 45 秒
   上传超时）；ack 循环用 `?` 中途返回；route worker 的插件 handler 没有 `catch_unwind`。

流程上是 vibe coding，这个判断成立，但原因不是「AI 写代码」，而是：**变更没有可审的粒度、
验证靠自述、规则没有执行器**。下面第 5 节用 webmux 的做法逐条对照。

---

## 1. 代码质量：按后果排序的问题清单

### P0 — 会丢消息 / 卡死全桥（建议本批修）

| # | 位置 | 事实 | 后果 |
|---|---|---|---|
| P0-1 | `crates/kernel/src/engine.rs:3120`（`execute_approved_tickets`）→ `:3130`（`send_admin_text`） | 审批通过后的执行与确认文本**在 `event_queue.rs:235 guard.ingest()` 的同一次持锁调用栈里同步发送**，底层 `outbound.send()` 文本 10s / 上传 45s / 异步 300s 超时 | 管理员一次 `/approve` 或收工时 flush 票据，可把整座桥（入站、时钟、控制台、所有会话）按住数十秒到数分钟 |
| P0-2 | `crates/kernel/src/engine.rs:8598-8616`（`record_and_ack`） | 平台已发送成功后才写记录；`store.ack` 在 for 循环里用 `?`，第 k 个订阅者失败 → 第 k+1..n 永久不 ack，且整个调用返回 `Err` 给模型 | 模型以为没发出去而重发（重复消息）；各会话水位不同步，洞后面反复重投 |
| P0-3 | `crates/kernel/src/route_worker.rs:143-151` | `plugin_handler.handle(...)` 裸调用；`plugin.rs:1698` 的 `catch_unwind` 是给 provider 调用用的，**没有覆盖 task handler** | 任一 Lua/动态插件 handler panic → 唯一的 route worker 线程死掉，之后所有路由发送与后台任务静默停止（只有一行日志） |
| P0-4 | `crates/bridge/src/main.rs:549-553` | 排空判据只看 `active_turns` / `running_tasks` / `worker.busy()`，**不看 `queued_turns` 与入站队列深度** | SIGTERM 时队列里未 ingest 的事件直接随进程消失 |
| P0-5 | `crates/kernel/src/engine.rs:2730` 注释 vs 全仓代码 | 注释写「记录留在流水里，下一进程凭未读接着处理」，但 `main.rs`/`endpoint.rs` 无任何启动补唤醒 | 崩溃/重启后未读永远不投，直到下一条平台消息；配合 `turn_timeout_secs = 0`（我们主动关的）就是静默停摆 |
| P0-6 | `crates/kernel/src/engine.rs:7965` | 桥只把 `turn_timeout_ms` 下发给 harness，**自己不看门**；`active` 只能被 `TurnEnd`/断连清掉 | harness 进程活着但一轮卡死，该会话永久被占，pending 只更新不投递。这是「去掉自动超时」这个决策的直接代价（见 D7） |

### P1 — 高峰期会咬人

- `route_worker.rs:57` + `engine.rs:3253`：路由队列满时动作为拒（仅日志），但任务已写成
  `Pending` → `tasks op=wait` 永远 `still_running`，消息消失。修：拒绝路径必须
  `finish_task(failed, "route queue full")`。
- `crates/kernel/src/task.rs:117`：子进程已退出但孙进程继承 stdout 时，`read_to_string`
  无超时 → worker 线程永久卡死（`exec` 里跑 `nohup … &`、ffmpeg 残留即触发）。修：独立
  reader + deadline + kill 进程组。
- `engine.rs:1430`/`:3510`：尝试记录以 `Running` 落库，启动无对账 → 崩溃后 `task_get`
  永远 running，`drain_snapshot` 还把它算作在跑。修：启动把孤儿 `pending/running` 置
  `unknown`。
- `engine.rs:4144`/`:3814`：`notify_turn_error` 在「组装入队」时就 `sent += 1`，平台发送
  失败不回滚、指纹已记 → 同类失败被当 duplicate 抑制，用户永远收不到。
- `endpoint.rs:931`：`write_frame` 用阻塞 `write_all` 且无写超时；harness 不读时该连接线程
  永久卡住。
- `endpoint.rs:373`/`:278`/`:284`/`:336`：`abandon_turn`、`delete_session_map`、
  `upsert_session_map` 的错误被 `let _ =` 吞掉 → 永久 active 会话，或 reset 后仍复用旧
  harness session。
- `engine.rs:96`/`:11345`：`PROMPT_TIMEOUT = 50ms`，慢 provider 的上下文被静默丢弃，且每次
  `thread::spawn` 不 join → 长期运行泄漏线程。

### P2 — 记在账上，不急

`storage/sqlite.rs:606` 的 `limit as i64`（`usize::MAX` → SQLite 的 `-1` 无上限）、
`engine.rs:5487`/`:8145` 全房间扫描、`engine.rs:5255` 的
`expect("a ticket always has an active turn")`、`console.rs:225` 每连接一线程无并发上限、
`engine.rs:5505` `located(...).ok()` 吞错。

**注**：子审计最初报「控制台监听 0.0.0.0 是配置文件写错」，我复核后**推翻**了这个说法——
仓库 `config.toml:58` 确实是 `127.0.0.1:3100`，但**运行中进程用的是另一份配置**
（`/home/nixos/.local/share/acb/config/config.active.toml`，其 `:81` 是 `0.0.0.0:3100`）。
真正的问题是「两份配置漂移 301 行」，不是笔误。见 §4。

---

## 2. 架构与体量

| 指标 | 数值 |
|---|---|
| `crates/*` Rust 总行数 | 123,766（src 94,148 + tests 29,336） |
| 最大三文件 | `engine.rs` 12,129 / `config.rs` 6,200 / `plugin.rs` 5,210（合计占 src 25%） |
| `#[test]` 总数 | 1,093（kernel 578、adapters 147、acb 125、supervisor 75、plugins 45…） |
| JS harness | 12,031 行，13 个 `*.test.mjs` 约 142 例 |
| docs / 代码 | 约 94 篇、29,770 行 ≈ src 的 32%；其中 `docs/plan` 45 篇 20,767 行（70%） |
| ADR | 30 篇 2,420 行 |
| 版本号 | 7 套（PROTOCOL=7、ADAPTER_API=1、PLUGIN_API=1、WIRE=1、ABI=1/2、SCHEMA=12、CONTRACT=1） |

三条判断：

1. **适配器边界实际是破的**。`adapters/onebot-snowluma/src/{event,link,catalog,factory,outbound}.rs`
   直接 `use acb_kernel::{Element, RecordDraft, Connection, EventQueue, Outbound}`，
   绕过了本该是唯一契约的 `adapter-api`。反向也有一处：`crates/launcher/src/main.rs:19`
   让生产入口依赖 CLI 库 `acb`。kernel 反向依赖 bridge/adapters 未发现（这点是干净的）。
2. **同一种语义有三套类型**：`proto::Record` / `kernel::RecordDraft` /
   `adapter-api::WireRecord`，靠 `kernel/src/wire.rs:127` 转。另外进程插件有两套载体、
   两套协议（`plugins/process-tools` 的 JSON-RPC 2.0 vs `plugins/process-provider` 的
   manifest/reply 行协议），`harnesses/shared/bridge-tools.mjs:3-6` 注释自己承认「dsh 里
   已经有一份同样的逻辑，不改」。
3. **工具面在三条路径上各写一遍**：`send_message` 的解析 + `ensure_can_send` 出现在
   `engine.rs:4194`（当轮路径）、`:4699`（prepared/异步路径）、`:5301`（轮后宽限窗口）。
   这不是「4 份拷贝」，但新增一个发送类工具确实要同步改三处，且三处的鉴权顺序不完全一致。

体量不是原罪，**12k 行的单文件 + 单 `impl` 才是**：改一行要在 12k 行里判断影响面，
这是「半个屎山」的物理来源。

---

## 3. 验证链路（本次最重要的发现）

| 环节 | 声称 | 实际 |
|---|---|---|
| `nix build .#acb-bridge` | `HANDOFF.md:30`「会自动跑 cargo test」 | `flake.nix:44` `doCheck = false`，注释明写「测试由 `cargo test` 单独执行」。**HANDOFF 与 flake 直接矛盾** |
| CI | — | 只有 `.github/workflows/docs.yml`（`docs/**` 触发，发 Pages）。**没有 cargo test / node --test / nix build** |
| 本地 `cargo test` | 「新依赖会撞 mtime，直接 nix build」 | 本次实测可用：`cargo test --workspace --no-fail-fast` 跑完，1080 通过 / 8 失败（见 §3.1） |
| harness JS 测试 | — | 13 个文件，但 `harnesses/shared` 没有 `package.json`，其中 2 个测试不在任何入口里 |
| 真机脚本 | — | `deploy/verify-shutdown.sh` 有真断言（PASS/FAIL + 退出码），`verify-launcher.sh` 要 user systemd 手工跑，其余无断言 |

也就是说：**当前唯一的质量门禁是「人记得跑」**。这解释了为什么 314 个提交能在 8 天内
全量落地而无一次红灯——因为没有红灯。

### 3.1 本次实测：1080 / 8

命令：`cargo test --workspace --offline --no-fail-fast`（2026-11-09 01:25，退出码 101）。

| 失败用例 | 所在 | 性质 | 根因 |
|---|---|---|---|
| `changed_value_updates_only_affected_file` | `acb/tests/unit_poc.rs:178` | **沙箱限制，非产品问题** | `set mtime` 返回 `EPERM`（第二层沙箱禁 `utimensat`），真机应过 |
| `second_install_is_unchanged_and_preserves_mtime` | 同上 | 同上 | 同上 |
| `direct_mode_spawns_bridge` | `acb/tests/run_backend.rs:476` | **真问题：fixture 过期** | 测试写的配置缺 `[endpoint].listen`，而 `EndpointConfig.listen` 是必填 `String`（`kernel/config.rs:658`），解析即失败 |
| `launcher_75_reloads_pointer_then_78_exits_with_reason` | `run_backend.rs:148` | **真问题：测试不 hermetic** | 测试 `env_remove("ACB_PROFILE_DIR")` 后退回**本机真实安装** `~/.local/share/acb/profile`，该布局下没有 `current/bin/acb-launcher`（我们的 launcher 来自 nix 系统路径） |
| `launcher_mode_renders_supervisor_toml_from_acb_unit_values` | `run_backend.rs:437` | 同上 | 同上 |
| `rate_limit_stops_after_three_failures` | `run_backend.rs:205` | 同上 | 同上 |
| `sigterm_forwards_to_child_and_reaps` | `run_backend.rs:261` | 同上 | 同上 |
| `stop_order_is_reverse_dependency_order` | `supervisor/tests/component_runtime.rs:779` | **并发偶发** | 单跑通过（`running 1 test ... ok`）；并行时「order file」未就绪 |

结论三条：

1. **核心逻辑是绿的**：kernel 578、adapters 147、proto 33、bridge 24 等全过。
2. **红全在 Trae 新加的部署层**，且其中 4 条暴露同一个设计问题：这些集成测试假设存在
   一种「自包含安装布局」（profile 里有 launcher），而我们的实际部署是 nix 提供
   launcher → 测试与部署模型不一致（正好是 D6 那条「nix 可选」的裂缝）。
3. **`nix build` 不跑测试这件事必须马上补**：否则下一次改 `engine.rs` 的人和今天一样，
   不会知道这 1080 条是绿的还是红的。

### 3.2 测试本身的质量

抽样 8 个：多数是真断言（如 `turn_flow.rs:3852` 断言两个订阅者水位都推进、
`direct_wake.rs:176` 覆盖信任/未信任/限流/审计、`supervisor/tests/component_runtime.rs:131`
起真实子进程）。但有若干「看起来测了、其实没验证」：

- `tests/drain_flow.rs:232` 只断言 `result`/`error` 字段存在，返回 `null` 也能过；
- `adapters/onebot-snowluma/src/outbound.rs:1987` 是拿文档表格对常量做自洽检查，不验证真实调用；
- `crates/bridge/src/console.rs:1667` 的前端测试只是 grep HTML 常量。

零测试的高风险模块：`engine.rs` 12k 行只有 5 个内联测试；`supervisor/adopted.rs`（接管已
运行进程）整文件 0 测试；`launcher/main.rs` 主循环/信号/watchdog 0 测试；
`plugins/process-provider` 整个 crate 0 测试；`policy_decision` 的 fail-closed 分支 0 测试。

---

## 4. 安全面（按可利用性）

1. **群消息提示注入（最高，且是产品本质）**：投递模板只在行首加 `#id 时间 昵称(id)`，
   正文、群名片、合并转发、引用摘要原样拼进同一段文本（`harnesses/opencode/plugin/acb-wire.mjs:499-532`、
   `:663-708`；昵称取自 `adapters/onebot-snowluma/src/event.rs:389-400` 未清洗）。
   agent 有 shell，任一已订阅房间的成员都能尝试说服它执行命令。只有**插件片段**被标了
   `[插件上下文]`，聊天记录没有同等边界。`docs/plan/self-iteration-adversarial-review.md:50`
   已经承认这一面未解。
2. **控制台 0.0.0.0 + 明文 HTTP + token 当 cookie**：活配置 `config.active.toml:81`
   是 `0.0.0.0:3100`；cookie 值就是 token，无 `Secure`、无 TLS、无 CSRF/Origin 校验
   （`crates/bridge/src/console.rs:296-323`、`:399-423`）。拿到 token 即可改配置、
   触发插件 reload（等于执行代码）。
3. **文件中继 3131 无鉴权且绑 0.0.0.0**（活配置 `:563`，`public_base` 走
   `host.containers.internal`）：凭据就是 32 位 hex 能力 URL，24 小时有效；路径穿越已防住
   （`relay.rs:151-163`），风险是 URL 泄露（发到群里就永久有效）。落盘用 `fs::copy`
   保留源权限、目录 755，同机其他用户可绕过 HTTP 直接读。
4. **Lua 插件 = 进程内完全信任**：`plugins/lua/src/lib.rs:734` 用 `StdLib::ALL_SAFE`
   （含 `io`/`os`/`package`），可读库文件与密钥文件、`os.execute`。`capabilities` 只约束
   扩展点 API，不约束文件/网络/进程。WASM 反倒安全（无 import + fuel + 内存上限）。
5. **token 明文面**：`config.toml.bak*` 三份 0644 含 6 个在用 token；连接 URL 连
   `access_token` 一起进日志（`onebot-snowluma/src/link.rs:684`）；`acbctl secrets resolve`
   把明文打到 stdout。**`ACB_KEY`/`ACB_KEY_FILE`/`v1:<kid>` 全仓零命中**，加密未实现。
6. **MCP 3101/3102 无 HTTP 鉴权**（仅回环），任意本地进程可 `tools/call`；debug 日志会打印 ticket。
7. 依赖面：162 个依赖全来自 crates.io，无 git 依赖；`git2 0.19` 偏旧（未跑 `cargo audit`，待验证）。

---

## 5. 决策复核：哪些是真的成立，哪些只是写得好

> 用户要求「不保证目前所有决策均正确」，所以这一节按「结论 → 我的判断 → 依据 → 若改的代价」写。
> 判断标准：**能与代码/约束对照的算证据，只有文档表述的算叙事**。

| 编号 | 决策 | 判断 | 依据 |
|---|---|---|---|
| D1 | 清空重写 + 文档先行 | **有保留** | 方向对，但文档只增不减：`docs/plan` 45 篇 20,767 行，无「已实现/已废弃/被取代」的自动收敛，`config-storage-audit.md` 这类被取代的文档还要靠首行手写「不作实现依据」 |
| D2 | 五等对象（连接/人格/记忆/harness/harness 会话） | 成立 | 代码结构与之对应，术语在 30 篇 ADR 里稳定 |
| D3 | 跨 harness 发送砍掉，`send_message_to` 边界=本会话订阅房间 | 成立 | `engine.rs:4211` 起的分支与测试一致；简化正确 |
| D4 | 配置分层：`config.toml` 只做引导，业务对象只认库 | **方向成立，实现 ~0%** | 活配置仍是单文件 15KB；库表无 token 列；`policy.rs` 仅 168 行 |
| D5 | token 入库 + 可选加密，密钥只从 `ACB_KEY`/`ACB_KEY_FILE` 读 | **未兑现的承诺** | `ACB_KEY` 全仓 0 命中；现状是明文 toml + 644 备份 + `secrets resolve` 打 stdout |
| D6 | nix 与 systemd 均为可选 | **与实现矛盾** | profile 走 store 软链、unit 模板随包分发、`acbctl` 按 nix 安装布局推导路径。要么改实现，要么把决策降级为「可选但有默认」 |
| D7 | 去掉单轮自动超时（2026-10-31 用户裁定） | **有保留，需补一半** | 关掉 harness 侧超时是对的（避免误杀长任务）；但桥侧必须自己的看门狗，否则 P0-6：harness 卡死 = 会话永久占死。建议改成「harness 侧可关，桥侧必开」 |
| D8 | 控制台热重载只在轮次边界生效 | 成立 | `engine.rs` stage/apply 两处应用，测试看守 |
| D9 | 工具可见域按会话圈，不按连接 | 成立 | `tool_scope_is_the_turn_session_not_the_connection` 看守 |
| D10 | 工具面 25 个 + `render::fit` ≤24KiB | 成立 | 但发送类工具在三条路径各写一遍鉴权（§2.3） |
| D11 | 插件 Lua 走进程内 | **建议改写表述** | 现在的事实是「插件 = 桥进程内任意代码」。若这是有意的，就在文档里写死「插件完全信任，只装自己写的」；若不是，按 ADR-0029 的进程隔离推进 |
| D12 | launcher / supervisor / payload 三层 + `acbctl upgrade` | **有保留** | 设计是合理的（升级链与业务版本解耦），但现场 payload 落后 HEAD，说明这条链还没被真正走通一次；复杂度先于收益落地 |
| D13 | issue 做成平台适配器（未实现） | 成立 | `docs/plan/issue-platform-adapter.md` 的「追加式修订」选择正确；注意 issue 正文同样是注入面 |

**最该被推翻的一条是 D6 的措辞**（「nix 可选」在代码里不成立），其次是 D1 的「只增不减」。
D7 不是推翻，是补全。

---

## 6. 流程：为什么现在算 vibe coding，webmux 怎么做的

### 6.1 现状的事实

- 8 天 314 提交 / 415 文件 / +153k−7.3k，**全部直接进 `dev_20261025`**，无分支、无 PR、无 review。
- 有 30 篇 ADR、45 篇 plan，但**没有一条被机器执行**：没有 archtest、没有文档预算、
  没有 wire 快照、没有覆盖门槛。
- 验证靠自述：`HANDOFF.md:30` 那句「nix build 会自动跑 cargo test」就是这种模式的自然后果。
- 历史可改写：尾注补写用了 `filter-branch` + 强推（正确做法，但也说明提交历史不承担契约作用）。
- 决策靠对话裁定，落地靠记忆：`PolicyStore` 2062 行文档 vs 代码 0 命中，是最好的证据。

### 6.2 webmux 的机制（都能在仓库里指到具体文件）

| 机制 | 具体做法 |
|---|---|
| **规则预算** | `AGENTS.md` 有 **10 KiB 硬上限，由 `internal/archtest` 的 `TestAgentsGuideSize` 看守**；超了必须把规则搬到 `docs/agent-rules/` 并留一行指针，**不许删规则也不许抬上限** |
| **架构可执行** | `internal/archtest` 定义分层规则 R0–R11 + `layers_test.go` 清单，**每个新增 `internal/` 包必须被分类**；前端 `check:arch` 管设计系统/分层/重复/死代码，基线清空、新增违规即失败 |
| **契约快照** | 后端 JSON 字段 = `internal/contract` 类型 + `web/src/schemas.ts`（Zod）双端；改字段必须同步并 `TestWireSnapshots -update` 重录快照 |
| **文档治理** | 单文件 20–400 行、单行 ≤300 列、≤48 KiB、每级目录 ≤20 个；`make docs-check` 校验**每个文档都能从 README/AGENTS 相对链接到达且锚点可解析** |
| **提案/实现分离** | OpenSpec：`propose` 只产 `proposal.md` + spec delta + `design.md` + `tasks.md`，**写完就停，等新指令才 apply**；`apply` 按 tasks 逐条打勾，遇到范围外必须停下问，不许悄悄缩范围 |
| **能力台账** | 每个已实现能力必须有 user story（角色/动作/目的/可观测验收），ID 永不复用；交付说明里必须点名受影响的 story ID 或说明无影响；**不许声称没跑的验证** |
| **PR 门禁** | 独立分支 + `git worktree`，不直推 main；Conventional Commits；跟随 CI 与 `pullfrog[bot]` 直到全绿；**UI 变更必须贴 before/after、明暗、桌面/手机截图证据**；PR 留着等人 review 再合 |
| **CI 不自欺** | 按变更路径裁剪测试矩阵，但未知路径/`.github`/`Makefile`/锁文件一律全量；**分类脚本用 base 版本执行**，防止 PR 改分类器跳过自己；`Test result` 交叉校验应有 job 全 success |
| **技能单一源** | `.agents/skills/*` 是唯一源，`.claude/skills/*` 是符号链接 → 同一份规则跨 harness 复用 |

一条关键设计原则值得单独说：**pullfrog 那个 AI review 只做「建议」，通过与否由 CI 和维护者定**。
他们不把模型意见当门禁，只把机器可验证的东西当门禁。这正好是我们最缺的分界线。

### 6.2.1 人与 AI 各自负责什么（webmux 的分工）

| 角色 | 做什么 | 靠什么保证 |
|---|---|---|
| 人（维护者） | 提需求、**授权**「现在可以开始实现了」、看证据、合 PR、发版 | 不需要逐行读 AI 的代码，只需要看：CI 是否绿、bot 意见是否已解决、UI 变更的截图、受影响的 story ID |
| agent | 读 `AGENTS.md` + 对应 skill（`webmux-ui`/`webmux-docs`/`openspec-*`）→ 开分支（必要时 `git worktree`）→ 写码 + 写测试 → 提 PR → 跟随 CI/bot 修到绿 → 留着等 review | 规则是「可加载的合同」，不是散落在聊天里的约定 |
| bot（pullfrog） | 在 PR 上提意见 | 只有建议权，没有通过权 |
| 机器（CI / archtest / docs-check） | 判定 | 唯一门禁 |
| 追溯 | 提交尾注带 `Co-Authored-By` 与 `Claude-Session:` 链接 | 事后能查「这次改动是哪次会话产出的」 |

对我们最有借鉴价值的是第一行：**人可以不读 diff，但不能没有证据**。所以证据必须由机器产生
（测试退出码、端口监听、快照 diff），而不是由 AI 自述。你现在不看代码细节也能验收，
前提就是这条。

### 6.3 差距

| 维度 | 我们 | webmux |
|---|---|---|
| 变更粒度 | 一次提交可跨 5 个 crate | PR 单主题，附动机/改动/验证 |
| 验证 | 靠人记得跑；`nix build` 不跑测试 | `make test` 一条命令、CI 四片并行、覆盖报告 |
| 架构约束 | 无 | archtest R0–R11，新包必须分类 |
| 契约 | 7 套版本号，无快照 | contract 类型 + Zod + wire 快照 |
| 文档 | 45 篇 plan 只增不减，无链接校验 | 体积/行宽/目录扇出/链接全有守卫 |
| 决策落地 | ADR 是叙事 | OpenSpec 提案 → tasks → 归档，story 台账对账 |
| AI 的角色 | 写代码 + 自己宣布验证过了 | 写代码 + 提 PR，**证据由 CI 与截图提供，不是自述** |

### 6.4 该搬什么（按我们的条件改写）

我们是一个人 + nix + 没有 CI 预算，不能照抄。可搬的五条：

1. **一条命令门禁**：`deploy/verify.sh` = `cargo test --workspace` + 各 harness
   `node --test` + docs 链接检查，退出码即结论。替代 webmux 的 `make test`。
   CI 可以先不建，但**门禁必须先存在**。
2. **可执行的 ADR 断言**：不搬整套 archtest，先做 3 条最便宜的：
   ① `adapters/*` 不得 `use acb_kernel::`（或显式白名单）；
   ② 文档里出现「验证/测试已通过」必须能在 `verify.sh` 的日志里找到对应项；
   ③ 每个 `config.toml` 键必须在 `docs/guide/config.md` 出现（现在配置与文档已经漂移）。
3. **提案/实现分离的最小版**：任何跨一次会话的改动，先写一页「变更单」
   （要改什么 / 验收断言 / 回滚方式），写完停下等确认，再实施。这正是 OpenSpec 的
   planning boundary，去掉 CLI 也能做。
4. **plan 三态**：`draft` / `accepted` / `archived`。`accepted` 必须对应代码或一条断言；
   每月把「accepted 但无对应实现」的挑出来，要么实现要么降回 draft。这一步直接治
   PolicyStore 那种 2062 行悬空文档。
5. **规则文件有预算**：`AGENTS.md`（我们目前没有）限 150–200 行，只放「每次改动都要遵守」
   的规则，其余外置。机制不是预算数字，而是**超了必须搬，不许删也不许抬**。

### 6.5 不该搬什么

- **PR + bot review 全家桶**：一个人开发时 PR 只是仪式，真正起作用的是门禁与证据。
- **OpenSpec CLI**：它的价值在「提案只产文档、实现另起指令」这个约束，不在工具。
- **UI 截图证据**：我们的控制台是单页 HTML，收益远小于成本。
- **覆盖率门槛**：1093 个测试里先挑 P0 路径补测试，比追百分比有用。

---

## 7. 建议的落地顺序

**批次 A（止血，半天内）**
1. `deploy/verify.sh` 一条命令跑全部测试（`--no-fail-fast`，别让第一个红挡住后面全部），
   跑通并记录基线；顺带把 §3.1 的 5 条真问题处理掉（4 条改成真隔离的临时 profile，
   `direct_mode_spawns_bridge` 补 `listen`），沙箱那 2 条用 `#[ignore = "需要真实 utimensat"]`
   显式标注；
2. 修 `HANDOFF.md:30` 与 `flake.nix` 注释的矛盾，明确「nix build 不跑测试」；
3. `config.toml.bak*` 改 0600 或删除（含 6 个在用 token）；
4. 控制台从 `0.0.0.0:3100` 收回回环（或加 Origin 白名单 + CSRF），并明确「仓库 config.toml
   不是活配置」这件事；
5. 连接 URL 日志脱敏、`secrets resolve` 改显式开关。

**批次 B（结构性，3–5 天）**
1. P0-2（ack 循环 best-effort + 发送成功后不因记录失败返错）；
2. P0-1（审批执行与确认文本移出持锁路径，改走 route worker）；
3. P0-3（route worker `catch_unwind` + 忙标志用 guard 递减）；
4. P0-4/P0-5（排空纳入队列深度；启动按 (session, room) 查未读并补唤醒）；
5. P0-6（桥侧看门狗，`turn_timeout_secs = 0` 保持但只关 harness 侧）；
6. 起步拆 `engine.rs`：先抽「发送类工具」的三处重复与 `tool_call` 的 515 行函数。

**批次 C（流程，与 B 并行）**
1. 写 `AGENTS.md`（≤200 行）+ `deploy/verify.sh` 引用；
2. plan 三态化，先把 45 篇 plan 打一遍标签；
3. 变更单模板（一页）；
4. 三个可执行断言（6.4-2）。

每批的验收方式：A 批看 `verify.sh` 退出码 + 端口不再是 0.0.0.0；B 批每条 P0 配一个
能复现失败场景的测试；C 批看新文档能不能被 `verify.sh` 的检查认出来。

---

## 8. 未做与待验证

- 未跑 `cargo audit`、未做真实平台压测、未在真机复现 P0-1/P0-4（只读代码路径推导）。
- 子审计报「控制台绑 0.0.0.0 是配置笔误」，经复核**推翻**：是活配置与仓库配置漂移。
- 子审计报「记录类型三份/版本号七套」已核实；「旧 driver 体系 vs 新 supervisor 并存」未核实。
- `git2 0.19` 是否有已知公告未查。
- 提示注入的实际可利用性取决于 QQ 是否放行昵称换行，未验证。

## 附：执行记录

- `cargo test -p acb-proto --offline` → 33 passed（确认本地 cargo 可用）。
- `cargo test --workspace --offline --no-fail-fast` → **1080 passed / 8 failed**，退出码 101，
  完整日志 `/tmp/acb-test-full.log`。失败明细见 §3.1。
- 单跑 `cargo test -p acb-supervisor --test component_runtime stop_order_is_reverse_dependency_order`
  → ok（证明那条是并发偶发，不是稳定失败）。
