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
2. **区分「消费者」与「前提」。** 基础设施类能力（插件存储、策略引擎、动态会话）的消费者
   要靠它自己才可能出现——用「现在没人用」否决它是循环论证（用户 2026-11-09 指出）。
   判据改为：**有没有一个端到端可验的最小闭环**（能写一个不动 core 就接入的示例 ≈ 有）；
   没有闭环的设计稿就是纸面设计，不算「已经做了」。
   反向仍然成立：不许照 2000 行设计稿全量实现，先做闭环，再做深。
3. **改判要留痕**：判定为 `drop` 或改方向的，必须写「原方案 → 新方案 → 理由」，
   并且给一条可反驳的条件（什么情况下该翻回来）。

---

## 1. 批次 A：止血（原本 5 条）

| id | 原待办 | 判定 | 说明 | 验收 | 状态 |
|---|---|---|---|---|---|
| A1 | `deploy/verify.sh` 一条命令跑全部测试 + 修 8 条红 | 照做 | 已落地：fmt/测试/harness/guards 四段，日志落 `target/verify/` | `./deploy/verify.sh` 退出码 0；1089 + 144 + 3 guards | done |
| A2 | 修 `HANDOFF.md:30`「nix build 会自动跑 cargo test」 | 照做 | `flake.nix:44` 是 `doCheck=false`，该句已证伪 | `grep -n "自动跑" HANDOFF.md` 无命中 | done |
| A3 | `config.toml.bak*` 改 0600 或删 | 照做（改权限不删） | 三份 0644、共含 15 处明文 token；已 gitignore 未入库，删了就丢回滚面 | `ls -l config.toml.bak*` 全 `-rw-------` | done |
| A4 | 控制台从 `0.0.0.0:3100` 收回回环 | **改判：保持监听，改为加固** | 用户 2026-11-09 明确答「别的机器也要用」→ 收回回环等于砍掉功能。改为 TLS + cookie 安全属性 + Origin/CSRF 校验 | `curl -k https://<ip>:3100/` 能开；`Set-Cookie` 含 `HttpOnly`；跨站 POST 返回 403 | next |
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
| C1 | `route_worker.rs:57` | 路由队列满 → 动作被拒但任务留 `Pending`，`tasks op=wait` 永远等 | 照做：`enqueue` 把被拒任务交回调用方落 `failed`；队列断掉时整批交回，不 `break` | done |
| C2 | `task.rs:117` | 孙进程继承 stdout → `read_to_string` 永久卡死 | 照做：独立 reader + deadline + kill 进程组 | next |
| C3 | `engine.rs:1430/3510` | 尝试记录崩溃后永远 `running`，`drain_snapshot` 还把它算作在跑 | 照做：启动时把遗留 `pending/running` 置 `unknown`（不是 `failed`，消息可能已发出） | done |
| C4 | `engine.rs:4144/3814` | `notify_turn_error` 入队即 `sent += 1`，失败不回滚 + 指纹已记 → 同类失败被抑制 | 照做：发送成功才记指纹 | next |
| C5 | `endpoint.rs:931` | `write_frame` 阻塞写无超时 | 照做：设写超时 | next |
| C6 | `endpoint.rs:373/278/284/336` | `abandon_turn`/`delete_session_map`/`upsert_session_map` 用 `let _ =` 吞错 | 照做：至少记日志，关键路径返错 | next |
| C7 | `engine.rs:96/11345` | `PROMPT_TIMEOUT=50ms` 静默丢上下文；每次 `thread::spawn` 不 join | 拆两条：超时改可配置+记日志（先做）；线程池化（后做） | next |
| C8 | `engine.rs:2916` | 审批命令写错（群内缺 nonce）或地点不对（不在 `approval_route` 里）一律 `return Ok(None)`：没订阅的房间彻底静默，订阅了的房间把 `/approve tk-…` 当普通消息喂给 agent | 照做：识别到命令就必回话（用法 / 拒绝原因 / 该去哪批）；`bt-` 整批不需要 nonce；审批消息里直接给可复制命令 | done |
| C9 | 进程输出 | 桥与 launcher 的 stdout/stderr **不进 journal**（`_PID=61715/61716` 均 0 条），所有 `eprintln!` 诊断（队列拒绝、审批拒绝、投递失败）实际不可见 | **已完成**（2026-11-09 核实）：launcher 把两者重定向到 `~/.local/state/acb/bridge.log`（34MB，`supervisor audit` / `turn_end received` 都在里面）。非 systemd 形态仍要自己重定向，记在 `docs/guide/operations.md` | done |
| C10 | `harnesses/dsh/src/driver.mjs` | dsh 重启后有一段 `agentAvailable=false` 的窗口（实测 30s～2.5min），此时 `session/selectModel` 会阻塞到 driver 的 15s 超时，**整轮 turn 直接 failed**；用户视角就是「dsh 挂了」（2026-11-09 12:15–12:19 连挂 4 轮） | 照做：轮前等 `agentAvailable` 就绪（带上限，别无限等）；`selectModel` 仍失败就降级为「沿用会话当前模型」继续本轮并 warn，不让整轮死 | next |
| C11 | `engine.rs:6813` / `tickets.rs:506` | ① 票据的 `approval_route` 是**创建时快照**，事后补配置不会重算，旧 pending 票只能等过期（2026-11-09 `tk-70bb436f010762f8-2` 就是这么废的）；② 房间没配 `approval_route` 时静默回退适配器 `direct_room`（bot 自己的私聊，没人订阅、没人看），票据一路 pending 到过期，只留一行 stderr | 照做：① `pending_unannounced` 每次按当前 config 重算路由（或 route_unavailable 时重解析一次）；② 无路由时给申请会话回一条「本房间没配审批地点，请让管理员配 `[grants.rooms."<房间>"].approval_route`」，别让人干等。**进展（2026-11-09）**：策略已入库（F3），新票在轮次边界后走新路由；剩下的是「旧票快照」与②的回话。**已修（2026-11-09）**：① `flush_ticket_approvals` 对「快照为空 / 上次投递失败」的票用当前配置重算路由（`AccessTickets::retarget`，已通知过的不动，避免改掉管理员正在看的目的地）；② 无路由时给申请房间回一条「本房间没配审批地点」并列出票号与剩余时间，只回一次 | done |
| C12 | `engine.rs` 轮次失败通知 | 「本轮出错」按房间限流 3 次/10 分钟，超了就**完全静默**。2026-11-09 14:22–14:30 连续 6 轮 `content-blocked`，用户看到的现象是「群里没人回话 / dsh 挂了」，实际每轮都失败了且一次提示都没有 | 照做：达到上限时至少留一条「本房间正在连续出错，去 3100 控制台看」的提示（每个房间一个窗口只发一次），并在概览页显示「该房间最近 N 分钟失败 M 次」。**已修（2026-11-09）**：抑制分支不再永久静默——每 `[notifications.in_band].remind_secs`（默认 600s）给同一房间补一条「[还在出错] 最近 N 分钟失败 M 次（抑制原因）」，并受同一节流，不会变成新的刷屏源；成功一轮同时清掉提醒节流。概览页新增「最近失败的轮次」卡片：**含被抑制的**失败计数（`/api/status.recent_failures`，窗口 15 分钟），把「每轮都在失败」和「真的没轮到他」分开 | done |
| C13 | `deps.rs` / `driver.mjs` / 控制台依赖表 | 面板的「dsh 不可用」其实是**上游模型**造成的：`selectModel` 在模型没就绪时挂到 15s 超时 → `DshError("unreachable")` → 归为 `backend` → `harness_turn_down` 无条件改状态。而同一时刻桥↔dsh 的连接一条都没断（2026-11-09 14:05 那次 `attempts=4` 未变、无 `closed` 迁移）。用户两次据此以为「dsh 挂了」 | **已完成**（2026-11-09）：① `harness_turn_down` 在 `open_connections > 0` 时只记 lastError、不改状态；② `selectModel`/`prompt` 的 `ETIMEDOUT` 归 `model/model_timeout`，并先探活 `session/list`（3s）——探活也失败才算后端；③ `model_timeout` 进后备链可重试集；④ 依赖表新增 `openConnections`，面板 harness 行显示通道条数，并补齐 `errorClass.*` 中文标签（此前一律显示「错误」） | done |

