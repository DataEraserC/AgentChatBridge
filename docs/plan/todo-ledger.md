# 待办总账（2026-11-09 起）

这份是**唯一台账**。散在 `full-review-20261109.md`、`handoff-*`、各 plan 里的待办，
以这里的状态为准；其他地方只保留论证，不再重复待办列表。

用法：每条有 id。做完改状态并在「验收」列写实际命令与输出要点；判定变了就在
「判定」列写新结论和理由，不删旧行——删行等于丢失「为什么当初想做」。

状态：`done` / `doing` / `next` / `hold`（等条件）/ `drop`（判定不做）/ `blocked`。

---

## 0. 判定原则（这轮新增）

三条会反复用到，先写死：

1. **XY 检查**：用户描述的解法 ≠ 用户的问题。每条待办先问「不按这个做，用户的痛还在不在」。
   在 → 说明这是解法不是问题，要回到问题重找方案；不在 → 可以照做。
2. **有消费者的才做**：没有实际使用方的能力（插件存储、策略插件）不排期，只留设计。
   真机当前只有 1 个外置插件、6 个会话、1 条权限规则——规模不支持大迁移先行。
3. **改判要留痕**：判定为 `drop` 或改方向的，必须写「原方案 → 新方案 → 理由」，
   并且给一条可反驳的条件（什么情况下该翻回来）。

---

## 1. 批次 A：止血（原本 5 条）

| id | 原待办 | 判定 | 说明 | 验收 | 状态 |
|---|---|---|---|---|---|
| A1 | `deploy/verify.sh` 一条命令跑全部测试 + 修 8 条红 | 照做 | 已落地：fmt/测试/harness/guards 四段，日志落 `target/verify/` | `./deploy/verify.sh` 退出码 0；1089 + 144 + 3 guards | done |
| A2 | 修 `HANDOFF.md:30`「nix build 会自动跑 cargo test」 | 照做 | `flake.nix:44` 是 `doCheck=false`，该句已证伪 | `grep -n "自动跑" HANDOFF.md` 无命中 | done |
| A3 | `config.toml.bak*` 改 0600 或删 | 照做（改权限不删） | 三份 0644、共含 15 处明文 token；已 gitignore 未入库，删了就丢回滚面 | `ls -l config.toml.bak*` 全 `-rw-------` | done |
| A4 | 控制台从 `0.0.0.0:3100` 收回回环 | **改判：hold** | 见 §5 改判记录 | — | hold |
| A5 | 日志脱敏 + `secrets resolve` 加显式开关 | **改判：只做日志** | `secrets resolve` 打印明文是部署脚本的**唯一用途**（`start-drivers.sh:69` 靠它取 token）；加开关要改 6 个脚本，收益为负。真正的泄露点是连接 URL 连 `access_token` 进日志 | `grep -n "access_token" ` 日志输出应为 `***` | next |

## 2. 批次 B：P0（会丢消息 / 卡死全桥）

| id | 位置 | 问题 | 判定 | 状态 |
|---|---|---|---|---|
| B1 | `engine.rs:3120` | 审批执行 + 确认文本在 `ingest` 持锁栈内同步发送，最长按住全桥 45s | 照做：改投 `route_action`，锁外执行；回执延迟几十毫秒可接受 | next |
| B2 | `engine.rs:8612` | `record_and_ack` 的 ack 循环用 `?`，第 k 个订阅者失败 → 后续永不 ack + 把 Err 返给模型（模型重发） | 照做：逐个 ack、失败记日志继续 | done |
| B3 | `route_worker.rs:143` | 插件 handler panic 杀死唯一 route worker，之后所有路由静默停 | 照做：`catch_unwind` + `finish_task(failed)` | done |
| B4 | `main.rs:549` | 排空只看 `active_turns`/`running_tasks`/`busy`，不看 `queued_turns`/`queued_tasks`/`pending_route_actions` | 照做：字段 `drain_snapshot` 里已有，判据补上即可 | done |
| B5 | `main.rs`/`endpoint.rs` | 启动不补未读，崩溃后未读永远不投 | 照做，但要先确认「补投不会重复」——水位已 ack 的不重投 | next |
| B6 | `engine.rs:7965` | 桥侧无轮次看门狗，harness 卡死则该会话永久被占 | **改判**：见 §5 | hold |