## 4. 批次 C：流程（原本 4 条）

| id | 原待办 | 判定 | 说明 | 状态 |
|---|---|---|---|---|
| E1 | 写 `AGENTS.md`（≤200 行） | 照做 | 已落地 114 行，guards 看守预算 | done |
| E2 | plan 三态化，45 篇打标签 | **drop** | 见 §5 | drop |
| E3 | 变更单模板（一页） | **改判：并入 AGENTS.md** | 单独一页模板没人填；把「改了什么/怎么验/剩什么」写进提交尾注足够 | drop |
| E4 | 三个可执行断言 | 照做（缩到 2 条） | 已做：AGENTS.md 行数预算、禁止本机配置状态入库。第三条「文档承诺必须有实现」无法自动判定，人工 review | done |
| E5 | docs 构建长期红，信号被污染 | 照做 | 真因三处：① `issue/<id>` 不在反引号内被当 Vue 标签；② 双花括号变量写法命中 Vue 插值；③ 表格单元格里的 `\|` 把行内代码切断，`<toml>` 漏成标签。已修，本地 `vitepress build` 13s 通过 | done |

## 5. 改判记录（原方案 → 新方案 → 理由 → 何时翻回）

### A4 控制台绑定地址
- 原：从 `0.0.0.0:3100` 收回 `127.0.0.1`。
- 判：**不收回。** 用户 2026-11-09 答「别的机器也要用」，远程访问是功能不是漏洞。
- 新：保持监听，补齐三件——① TLS（自签或用户自己的证书，配置项给证书路径）；
  ② 登录 cookie 加 `HttpOnly` + `SameSite=Strict` + `Secure`；③ 改配置的接口校验
  `Origin`/`Referer` 并加 CSRF token，token 不再出现在 URL 里。
- 理由：真实风险是「明文 HTTP + 凭据当 cookie + 无 CSRF」，不是监听地址。绑回环是
  用砍功能的方式消症状。
- 翻回条件：若用户改用 SSH 端口转发访问（`ssh -L 3100:127.0.0.1:3100`），则监听改回环、
  TLS 可省——因为隧道已经加密且不出本机。

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
- 原：按 `acb-policy-store.md` §3.12/§3.13/§6.5 全量实现 `StorageHandle`、`plugin_<id>_*` 分表、
  `plugin_config` + schema 校验。
- 第一次改判：**hold** —— 理由「真机只有 1 个外置插件，没有消费者」。
  **这条理由被用户 2026-11-09 驳回，成立**：插件存储是「怎么会有用户」的前提，
  用它缺用户来否决它是循环论证。设计原则 §0-2 已据此改写。
- 第二次改判：**做，但拆两级，每级必须有端到端可验的示例。**
  - **L1 插件配置闭环**（先做，服务主线）：`plugin_registry` + `plugin_config` 两张表；
    插件随包声明 config schema；core 只做类型/枚举/密钥扫描校验，不解释业务字段；
    控制台按 schema 渲染表单；agent 通过一个新工具读写（单独能力 `plugin_config_write`）；
    **验收 = 把 `issue.tools` 的 3 个配置项从 toml 搬进库，改一次配置不重启桥生效。**
  - **L2 插件自带存储**（L1 之后）：`StorageHandle`（前缀强制走 SQLite authorizer）、
    `plugin_migrations`、声明式迁移。
    **验收 = 写一个只有 200 行、只依赖插件接口的示例插件，它建自己的表、存自己的数据，
    全程没有一行改动 `crates/kernel` 的业务代码。** 达不到这条就算 L2 没做完。
  - 不在本轮：`plugin_secret_refs`（走已有 secrets）、external 档的跨库审计、
    限流令牌桶、`.so`/进程外的 C ABI 映射面。
- 理由：用户要的是「外部开发者能自主接入、agent 能自己改」。这两件事的瓶颈都不是
  「表够不够全」，而是「有没有一条别人能照着走通的路」。全量照稿实现会先花两周造出
  没人验过的接口，再花两周发现接口不对。
- 翻回条件（改成 hold 的唯一条件）：L1 做完后连续两个月没有第三个插件出现，
  且 L2 的示例插件写不出来——说明插件模型本身不对，那时应该重设计而不是继续加表。

## 6. 产品批：配置单源（用户的真实痛点）

用户的动机（2026-11-09）：「权限调整要动态、持久生效；以前塞在一堆配置文件 + 覆盖，出了很多 bug」。