## 3. 批次 B'：P1（高峰期咬人，7 条）

| id | 位置 | 问题 | 判定 | 状态 |
|---|---|---|---|---|
| C1 | `route_worker.rs:57` | 路由队列满 → 动作被拒但任务留 `Pending`，`tasks op=wait` 永远等 | 照做（一行：拒绝路径 `finish_task(failed)`） | next |
| C2 | `task.rs:117` | 孙进程继承 stdout → `read_to_string` 永久卡死 | 照做：独立 reader + deadline + kill 进程组 | next |
| C3 | `engine.rs:1430/3510` | 尝试记录崩溃后永远 `running`，`drain_snapshot` 还把它算作在跑 | 照做：启动把孤儿 `pending/running` 置 `unknown` | next |
| C4 | `engine.rs:4144/3814` | `notify_turn_error` 入队即 `sent += 1`，失败不回滚 + 指纹已记 → 同类失败被抑制 | 照做：发送成功才记指纹 | next |
| C5 | `endpoint.rs:931` | `write_frame` 阻塞写无超时 | 照做：设写超时 | next |
| C6 | `endpoint.rs:373/278/284/336` | `abandon_turn`/`delete_session_map`/`upsert_session_map` 用 `let _ =` 吞错 | 照做：至少记日志，关键路径返错 | next |
| C7 | `engine.rs:96/11345` | `PROMPT_TIMEOUT=50ms` 静默丢上下文；每次 `thread::spawn` 不 join | 拆两条：超时改可配置+记日志（先做）；线程池化（后做） | next |

## 4. 批次 C：流程（原本 4 条）

| id | 原待办 | 判定 | 说明 | 状态 |
|---|---|---|---|---|
| E1 | 写 `AGENTS.md`（≤200 行） | 照做 | 已落地 114 行，guards 看守预算 | done |
| E2 | plan 三态化，45 篇打标签 | **drop** | 见 §5 | drop |
| E3 | 变更单模板（一页） | **改判：并入 AGENTS.md** | 单独一页模板没人填；把「改了什么/怎么验/剩什么」写进提交尾注足够 | drop |
| E4 | 三个可执行断言 | 照做（缩到 2 条） | 已做：AGENTS.md 行数预算、禁止本机配置状态入库。第三条「文档承诺必须有实现」无法自动判定，人工 review | done |

## 5. 改判记录（原方案 → 新方案 → 理由 → 何时翻回）

### A4 控制台绑定地址
- 原：从 `0.0.0.0:3100` 收回 `127.0.0.1`。
- 新：**暂不动**，先做「拿不到 token 就进不去」之外的两件事——① 改配置的接口加 Origin 校验；
  ② 登录 cookie 加 `HttpOnly`+`SameSite=Strict`。之后再按用户实际访问方式决定绑哪。
- 理由：这是典型 XY。真实风险是「明文 HTTP + token 当 cookie + 无 CSRF」，不是绑定地址；
  直接改回环会砍掉远程访问能力，而我没证据用户是否依赖它（`ss` 显示当前无连接，但只说明此刻没连）。
- 翻回条件：用户确认「从不远程开控制台」→ 立即改回环，成本 1 行。
- **需要用户回答**：你平时是在本机浏览器开 3100，还是从别的机器/手机开？

### B6 轮次看门狗
- 原：桥侧加 `turn_timeout`，超时强制结束轮次。
- 新：**只做可观测，不做自动 kill**。控制台/`capabilities` 显示「该会话已 N 分钟无进展」，
  打断仍走手动（`control interrupt` + 控制台按钮）。
- 理由：2026-10-31 用户明确裁定「去掉自动超时」，因为长任务被误杀。原方案直接违背这条。
  真正的病是「卡死不可见」，不是「没超时」。
- 翻回条件：若出现「无进展但 harness 心跳正常」的持续占用，再考虑按**进度心跳**（而非墙上时间）判死。