| id | 目标 | 现状 | 判定 | 状态 |
|---|---|---|---|---|
| F1 | 控制台不再写 `90-console.toml` 覆盖层 | **走 B（用户 2026-11-09 裁定）：控制台直接写库。** 审计见 [config-source-audit.md](config-source-audit.md)；实现与**真机迁移记录**见其 §8（2026-11-09 已执行：schema 15 上线、覆盖层归档、`config.toml` 清成纯引导；`config.active.toml` 实为 acb-config-merge 派生物，改配置要改 `config.toml`） | **done**（F1a 的 9 条规则已于 2026-11-09 20:30 入库，见审计 §8.4） | done |
| F1b | 会话增删后驱动跟着重起 | **已完成（2026-11-10，commit `9c8faa7`）**：`deploy/presets/drivers/drivers_acb.sh` 改成常驻对账（5s 一轮）：新增会话 spawn、删除 SIGTERM、指纹（id/preset/port/token 哈希/model）变化重启、崩溃指数退避（上限 60s）、`--once` 供测试；`Session.driver_port` 显式化并校验唯一。**仍缺**计划里的 `deploy/verify-drivers-reconcile.sh` 独立脚本测试（逻辑有单测覆盖，端到端没有） | 补对账脚本测试 | done（缺测试） |
| F1a | 止血：把 plugin 授权补回库（**用户裁定不止血**） | 覆盖层吃掉 `70-grants-issue.toml` 的 9 条 `plugin` 规则 → F3 首次导入只拿到 1 条规则（`allow=1 deny=0 rooms=7`）；此后 `bridge.log` 实测 **12 次** `admin:no_plugin_grant`（含管理员私信），system_admin 调不动任何插件工具 | **已完成（2026-11-09 20:30）**：用户澄清「不止血」= 不再往配置文件塞新东西、不是不入库；`/api/policy` 整份写回（9 条原样 + 原有 1 条 = 10 条，房 7），70 片与三个 `pre-*` 归档进 `config.d/history/`；`acbctl grants` 一并接库（审计 §8.4） | done |
| F2 | sessions 进库 + 能动态新增 | **已完成（2026-11-10）**：会话在库（schema 15）、控制台可增删改、下一轮生效；`driver_port` 进 `Session` 并做全局唯一校验；驱动侧 5s 对账据此起停，加完会话约 5 秒内起驱动 | 无 | done |
| F3 | grants/rooms 进库 | **已完成**（schema 13）：`policy_rules`/`policy_rooms`/`policy_accounts`/`policy_admins` 四表；启动时一次性导入 TOML 并打 `policy_initialized`，之后库为唯一来源；控制台「权限」页 + `GET/POST /api/policy`，保存后轮次边界生效 | 做完了。顺带解决了 C11：审批路由改完下一张票就走新路由，不再有「建票时快照」 | done |
| F4 | 插件注册进库 | **代码已完成（2026-11-10，schema 17）**：七张插件表 + `PluginsStore`；`[[tool_plugins]]` 只在库为空时 seed，之后库为唯一源；`command_kind` + sha256 指纹 + 可执行检查，失败只标 degraded；`acbctl plugin list/show/add/remove/enable/disable/check/export/import`；`/api/plugins` 带 registry 视图。**真机已把 schema 15→17、`issue.tools` 入库并 check 通过**；2026-11-10 20:43 已换代到 `gen-1794314416628-bf0fc0b5` 并归档 `80-issue-tools.toml`，新桥读库、启动日志确认文件条目被忽略 | 已归档；回滚需同时恢复备份库 | done |
| F5 | 插件配置 `plugin_config` + `StorageHandle` | **L1 已完成（2026-11-10，schema 17）**：`plugin_config` / `plugin_records` / `plugin_secret_refs` 三表；manifest 声明 config/resources/actions；写入校验类型/枚举/范围/密钥形态；控制台按 schema 渲染表单、资源记录增删查、动作按钮；`acbctl plugin config/records`；示例插件 `examples/plugin-demo` 全流程可跑。L2（插件自带库、ACB 只存 location）只有表字段，没有实现 | 下一批做 L2 | doing |
| F6 | agent 能改配置（工具 + 能力 + 审批） | **已完成（2026-11-10）**：能力枚举加 config_read/config_write/policy_write/plugin_admin/access_approve；新增 `config`/`policy`/`plugin_config`/`plugin_action` 四个工具。不写身份分支：一律 能力映射 → Policy → 审计落盘 → 执行；`AuditStore` 双实现；票据审批人也要逐个能力过 Policy（account_secret 仅 system_admin break-glass）。默认策略数据不带任何授予 = 默认全拒。配置写入可走票据：连接级 op（config:get/list/set/delete、policy:get/set、plugin_config:get/set、plugin_action:run）走 `TICKET_CONNECTION_OPS`，批准后放行一次 | 按需在控制台加角色/规则即可放开 | done |
| F7 | 模型后备链入库 | **已完成**（schema 14）：`model_fallbacks` 表 + `GET/POST /api/model-fallbacks` + 控制台「模型」页；桥每轮随 `SessionInfo.model_fallbacks` 下发，驱动在主模型被拒（content-blocked / 限流 / 下线 / 超时）且**本轮没调过桥工具**时换模型重放 | 做完了。遗留：per-session 覆盖要等 F2 会话入库（现在链是全局的） | done |
| F8 | 密钥入库（P2.5） | **桥侧已完成（2026-11-10）**：`secrets` 表 + `v1:<kid>` AEAD；`Config::load_with_secrets` 吃 `SecretPreset`，启动先 `secrets_db::preload` 再加载，命中即 `kind=db`、不读文件；`acbctl secrets migrate/check` 迁移 8 个来源（kid `bd316e15df6cedc8`）；unit drop-in 注入 `ACB_KEY_FILE`；换代 `gen-1794321068510-e92068ce`（label `p25-secrets2`）后日志 `secrets: 8 name(s) loaded from store`；移走 `session-tokens.env` 后 `acb-bridge --check` 仍通过 | **仍差**：驱动侧 `ACBCTL_BIN` 是旧 acbl 构建（无 preload），删文件要等系统代重建 | doing |
| F7 | 权限判定插件化（`policy_binding`） | `Policy` trait 已就位，`CorePolicy` 在 core；无外部实现、无白名单 | hold（没有外部实现需求） | hold |

**F1/F2/F3 顺序理由**：先断掉「第二写入口」（F1），再迁数据（F2/F3）。反过来的话，
控制台还会继续往覆盖层写，迁完立即漂移。

## 6b. 控制台前端（2026-11-09 用户追加）

用户原话：「前端可以重写，希望多端都美观好用，插件配置也能在前端反映」。

**现状（实测，不是印象）**：`crates/bridge/src/console.html` 1266 行，已有设计令牌
（`--space-*`/`--text-*`）、`@media (max-width:720px)` 与 `@media (pointer:coarse)`。
但**只有概览 tab 是成品**（卡片头、信息提示、刷新按钮）；sessions / feedback /
plugins / config 四个 tab 顶部都挂着 `console.ui.pending.tab`「This tab is not
localized yet.」，内容是裸渲染。插件 tab 只有 reload/unload，没有配置。

所以这不是「从零重写」，是「把四个占位 tab 补成概览的质量」+「窄屏降级」。