### E2 plan 三态化
- 原：给 45 篇 plan 打 `proposal/accepted/implemented` 标签。
- 新：**不做**。真问题是「文档写得像已实现」（policy store 2062 行、`PolicyStore` 全仓 0 命中）。
  打标签不改变这一点。
- 替代：文档首行必须写「状态：已实现 / 部分实现（哪些） / 仅设计」，并且 `verify.sh` 检查
  「标了已实现的条目必须在代码里能 grep 到」。这条作为 E4 的延伸，下次做。
- 翻回条件：真的有人按 plan 标签做决策时。

### D3/D5 插件存储与配置
- 原：按 `acb-policy-store.md` §3.12/§6.5 全量实现 `StorageHandle`、`plugin_<id>_*` 分表、
  `plugin_config` + schema 校验。
- 新：**hold**。真机只有 1 个外置插件（`issue.tools`），且它是独立进程 + 自有库（C 档），
  根本不需要 B 档分表。
- 理由：没有消费者。先让 sessions/grants 进库（有 6 个会话、7 条规则在用），插件存储等有第二个
  真实插件再说。
- 翻回条件：出现「需要 core 代管数据」的插件（不是独立进程），或现有 `issue.tools` 要迁进 B 档。

## 6. 产品批：配置单源（用户的真实痛点）

用户的动机（2026-11-09）：「权限调整要动态、持久生效；以前塞在一堆配置文件 + 覆盖，出了很多 bug」。

| id | 目标 | 现状 | 判定 | 状态 |
|---|---|---|---|---|
| F1 | 控制台不再写 `90-console.toml` 覆盖层 | `console.rs:145/574` 仍写覆盖层，顶层 key 整体替换下层 = bug 根源 | **最高优先**：先让控制台写库（或至少写独立片段而不是覆盖层），再做表迁移 | next |
| F2 | sessions 进库 + 能动态新增 | 会话仍在 toml；`acbctl sessions` 只读；控制台只有 GET + reset | 做，但要先解决三个前置：端口显式化、token 来源、drivers 重起 | next |
| F3 | grants/rooms 进库 | 仍在 toml（1 条规则 + 6 个房间覆盖） | 跟 F1 一起做才有意义（否则又是两个源） | next |
| F4 | 插件注册进库 | `[[tool_plugins]]` 在 `80-issue-tools.toml`，command 是 nix store 路径（会 GC 失效） | hold（见 §5 D3/D5） | hold |
| F5 | 插件配置 `plugin_config` + `StorageHandle` | 0 实现 | hold（同上） | hold |
| F6 | agent 能改配置（工具 + 能力 + 审批） | 29 个工具里 0 个能写配置；能力枚举 10 项无配置类 | 依赖 F1/F2；先把能力加上，工具后跟 | next |
| F7 | 权限判定插件化（`policy_binding`） | `Policy` trait 已就位，`CorePolicy` 在 core；无外部实现、无白名单 | hold（没有外部实现需求） | hold |

**F1/F2/F3 顺序理由**：先断掉「第二写入口」（F1），再迁数据（F2/F3）。反过来的话，
控制台还会继续往覆盖层写，迁完立即漂移。

## 7. 安全批（按可利用性）

| id | 问题 | 判定 | 状态 |
|---|---|---|---|
| G1 | 群消息提示注入：昵称/正文原样拼进投递文本，agent 有 shell | **做两件低成本的事**：① 昵称剥换行与控制字符；② 聊天正文加重边界标记（不只插件片段有） | next |
| G2 | 控制台明文 HTTP + token 当 cookie | 见 A4：先 Origin 校验 + cookie 属性 | next |
| G3 | 文件中继 `0.0.0.0:3131` 无鉴权，凭据是 24h 有效的能力 URL | 改绑 podman 网关地址（容器仍可达，局域网不可达）+ 落盘权限收紧 | next |
| G4 | Lua 插件进程内全信任（`io`/`os`/`package`） | **真机已启用 3 个**（`lua.stop_command`/`lua.wake_policy`/`lua.three_hour_clock`）。风险面是「能写 lua 目录的人 = 能执行命令」，不是群成员。判定：文档写清这一条，暂不改沙箱（改了要重写 3 个脚本） | next（文档） |
| G5 | token 明文面 | 见 A3 + A5 | doing |
| G6 | MCP `3101/3102` 回环无鉴权 | 记账，不做（任意本地进程本来就能读 token） | drop |
| G7 | `cargo audit` 未跑 | 跑一次，有结果再定 | next |