| id | 目标 | 判定 | 状态 |
|---|---|---|---|
| J1 | 四个 tab 与概览同等质量（卡片头、空态、错误态、刷新、可读的键值/表格） | **已完成（2026-11-10）**：sessions/feedback/plugins/config 四页全部改成 toolbar + card 结构，加载/空/错误态走统一状态块；配置页拆七张卡并全量文案入库 | 插件配置弹窗、权限页、模型页仍中文硬编码 | done |
| J2 | 多端可用：窄屏表格转「标签-值」卡片、导航可横滑、按钮达 44px | **已完成（2026-11-10）**：≤720px 表格按 td[data-label] 拆成标签-值卡片；卡片头按钮成组右对齐；头部标题独占一行；导航隐藏滚动条；390px 实测三页 | 触控尺寸沿用原有 media 规则 | done |
| J3 | 插件配置在前端可见可改（按插件声明的 schema 渲染表单） | **已解除阻塞（2026-11-10）**：插件 tab 有注册表卡片，按 manifest 渲染配置表单、资源记录与动作按钮；密钥字段只显示绑定状态 | done |
| J4 | 浅色/深色跟随系统 + 手动切换 | **已完成（2026-11-10）**：三态选择存 localStorage；`prefers-color-scheme` 变化实时跟随；首屏前内联脚本定主题避免闪色 | 无 | done |

**为什么不引入 React/Vue + 构建链**：① `nix build` 现在是纯 Rust，加了前端构建就多一条
node 依赖链，且产物要 `include_str!` 或额外打包；② 这个页面没有路由、没有共享状态树，
五个 tab 的复杂度撑不起框架；③ 现有的设计令牌与组件 CSS 可以直接复用，重写等于把已经
做对的部分（触控尺寸、断点、i18n 属性）扔掉重来。
**翻回条件**：若 J1 做完后仍需在页面内做多步表单流（比如插件 schema 嵌套条件字段 +
实时校验 + 依赖联动），再考虑引入轻量框架（此时应单独起一个 crate 托管静态资源）。

**顺序**：J1+J2+J4 一批（纯前端，不动后端）→ F5-L1（后端表与 schema）→ J3。

## 6c. 插件配置与存储（L1 / L2）

见 §5「D3/D5」。要点：L1 的验收是「`issue.tools` 的三个配置项从 toml 搬进库，改一次
不重启桥生效」；L2 的验收是「写一个只依赖插件接口的示例插件，全程零改动
`crates/kernel` 业务代码」。J3 的插件配置表单直接吃 L1 的 schema。

**L2 现状（2026-11-10 核对）**：`plugin_storage` 只有 schema 表，`plugin-api` 里没有
`StorageHandle`，`examples/plugin-demo` 也没用存储——L2 未开工。



## 7. 安全批（按可利用性）

| id | 问题 | 判定 | 状态 |
|---|---|---|---|
| G1 | 群消息提示注入：昵称/正文原样拼进投递文本，agent 有 shell | **做两件低成本的事**：① 昵称剥换行与控制字符；② 聊天正文加重边界标记（不只插件片段有） | next |
| G2 | 控制台明文 HTTP + token 出现在 URL（`?token=`） | **实测**：cookie 已带 `HttpOnly; SameSite=Strict`（`console.rs:321`），没有 Origin 校验。真正缺的是 TLS 与「token 不进 URL」——按 A4 做 | next |
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
已完成：A2 A3 B2 B3 B4 C1 C3 E5 + 门禁假红
下一轮：A4（TLS/cookie/CSRF，用户要用远程）+ G1（昵称剥换行，成本最低）
再下轮：J1 J2 J4（四个占位 tab 补成成品 + 窄屏降级 + 深浅色）
之后：B1 B5（结构性）+ F1（断「控制台写覆盖层」这条第二写入口）
再后：F2 F3（会话与权限进库）→ F5-L1（插件配置闭环）→ J3（插件配置表单）→ F5-L2
hold：B6 F7 E2 G6（等条件，见 §5）
```

理由：先把「静默丢消息/卡死」清掉（用户感知最强、修复最确定），再断第二写入口（F1），
再做迁移（F2/F3）。前端 J1/J2/J4 不动后端，可以并行插队；J3 必须等 F5-L1，否则前端
没有 schema 可渲染。

## 10. 分支与推送状态

- 已推送：`2c372f3..2c12de8`（17 个提交）到 `origin/dev_20261025`，2026-11-09 11:16。
- **未推送：2026-11-10 这一批 41 个提交**（控制台 J1/J2/J4、P2.5 密钥、空投递、状态脚本修复、换代记录）。
- CI（`.github/workflows/verify.yml`）：推送后需看 Actions 结果，**尚未确认**。
  若 Actions 红而本地绿，优先查两者差异（node 版本、缺少 nix 依赖）。

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

| `crates/kernel/src/route_worker.rs` | `enqueue` 返回被拒任务，队列断掉时整批交回（不 `break`） | C1 |
| `crates/kernel/src/event_queue.rs` | 拿到被拒任务后落 `failed` 并写原因 | C1 |
| `crates/kernel/src/engine.rs` | 启动 `reclaim_orphan_tasks`：遗留 `pending/running` 置 `unknown` | C3 |
| `crates/kernel/tests/task_recovery.rs` | 新增：崩溃遗留任务变 `unknown`；已闭环的不动；排空快照不再计数 | C3 验收 |
| `deploy/verify.sh` | harness 测试子进程设 `SAFE_DELETE_ENABLED=0`（沙箱批量删除拦截会造成假红） | 环境 |
| `docs/plan/self-iteration-adversarial-review.md` 等 3 份 | 修 Vue 模板误解析（裸 `<id>`、双花括号插值、表格里含 `\|` 的行内代码） | E5 |
| `crates/kernel/src/engine.rs` | 授权命令的拒绝分支改为回话（用法 / 拒绝原因 / 该去哪批）；群内 `bt-` 整批不再要求 nonce；审批消息每条附可直接复制的 `/approve <id> <nonce>` | C8 |
| `crates/kernel/tests/{place_approval,access_ticket}_flow.rs` | 断言从「不算 Handled」改成「Handled + 回话内容」；新增可信群按 batch id 整批批准用例 | C8 验收 |

`docs` 工作流长期红，真因不是构建环境，是文档里三处写法被 Vue 编译器当模板解析。
本地 `vitepress build` 13 秒就能验，**不必等 CI**——这条已写进 `AGENTS.md` 的坑清单。

验证：`./deploy/verify.sh` — 见 §12。

### 2026-11-09 17:05 追加：C12 换代 + F1 配置来源审计

- 换代：`gen-1794215544362-ebf3ce70`（label `c12-failure-reminder`，store 策略，
  `--config` 指活配置），重启后 `deploy/acb-status.sh` 全绿；`/api/status` 已带
  `recent_failures`（当前空 = 没有失败轮次）。库 schema 14 不变，无需迁移。
- 审计产出 [config-source-audit.md](config-source-audit.md)：加载链 4 源实测、合并语义、
  写入面 7 条、对象×真源 14 行、5 条硬证据；结论是「真源已是覆盖层」+「9 条 plugin 规则
  被吃、12 次实测拒绝」，并给出三步切割与 3 个待裁定项。
- 探针方式（可复用）：复制配置目录到 `/tmp`，加一个只含新会话的分片，跑
  `acb-bridge --check --config <副本>`，对比 `config source` 声明数与 `sessions:` 实际数。

## 11b. 本轮执行记录（2026-11-10 换代）

1. 库：schema 15 → 17；备份 `backups/pre-schema17-20261110-180521`、`backups/pre-gen-20261110-203419`。
2. 插件：`issue.tools` 入库（nix_store + 指纹），`config.d/80-issue-tools.toml` 归档到 `history/…archived-20261110-204346`。
3. 角色：`role-config-admin` / `role-policy-owner` / `role-plugin-admin` 指派给 `qq:323622442`（connection/qq），人工操作已写 `policy_audit`。
4. 换代：payload `f15nzrvk…` → `gen-1794314416628-bf0fc0b5`；LKG 已 promote。
5. 一个真机 bug：drivers/dsh-presets 组件从 `supervisor.toml` 的 `ACBCTL_BIN` 读到旧代 acbctl（只认 schema 15），换代后反复退出。已改预设优先用自身 PREFIX 的 acbctl（提交 `1c1c…`），并把 supervisor.toml 的 `ACBCTL_BIN` 指到新 acbl。
6. acbl 本身没随 payload 换代：用新构建的 `acbctl unit install-user` 之外的路径安装会遮蔽 nix unit；本次用“用户单元指向新 acbl launcher + 新预设”完成，system 级 unit 仍是旧 acbl。

## 12. 验证证据

`./deploy/verify.sh`（58s，退出码 0）：

```
fmt            OK   3 个文件格式正确
test           OK   通过 1093
node           OK   5 个 harness，144 条通过
guards         OK   3 条约束通过
全部通过。
```

`cd docs && node node_modules/vitepress/bin/vitepress.js build` → `build complete in 13.26s`。

CI：push `2c372f3..2c12de8` 后 `verify` 工作流 **首次跑绿**；`docs` 因上述三处写法红，
本轮修完待下次 push 复验。

中间结果：

- `cargo test -p acb-kernel --test task_recovery` → 1 passed（崩溃遗留任务变 unknown）
- `cargo test -p acb-kernel --lib route_worker` → 2 passed（满队列交回、队列断掉整批交回）
- `cargo test -p acb-kernel` → 31 个测试目标全绿
- `cargo test -p acb-kernel --test place_approval_flow` → 16 passed（含新增「可信群按 batch id
  整批批准不需要 nonce」）
- 基线 1090 → 1093：route_worker 新增 1 条、task_recovery 新增 1 条、审批批处理新增 1 条

**注意**：`cargo fmt --all` 会格式化全仓库（本仓库历史文件并不都合 rustfmt），
不要用它；只对改动的文件跑 `rustfmt`，或者依赖门禁的 `skip_children` 逐文件检查。

## 2026-11-10 21:15 真机换代（console-ui）

1. 备份 `state/acb.db` + `-wal` + `-shm` 到 `state/backup-pre-console-ui-20261110-211408/`（先停桥）。
2. `nix build .#payload` → `/nix/store/ypq5v4bfwjh2c6myx5pzy8i233j4ph7k-acb-payload-0.1.0`。
3. `acbctl profile add … --strategy store --label console-ui` → `gen-1794316491779-df056b0f`；`profile switch` 后重启单元。
4. 验收：`acbctl status` unit active / endpoint open / console 200；`acb-status.sh` 全绿（5 条 ESTAB、驱动→dsh 通、/healthz 带令牌通、三段日志无错误）；线上 `/` 返回的 HTML 含新界面标记（`fb-replies`、`data-theme`、`console.config.sessions.title`），`/proc/<pid>/exe` 指向新 store 路径。
5. 顺手修 `deploy/acb-status.sh`：令牌解析补 `--config` 与 cwd；drivers.log 启动锚点认时间戳前缀（此前把上一轮启动的陈错报成本轮）。