## 8. 未验证项（不是待办，是「不知道」）

| id | 问题 | 怎么验 |
|---|---|---|
| H1 | `git2 0.19` 是否有已知安全公告 | `cargo audit` |
| H2 | QQ 是否放行昵称换行（决定 G1 可利用性） | 让某个群成员改昵称试；或查 OneBot 事件原文 |
| H3 | 旧 driver 体系 vs 新 supervisor 是否并存 | 查真机进程与 unit |

## 9. 建议执行顺序

```
本轮：A2 A3 + B2 B3 B4（已完成三条 P0，改动小、可测）
下一轮：C1 C3（都是一行级修复）+ G5 日志脱敏
再下轮：B1 B5（结构性，要设计）+ F1（断第二写入口）
之后：F2 F3（会话与权限进库）
hold：A4 B6 F4 F5 F7 E2 G6（等条件，见 §5）
```

理由：先把「静默丢消息/卡死」这一类清掉（用户感知最强、修复最确定），
再动结构（F1），最后才做迁移（F2/F3）。反过来会在旧结构上叠加新结构，漂移更难查。

## 10. 分支与推送状态

- 本地未推送：13 个提交（含门禁、CI、access_request 修复、payload 修复）。
- CI（`.github/workflows/verify.yml`）只在推送后才会真正跑过一次，**尚未验证**。
  这本身是个待办：推送后看一眼 Actions 是否绿。

## 11. 本轮执行记录（2026-11-09）

改了什么：

| 文件 | 改动 | 对应 |
|---|---|---|
| `crates/kernel/src/route_worker.rs` | 插件 handler 调用包 `catch_unwind`，panic 转 `failed`；worker 循环再加一层兜底，单个动作崩了不带走线程 | B3 |
| `crates/kernel/tests/task_flow.rs` | 新增 `PanicTask` + 用例：panic 后任务 `failed` 且原因指回 handler，紧接着的下一个任务照常 `succeeded` | B3 验收 |
| `crates/bridge/src/main.rs` | 排空判据补 `queued_turns` / `queued_tasks` / `pending_route_actions` | B4 |
| `crates/kernel/src/engine.rs` | `record_and_ack` 的 ack 改成逐个 best-effort，失败只记日志，不返错给模型 | B2 |
| `HANDOFF.md` | 「nix build 会自动跑测试」改为「`nix build` 不跑测试，验证用 `deploy/verify.sh`」 | A2 |
| `config.toml.bak*`（3 份，未跟踪） | `chmod 600` | A3 |

顺手修的门禁缺陷：`rustfmt --check` 默认顺着 `mod` 递归检查子模块，于是「改 `main.rs`」
会把同目录历史遗留不合规的 `console.rs` 算到 `main.rs` 头上，判成假红。加
`--config skip_children=true`，门禁只判本次改动碰过的文件。这条属于「门禁必须可判定、
不许假红」，记在 E4 名下。

验证：`./deploy/verify.sh` — 见 §12。

## 12. 验证证据

`./deploy/verify.sh`（625s，退出码 0）：

```
fmt            OK   14 个文件格式正确
test           OK   通过 1090
node           OK   5 个 harness，144 条通过
guards         OK   3 条约束通过
全部通过。
```

中间结果：

- `cargo test -p acb-kernel --test task_flow` → 6 passed（含新增 `panicking_plugin_task_…`）
- `cargo test -p acb-kernel --test drain_flow --test turn_flow --test endpoint_flow --test task_flow` → 4 + 19 + 6 + 81 全绿
- 基线是 1089 条；1090 = 基线 + 本轮新增 1 条

**注意**：`cargo fmt --all` 会格式化全仓库（本仓库历史文件并不都合 rustfmt），
不要用它；只对改动的文件跑 `rustfmt`，或者依赖门禁的 `skip_children` 逐文件检查。