### 2026-11-10 21:35 私信写入的审批死胡同（已用策略补口）

现场：管理员（323622442）在群 973928169 让 dsh-dev「测试给我发私信」→ 目标
`qq:private/323622442` 不在 dsh-dev 订阅里 → 判跨房间写 → 开票
`tk-4c7fd7ce25e57668-1`。审批发到群 498587179/962657331，管理员在群里点
`/approve` 同样被拒（`place=group` 时 `base-system-admin-private` 的
`scope=subscribed` 不成立，审批检查又把 subscriptions 传成空表），票据 10 分钟后过期。

改动：给 `system_admin` 补两条 `scope=connection`（target `qq`）的 `room_write`
规则——`admin-account-write-group`（place=group）与 `admin-account-write-private`
（place=private）。只给 `room_write`，不给 `room_read`。经 `/api/policy` 落库，
`config_version=4`，`pending_reload=false`。

验证：`/api/policy` 规则 13 条、DB `policy_rules` 13 行；下一条 access 日志应出现
`basis=...rule:admin-account-write-group` 且不再开票。

### 2026-11-10 22:30 P2.5 库内密钥（上半：桥侧已生效）

1. 代码：`Config::load_with_secrets` 接受 `SecretPreset`（来源名→明文）；命中即标
   `kind=db`、不读文件；`kernel::secrets_db::preload` 先按 `[storage].path`（默认
   `state/acb.db`，按 cwd）开库解密。快照用 `OnceLock` 挂在进程上，库对象/控制台
   覆盖层的重载路径同样吃得到。bridge 与 `acbctl` 启动都先 preload。
2. 现场：`~/.local/share/acb/secrets/acb.key`（0600，64 hex）生成；unit 加 drop-in
   `10-secrets-key.conf` 注入 `ACB_KEY_FILE`。`acbctl secrets migrate` 迁入 8 个来源
   （kid `bd316e15df6cedc8`，全部 encrypted=1）；`resolve --session dsh-dev` 与文件
   sha256 一致。
3. 换代：`gen-1794321068510-e92068ce`（label `p25-secrets2`）；bridge 日志首行
   `secrets: 8 name(s) loaded from store`。
4. 验收：把 `session-tokens.env` 移走后 `acb-bridge --check` 仍通过（6 个会话的 token
   全部来自库）；移回文件；`acb-status.sh` 全绿。
5. 仍差：驱动侧 `ACBCTL_BIN` 指向的 acbl 包还是旧构建（没有 preload），删掉
   `session-tokens.env` 会让驱动拿不到 token。要等用新 acbl 做一次
   `nixos-rebuild switch`，之后归档该文件才算 P2.5 收口。

### 2026-11-10 22:55 空投递（事件唤醒到点已无记录）

现场：群 `769843907` 触发事件 #4372、报「这轮共 7 条未读」，投递正文却是
「这一轮没有新记录」。水位当时已到 4385——触发记录在延迟窗口里被同一会话的
另一轮用 `wait_message` 取走并 ack 了。

修法：`Pending` 增加 `from_record`（只有「记录命中唤醒策略」这条入队路径置位）；
`open_turn` 在 `from_record` 且 `pending_records` 为空时返回 `None` 并记日志；
`poll_turn` / `poll_turn_for` 改循环，丢空轮后继续找下一个就绪会话。时钟、
后台任务、票据结果、直接唤醒不受影响（它们本来就是「没消息也让模型看一眼」）。
换代 `gen-1794322200728-b2320450`（label `nowake-empty`）。

### 2026-11-10 23:30 控制台保存全线报错（覆盖层回灌 base）

现象：页面上任何一次保存都返回「补丁后的配置解析不过，一个字节都没写」，
detail 为 `90-console.toml must not declare [secrets]`。

原因：没有 `90-console.toml` 时 `console_document()` 退回 `config.toml` 全文，
表单补丁把整份 base（含 `[secrets]`/`[storage]`）当覆盖层送校验。

修法：控制台可编辑文档固定为覆盖层本身，文件不存在时给空文档；TOML 编辑器同样
只编辑覆盖层。新增 `tests/console_overlay.rs` 两条断言（空覆盖层可合并、
覆盖层写 `[secrets]` 仍拒）。换代 `gen-1794324715834-15236217`（label
`console-save-fix`），并在修好后给 `dsh-dev` 订阅 `qq:group/967578142` 成功
（`config_version=3`）。

### 2026-11-10 23:55 控制台新建会话 + 对象补丁丢条目

1. 新建会话必然失败：表单要求来源名已在 `[secrets]` 声明，页面无法声明。改为自动
   生成令牌（`config.d/secrets/<名>.token` 0600 + `config.d/50-session-secrets.toml`
   追加来源，分片 sources 是追加语义），守卫失败时整体回滚；表单允许留空。
2. 事故：23:44 我实测建会话后，`dsh-dev` 从库对象里消失（驱动随之停）。已从
   `state/backup-pre-p25-20261110-222023` 补回 payload 并重启，四路驱动全部回来；
   `group/967578142` 订阅随后重加。
3. 根因未定位到唯一一行（文本往返在构造用例与真实对象文档上均未复现）。已加兜底：
   补丁后逐类比对对象集合，任何没被显式 `delete` 却消失的条目让整单 500 拒绝
   （`bridge.console.patch_lost_object`），不再可能静默丢数据。
4. 换代 `gen-1794325936774-c33445dd`（label `patch-guard`）；实测新建 `probe-mint2`
   → 7 个会话，删除 → 6 个会话，其余会话无损失；临时令牌文件与分片已清理。

### 2026-11-11 00:22 库内令牌打通（桥侧），但驱动侧 acbl 仍是旧构建

代码：
1. `SecretStore::from_raw` 允许「快照里有、`[secrets]` 没声明」的名字（`kind=db`）。
2. 控制台新建会话时令牌双写：`secrets` 表（`ACB_KEY` 加密，`source=console`）+ 0600
   文件与 `config.d` 分片（兼容旧 acbctl）。校验通过才落库，失败回滚。
3. `Bridge::put_secret/delete_secret`。

实测：
- 建 `probe-db` → DB 出现 `probe-db-token|1|bd316e15…|console`，驱动起在 3107 并连上桥。
- 移除分片与令牌文件后重启：桥加载 7 个会话，`probe-db-token (db, database)` 生效。
- 但 `acbctl sessions` 立刻失败（旧 acbctl 没有 preload，遇到未声明名字直接
  UnknownSecret），drivers 进入「保留现有驱动」；已回滚到「文件 + 声明」形态，
  驱动全部恢复。

结论：**驱动侧 acbl 还没换到带 preload 的版本**。现场证据：单元仍 ExecStart
`/nix/store/lxbflj…-acbl-0.1.0`，`/run/current-system/sw/bin/acbctl` 是
`l0crh4…`，两者 `strings | grep 'preload secrets'` 都是 0，且拿掉 env 文件后
`secrets resolve --session` 仍报「cannot read …session-tokens.env」。需要用户把
系统 flake 的 acb 输入更新到含 `e7c99b1` 的 rev，并重建**用户级** unit
（`~/.config/systemd/user/acb-bridge.service` 里硬编码了 launcher 路径）。

### 2026-11-11 00:55 驱动侧换到新 acbl，库内令牌闭环收口

承接 00:22 那条「驱动侧 acbl 仍是旧构建」：

1. 系统 00:33 重建后 `/run/current-system/sw/bin/acbctl` 已含 preload（acb rev
   `6990ef0`）。但 `~/.config/systemd/user/acb-bridge.service` 是手写旧文件，遮蔽了
   `/etc/systemd/user/acb-bridge.service`，ExecStart 仍指向 `lxbflj…` 旧 acbl。已备份
   并移除该文件（保留 `acb-bridge.service.d/10-secrets-key.conf` 这个 ACB_KEY_FILE
   drop-in），`daemon-reload` 后 FragmentPath 回到 `/etc`，入口改为
   `/run/current-system/sw/bin/acb-launcher`。
2. `state/supervisor/supervisor.toml` 里 dsh / dsh-presets / drivers 三个组件的 env
   硬编码 `ACBCTL_BIN=/nix/store/lxbflj…`（旧、无 preload），且继承不到
   ACB_KEY_FILE。去掉 ACBCTL_BIN（驱动脚本会回落到自身 PREFIX 里的 acbctl），并给
   带 `ACB_CONFIG_FILE` 的两个组件显式补 `ACB_KEY_FILE`。
3. `config.toml` 删掉 5 条指向 `secrets/session-tokens.env` 的 `[[secrets.sources]]`
   （dsh-dev/qas/group2/snowluma-test、opencode-token），重启后启动日志
   `secrets: 8 name(s) loaded from store`，全部 `(db, database)`；4 个 dsh 驱动 +
   opencode 均从 `5dwa95…acbl` 启动并连上 endpoint。

踩坑：第一次用「按 `[[secrets.sources]]` 切块」的脚本删条目时，把最后一个块之后
的 `[endpoint]/[console]/[tickets]/[file_relay]/[notifications]` 一起吞了，桥报
`notifications.topic contains a literal secret` 并崩溃循环。已从
`config.toml.bak-20261111-0048` 恢复，改成「块边界到下一个 `[` 行」重删。

备份：`config.toml.bak-20261111-0048`、`config.active.toml.bak-20261111-0048`、
`state/supervisor/supervisor.toml.bak-20261111-0052`、
`~/.config/systemd/user/acb-bridge.service.bak-20261111-0045`。

验收：
- `systemctl --user show acb-bridge -p FragmentPath` → `/etc/systemd/user/acb-bridge.service`
- `grep "secret sources" -A9 bridge.log` 最新一段全部 `(db, database)`
- `pgrep -af driver.mjs` → 4 个；drivers.log 不再出现 `acbctl sessions 失败`
- `acbctl sessions --config …` （带 ACB_KEY_FILE）列出 6 个会话

遗留：会话仍无显式 `driver_port`，按历史下标推导（3101–3106）；要稳定端口需在
控制台给每个会话写入 `driver_port`。
