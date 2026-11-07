# ACB 升级链：ACB 发起、acbl 执行

状态：2026-11-07，设计稿。**不改代码**；目标是把现有 `profile`、`current.json`、
`lkg.json`、75/78 和三种部署后端串成一条可验证的升级链。

## 0. 结论先行

1. ACB（bridge）只写一份**升级请求文件**，不碰 `profile/current`、不碰
   `current.json`、不碰子进程、不调用 `acbctl` / `systemctl` / `nix`。
2. `acbl`（launcher + supervisor）是唯一执行者：校验请求、准备 generation、
   停旧子进程、提交指针、退出 `75`。
3. 外部重启方负责把 `acbl` 拉起来：systemd、`acbctl run --mode=launcher`、
   容器入口或 Windows 的等价父进程。重启后 `acbl` 读 `current.json` 启动新版本。
4. **`current.json` 是唯一执行真值，`profile/current` 是投影**。两者无法跨目录
   做一次原子 rename；用 journal + 启动对账把窗口封住。运行期版本选择不再依赖
   `profile/current`。
5. 新版本稳定 `stable_window_secs`（默认 60s）后 supervisor 把 LKG 提升到新版本；
   失败则回滚到旧 LKG，再失败就 `78` 止损，不再自动拉。
6. 升级前必须有可用的 LKG；没有 LKG 时默认拒绝自动升级，除非显式允许
   “无回滚升级”。
7. 升级、回退、自愈都写 append-only 事件：绑定 issue 的进 issue 时间线，无主的进
   系统流；不可逆动作先写 `intent`、后写 `result`，崩溃后由 journal/指针补录。
8. 本机现状（没有 `current.json` / `lkg.json`、`acbl` 未部署、`acbl` 只是
   `exec acb stack` 的 shim）意味着**整条链现在完全不通**。S1 是硬前置，不是
   可选的优化。

## 1. 角色、目录与责任边界

```text
<acb-home>/                              # 默认 $XDG_DATA_HOME/acb
  profile/
    generations/<id>/
      meta.json
      manifest.json                      # 仅 copy 策略
      root/                              # store: 软链；copy: 快照目录
    current -> generations/<id>/root     # Unix 投影；Windows 用 current.pointer
    current.pointer                      # Windows 指针文件，内容含 generation id
  state/supervisor/
    current.json                         # 执行真值；launcher 启动只看它
    lkg.json
    supervisor.log
    upgrade/
      requests/<request_id>.json         # ACB 写
      inflight/<request_id>.json         # acbl 认领后改名进来
      history/<request_id>.result.json   # acbl 写结果
      active.json                        # 当前唯一在途请求
      journal.json                       # 指针提交日志
      failures.json                      # 连续升级失败计数/冷却
      .lock                              # 单写者锁
  config/config.toml
  db/
```

| 角色 | 能写什么 | 不能写什么 |
|---|---|---|
| ACB / bridge | `upgrade/requests/`、自己的库和配置 | `profile/current`、`current.json`、`lkg.json`、`upgrade/inflight` |
| acbl / launcher | `profile/generations`、`profile/current`、`current.json`、`upgrade/*` | 不调用 `systemctl` / `nix` / `sudo` |
| supervisor（acbl 内） | `lkg.json`；回滚时通过同一个指针写入器写 `current.json` + 投影 | 不直接起 LKG 子进程（External 模式） |
| 外部重启方 | 只拉起 acbl | 不选版本、不写指针 |
| 控制台管理员 | 通过 ACB 产生请求 | 不直接改文件 |

`profile/current` 在升级路径里只是投影。`acbctl unit render`、脚本和人都可以读它，
但**运行期选版本以 `current.json` 为准**；两者不一致时以 `current.json` 为准并修复投影。

运行期的指针写入只有 acbl 一个写者。运维手工 `acbctl profile switch` 是离线路径：
acbl 停止时可以直接执行，下次启动按 `current.json` 对账；acbl 运行时必须复用同一个
activation writer（见 §4.5），不能只改软链。

## 2. 状态机

### 2.1 状态与写入者

| 状态 | 谁进入 | 写入 | 子进程 | 失败/退出行为 |
|---|---|---|---|---|
| `idle` | 无请求 | 无 | 正常运行 | 无 |
| `requested` | ACB 自己 | `requests/<id>.json`（原子写） | 继续运行 | 请求过期则 `expired` |
| `claimed` | acbl | 请求 rename 到 `inflight/`；`active.json` | 继续运行 | 认领失败则保留请求等下次 |
| `validated` | acbl | `active.json` 记目标、LKG、快照策略 | 继续运行 | 校验失败 → `rejected` |
| `staged` | acbl | `generations/.staging-<id>/` 写完 rename 为 `<id>` | 继续运行 | 复制/校验失败 → `aborted` |
| `prepared` | acbl | `journal.json`（from/to/phase） | 开始优雅停旧进程 | 停不下来 → `aborted`，恢复旧进程 |
| `committed` | acbl | `current.json` rename 提交；随后更新 `profile/current` | 旧进程已停 | 投影失败 → 补偿回旧指针并 abort |
| `restarting` | acbl | `active.json` 标 `restarting` | 无 | 退出 `75`，等外部重拉 |
| `observing` | 新 acbl | `active.json` 标 `observing` | 新版本运行 | 失败由 supervisor 判 rollback/fail |
| `stable` | acbl 观察 supervisor/LKG | `active.json` → `stable`，结果文件 | 新版本运行 | 成功终态 |
| `rolled_back` | supervisor + acbl | 指针写回 LKG；`active.json` → `rolled_back` | 旧版本（重启后） | 退出 `75`；请求标失败 |
| `failed`（终态） | supervisor | `active.json` → `failed`，失败计数 | 无 | 退出 `78`，不再自动拉起 |
| `rejected` / `expired` / `aborted` | acbl | 结果文件；`active.json` 清空 | 旧版本继续 | 不退出、不切版本 |

持久化边界：请求、结果、`active.json`、`journal.json`、`failures.json` 都在磁盘上；
supervisor 的连续失败计数只在内存，重启后归零。**跨升级的止损靠 `failures.json`，
不靠 supervisor 内存计数。** 每个状态转换都产生一条事件，事件名、产生者和落点见 §9。

### 2.2 一次成功升级的完整时序

```text
ACB       写 requests/req-....json  (原子 rename，无锁外泄)
acbl      轮询/事件发现 → rename 到 inflight/ → 写 active=claimed
acbl      校验：
            - schema / TTL / 请求者 / 文件属主
            - 目标 generation 存在且 payload 完整
            - 或 payload + strategy 合法（acbl 负责 add）
            - 当前有可用 LKG、未被冻结、未触发限速
acbl      需要时 stage generation（复制/软链 + manifest + 原子 rename）
acbl      写 journal{from,to,phase:prepared}
acbl      优雅停旧子进程（SIGTERM，最多 45s，超时 SIGKILL）
acbl      写 current.json.tmp → fsync → rename 为 current.json   ← 提交点
acbl      原子替换 profile/current（投影；失败则补偿回旧指针）
acbl      删除 journal；active=restarting；退出 75
外部      重拉 acbl
新 acbl   读 current.json → 启动新版本；active=observing
supervisor 健康探针每 5s；稳定窗口 60s 后 lkg=new
acbl      active=stable；结果写 history/<id>.result.json；请求成功
```

### 2.3 失败与崩溃恢复

| 崩溃/失败点 | 磁盘现场 | 恢复动作 |
|---|---|---|
| 写请求前 | 无请求 | 无 |
| 认领后、校验前 | `inflight` + `active=claimed` | 重新校验；过期则 `expired` |
| stage 中途 | `generations/.staging-*` 残留 | 删除 staging；请求 `aborted`；原版本不受影响 |
| 写完 journal、停进程前/中 | `journal=prepared`，`current.json` 仍旧 | 启动对账：恢复旧投影，请求 `aborted`，按旧 `current.json` 起旧版本 |
| `current.json` 已 rename、投影未更新 | `journal=prepared`，`current.json` 已新 | 启动对账：把投影补成新目标，视为已 `committed` |
| 投影更新后、退出 75 前 | `journal` 可能已删，`current.json` 新 | 重启读新版本；`active` 从 `restarting` 转 `observing` |
| 旧进程停不掉 | `current.json` 仍旧 | 尝试重启旧进程；升级 `aborted`；若旧进程也起不来则 `78` |
| 新版本启动即失败 | `current=new`，`lkg=old` | supervisor 走 rollback；写回 LKG，退出 75 |
| 回滚途中（停旧后、写指针前）崩溃 | `rollback_decided` intent + journal；`current=new`，`lkg=old` | 启动先按 intent 补完回退到 LKG，再起进程；发 `upgrade_rolled_back`（recovered） |
| 回滚版本再失败 | `current=lkg`，`lkg` 被清空 | supervisor `fail` → 退出 78，终态 |

对账规则写在 acbl 启动的最前面，在 `load_current()` 之前执行：

1. 有 journal 且 `current.json` 等于 `to` → 补投影，删除 journal。
2. 有 journal 且 `current.json` 等于 `from` → 恢复投影到 `from`，删除 journal，
   拒绝该请求。
3. 两者都不是或 JSON 损坏 → 不猜，退出 `78`，要求人工处理。
4. 没有 journal 且投影与 `current.json` 不一致 → 以 `current.json` 为准修复投影，
   记一条 `projection_repaired` 审计。

## 3. 发起通道：请求文件

### 3.1 位置与格式

默认目录：`<state_dir>/upgrade/`。`state_dir` 必须与 launcher 的
`supervisor.state_dir` 是同一个目录，避免出现两个 mailbox。

请求文件示例：

```json
{
  "schema": 1,
  "request_id": "req-1780000000000-a1b2c3d4",
  "idempotency_key": "console-upgrade-20261107-1",
  "created_at_ms": 1780000000000,
  "expires_at_ms": 1780000300000,
  "action": "upgrade",
  "target": {
    "kind": "generation",
    "id": "gen-1780000000000-ef567890"
  },
  "expected_current_id": "gen-1779990000000-11223344",
  "issue_id": "iss-1780000000000-55667788",
  "reason": "manual upgrade from console",
  "requested_by": {
    "actor": "console-admin",
    "source": "bridge-console",
    "token_fingerprint": "sha256:..."
  }
}
```

`target` 允许两种形态：

- `kind = "generation"`：generation 已存在，acbl 只校验并切换。
- `kind = "payload"`：带 `path` 和 `strategy`（`store` / `copy`），acbl 负责
  `add` 后再切换。`path` 必须落在配置允许的根目录内（例如 `/nix/store` 或
  `<acb-home>/incoming`），否则直接拒绝。

规则：

- 不出现 `program`、`args`、命令行、shell 字符串。请求只能表达“切到哪个已批准的产物”。
- `strategy` 必须显式；没写、写错、或当前平台不支持（例如非 Unix 用 `store`）
  直接报错，不降级到另一种策略。
- `expected_current_id` 是乐观并发控制：认领时若与当前 generation 不符，返回
  `stale`，不执行。
- `issue_id` 可选。带上后，这条升级链的全部事件都追加到该 issue 的时间线；不带则
  只进系统流。acbl 仍以请求里的 `request_id` 为关联键，不依赖 issue 服务在线。
- 请求文件最大 64 KiB；`request_id` 与 `idempotency_key` 都不得为空。

原子写：ACB 写 `requests/.tmp-<nonce>` → `fsync` → `rename` 为最终名；目录已存在，
文件权限 `0600`（同用户部署）或 `0660`（分组部署）。

### 3.2 认领、幂等、并发、过期

- **认领**：acbl 用 `rename` 把 `requests/<id>.json` 移到 `inflight/<id>.json`。
  同一文件只会被一个进程 rename 成功；这是文件系统级的单消费者保证。
- **单飞**：`upgrade/.lock`（`flock`）+ `active.json`。同一时刻只允许一个升级。
- **重复请求**：同 `request_id` 或同 `idempotency_key`：
  - 还在 `inflight`：返回 `duplicate_in_progress`，不重复执行。
  - 已在 `history`：直接返回已有结果，字节不变。
- **并发请求**：第二份请求不排队，追加一条 `history/<new-id>.result.json`，
  `status = "busy"`，`blocked_by = <active-id>`。调用方稍后重试。这样不会出现
  “排队的旧意图在新版本上突然生效”。
- **过期**：超过 `expires_at_ms` 的请求在认领阶段直接标 `expired`，不切版本。
  默认 TTL 5 分钟，可配置。
- **崩溃恢复**：`inflight` + `active.json` 是恢复依据；acbl 启动先跑 §2.3 的
  对账，再决定继续观察、拒绝还是完成。请求文件本身不依赖内存状态。

### 3.3 为什么不用 socket

- ACB 与 acbl 生命周期不同：ACB 可能在 acbl 未运行时产生意图。socket 要求服务端
  在线，文件不要求。
- 文件天然持久、可审计、可幂等；崩溃后还能看到“上次想升到哪”。
- 文件权限已经能表达“谁可以发起”，不需要额外做 peer credential 握手；跨平台
  （Windows 没有 Unix socket 的那套惯例）也简单。
- ACB 只做普通文件写入，不获得任何执行能力；socket 容易演化成“桥可以直接命令
  launcher 做事”的通道。
- 代价是延迟（轮询 100ms～1s）和清理逻辑；升级不是毫秒级操作，这个代价可接受。

将来换通道要保留的抽象：

```text
UpgradeChannel
  submit(request) -> request_id / result
  status(request_id) -> state
  cancel(request_id) -> bool
```

文件通道只是其中一个实现。若以后加 Unix socket / HTTP broker / QQ 管理端，
请求和结果 JSON 结构不变，broker 负责把消息落成同一个 mailbox；acbl 不感知传输层。

## 4. 切换的原子性

### 4.1 为什么不能真的“一次原子操作”

`profile/current` 在 `profile/` 下，`current.json` 在 `state/supervisor/` 下，
跨目录、跨文件，普通文件系统没有跨文件事务。把两者做成一个目录再 rename 也不可行：
它们是不同形状（软链 vs JSON），而且 `current.json` 还要被 supervisor 回滚时单独改写。

因此采用：

- **单写者**：只有 acbl 写这两个文件；ACB 和外部工具不写。
- **单一提交点**：`current.json` 的 rename 是提交点。
- **投影后置**：`profile/current` 在提交后更新；失败时补偿回旧指针。
- **journal + 启动对账**：崩溃窗口由 acbl 启动时修复，见 §2.3。
- **读侧契约**：需要“精确当前版本”的工具读 `current.json`；读 `profile/current`
  是看投影，可能短暂落后。

### 4.2 提交算法

```text
acquire upgrade/.lock
write journal{from, to, phase="prepared"}     # fsync
stop child (graceful; timeout -> SIGKILL)      # 失败则 abort，指针不动
write current.json.tmp                          # fsync
rename current.json.tmp -> current.json         # ← 唯一提交点
replace profile/current (temp symlink + rename)
delete journal                                   # 或 phase="committed"
release lock
exit 75
```

- 停旧进程在提交之前：提交后不会出现“旧进程还活着、指针已经切走”的状态。
- 提交后投影失败：用 journal 的 `from` 补偿写回 `current.json`，恢复旧投影，
  重启旧子进程，请求 `aborted`；补偿也失败则 `78`，等人处理。
- 提交后、退出前崩溃：外部重启读新 `current.json`，直接进入 `observing`。

### 4.3 两种快照策略

| 项目 | `store` | `copy` |
|---|---|---|
| `generations/<id>/root` | 指向 `/nix/store/...` 的软链 | 真实快照目录 |
| 依赖 | 必须有 nix store、Unix 软链 | 只依赖普通文件系统 |
| 切换成本 | 改两条指针，几乎零成本 | 同样只改指针；成本在 `add` 时复制 |
| 回退可靠性 | store 不可变，源路径仍在即可 | 不依赖源目录；manifest 校验快照 |
| 校验 | 目标 store path 存在 | 逐文件 size/sha256 + symlink target |
| 不支持的场景 | payload 不在 `/nix/store`、Windows 无软链 | 文件系统不支持符号链接、复制失败 |
| 失败处理 | 明确报错，不自动改成 copy | 明确报错，不自动改成 store |

共同点：

- `add` 先写 `generations/.staging-<id>/`，写完 `meta.json` / `manifest.json` 后
  整体 rename 为 `generations/<id>`；不允许 `current` 指向半成品。
- launcher 只消费 `meta.payload_path()` 给出的 `root` 路径，不感知策略。
- 策略写在 `meta.json` 的 `payload.strategy`；旧 generation 没有该字段时按
  兼容软链处理，但**新请求必须显式声明**。

### 4.4 旧进程仍在运行时

升级执行顺序是“先停旧、再提交”，所以不存在“旧进程从已被切走的代运行”的窗口：

- 停旧之前：`current.json`、`profile/current`、旧 generation 全部不动。
- 优雅停机最多 45s；期间旧进程继续服务，升级请求处于 `prepared`。
- 超时仍不退：`SIGKILL`；还失败则 abort，指针不提交，尝试重启旧版本。
- `gc` 必须排除：`current`、`LKG`、`inflight` 目标、正在运行的 generation。
  删除 generation 不能在升级/回滚过程中并发执行。

### 4.5 离线 `profile switch` 的兼容

现有 `profile switch` 只改 `profile/current`，不写 `current.json`，这正是“切了版本、
launcher 重拉仍跑旧代”的原因。设计上不保留这个漂移：

- acbl 运行中：`profile switch` 必须调用 activation writer，和升级走同一套提交 +
  journal；不能只做软链 rename。
- acbl 已停止：允许直接改投影作为离线准备；但下次 acbl 启动时以 `current.json`
  为准。如果操作者想让离线切换生效，必须同时写 `current.json`（或使用
  `profile activate`），否则启动后会被对账修回旧版本。
- `acbctl profile current` 在运行期应优先读 `current.json`；发现投影不一致时打印
  `drift` 警告，而不是把投影当成运行版本。

## 5. 健康确认、LKG 与止损

### 5.1 观察期

沿用现有 supervisor 参数（默认值）：

| 参数 | 默认 | 作用 |
|---|---|---|
| `probe_interval_secs` | 5 | 健康端点采样间隔 |
| `startup_grace_secs` | 30 | 启动宽限，避免刚起就判死 |
| `unhealthy_secs` / `unhealthy_probes` | 60 / 3 | 持续不健康判定 |
| `stable_window_secs` | 60 | 健康满窗口后提升 LKG |
| `consecutive_failures` | 3 | 连续失败阈值 |
| `window_failures` / `window_secs` | 5 / 600 | 窗口失败阈值 |
| `restart_backoff_secs` | 1,2,4,8,30 | `hold` 重启阶梯 |

成功判定：

- 新进程启动并通过健康探针，健康时间累计满 `stable_window_secs`；
- supervisor 把 `lkg.json` 提升到新 generation；
- acbl 把 `active.json` 标 `stable`，写成功结果。

若配置 `lkg_update = "never"`（canary），LKG 不提升；此时用同样的稳定计时判定
“升级成功”，但请求结果要注明 `lkg_not_promoted`。

### 5.2 失败判定

按现有策略优先级：panic 类 → 启动宽限 → 健康不健康 → 连续失败 → 窗口累计 →
`hold`。不新增第二套判定。

- 有 LKG：策略产出 `rollback`。
- 无 LKG：策略产出 `fail`。
- `rollback = "none"`：`rollback` 降级为 `fail`，退出 78。
- 回滚后 `lkg == current`，launcher 把内存 LKG 清空；这正是“只允许一次自动回滚”的
  现有语义，不要改。

### 5.3 回滚时序

```text
supervisor 判定 rollback（有 LKG）
  → 停子进程（External 模式已有实现）
  → 通过同一个指针写入器写 current.json = LKG + 更新 profile/current
  → current.json 写入失败：退出 78（避免无限回滚）
  → 写入成功：内存 current = LKG，lkg = None，退出 75
外部重启方拉起 acbl
新 acbl 读 current.json = LKG，启动旧版本
旧版本再失败 → 没有 LKG → fail → 退出 78 → 外部不再拉
```

关键点：回滚也必须走 §4.2 的指针写入器，不能只 `save_current` 而不更新
`profile/current`，否则 `profile/current` 又和运行版本漂移。

### 5.4 连续失败与止损

分两层：

1. **进程层（supervisor 内）**：`hold` 退避 1/2/4/8/30s；连续 3 次或窗口 5 次失败
   后 rollback；panic 立即 rollback。回滚后的版本再失败直接 `78`。
2. **升级层（acbl 持久化）**：`upgrade/failures.json`
   ```json
   {
     "consecutive": 2,
     "last_failure_ms": 1780000000000,
     "cooldown_until_ms": 1780000600000,
     "frozen": false
   }
   ```
   - 连续失败冷却阶梯：30s、2m、10m、1h、6h。
   - 连续 3 次升级失败后 `frozen=true`：拒绝新请求，直到人工执行
     `acbctl upgrade reset`（或控制台显式解除）。
   - 触发 `78` 的失败直接冻结，不再自动尝试。
   - 成功升级把 `consecutive` 清零。
   - 同一时间只允许一个升级；默认 1 小时内最多 3 次请求，防止变成重启风暴。

## 6. 与现有后端、75/78 的对应

| 后端 | 谁重拉 acbl | 75 | 78 | 备注 |
|---|---|---|---|---|
| systemd | `acb-launcher.service` | `RestartForceExitStatus=75` 重拉 | `RestartPreventExitStatus=78` 停 failed | 现有 unit 可用；`acbl` 必须指向 launcher，不能是 shim |
| `acbctl run --mode=launcher` | 父循环 | 现有循环读到 75，重读 `current.json` 重拉 | 现有循环退出 78 | 需要补“launcher 异常崩溃也重拉”的窗口限流 |
| 容器 | 容器内 `acbctl run` 或入口包装器 | runtime 会把 75 当普通失败 | runtime 可能也重拉 78 | **不要**让 runtime 直接区分 75/78；入口必须消化 78 |
| Windows / 无 systemd | `acbctl run --mode=launcher` 或服务包装器 | 父循环重拉 | 父循环退出 | 没有 watchdog；用 supervisor 健康探针代替 |
| `acbctl run --mode=direct` | 无 | 无意义 | 无意义 | **必须拒绝升级请求**，因为没有重拉方 |

### 6.1 启动能力声明

acbl 启动时写 `state/supervisor/capabilities.json`：

```json
{
  "restart_owner": "systemd|acbctl-run|container|windows-wrapper|none",
  "symlink_supported": true,
  "strategies": ["store", "copy"],
  "watchdog": true
}
```

ACB 只有在 `restart_owner != "none"` 时才展示/接受升级入口；acbl 只接受请求，
不假设外部一定会重拉。没有重拉方却切了指针，会变成“进程退出后没人拉”，必须提前拒绝。

### 6.2 没有 systemd 时

- `acbctl run --mode=launcher` 就是重启方。它已经能识别 75/78；升级链只是让
  launcher 在“切换成功后”以 75 退出。
- 容器里推荐把 `acbctl run` 或等价包装器作为 entrypoint，而不是裸 bridge；
  这样 78 可以在容器内变成“正常停止”，避免编排层无限重启。
- Windows 没有 systemd，也没有 Unix 信号：`acbl` 用 `supervisor.shutdown()` 停子进程，
  父进程按退出码重拉；watchdog 不可用，健康探针接管。

### 6.3 Windows 上的软链

- `store` 策略依赖 Unix 软链，Windows 上**明确拒绝**，报
  `store strategy requires Unix symlinks`，不静默降级。
- 升级链在 Windows 上要求 `copy` 策略：generation 是真实目录快照。
- `profile/current` 在 Windows 上不是软链，而是 `profile/current.pointer`
  （JSON 文件，含 generation id 和 payload 路径）。读取统一走
  `profile::resolve_current()`：优先 `current` 软链，不存在则读 `current.pointer`。
- 运行期 launcher 仍然只读 `state/supervisor/current.json`，不依赖 pointer 文件。
- copy 快照里如果遇到 Windows 不支持的符号链接，`add` 直接失败并报具体路径；
  不自动改成复制目标内容。

## 7. 安全与权限

### 7.1 谁能发起

- **默认只有控制台管理员**：`POST /api/upgrade` 复用现有控制台 token 鉴权；
  请求里只记 token 的指纹/actor，不记原始 token。
- 可选本地 CLI：`acbctl upgrade request` 直接写同一个 mailbox，属于本机同用户操作。
- **聊天平台消息默认不能触发升级**；若以后要开，必须单独 grant + 二次确认，
  不沿用普通消息权限。
- 任何发起者在 acbl 侧都没有特权：acbl 重新校验所有字段。

### 7.2 请求文件防伪造

- **同用户部署（默认）**：请求目录 `0700`，文件 `0600`，acbl 检查
  `symlink_metadata`：必须是普通文件、不是软链、owner 与 acbl 运行用户一致、
  其他用户无写权限。同 uid 的进程本来就能写 `profile/current`，文件权限不是提权
  边界，只是“只有 ACB 这个入口会写”的约定。
- **分组部署**：`upgrade/requests/` 为 `0770`，组只含 bridge 和 acbl；文件 `0660`。
  可再加 HMAC：
  - `upgrade.hmac_key_file` 权限 `0600`，只有 acbl 和 bridge 能读；
  - 请求带 `signature = HMAC(key, canonical_json_without_signature)`；
  - acbl 校验失败即拒绝，并记审计。
  - 这不能防“同 uid 的被攻陷 bridge”，但能防跨用户/跨组伪造。
- 内容校验：schema、TTL、目标存在、payload 根目录白名单、strategy 与平台匹配、
  `expected_current_id`、请求大小、字段白名单（拒绝 `command` / `args` / `env`
  这类执行字段）。
- 文件锁 + 单飞 + 限速 + 冻结，防重复/并发/风暴。

### 7.3 权限要求

- **桥不提权**：不调用 `sudo` / `acbctl` / `systemctl` / `nix`，不需要 setuid、
  capability、root。只需要：
  - 写 `upgrade/requests/`；
  - 写 `events/producers/bridge/`（只写自己的事件段，不能改别的 producer 段）；
  - 读 `upgrade/history/`、`capabilities.json` 和事件流用于展示状态。
- **acbl 不提权**：只读写 `<acb-home>` 内的 profile/state/config/db；以普通用户
  运行；systemd 用 user unit 或专用 `acb` 用户。
- 事件投递器（ACB 内的 relay，或独立 worker）读全部 producer 段、只写
  `events/delivery/`；它需要 issue 服务的受限 token，但 acbl 不需要。
- 建议（非强制）：bridge 和 acbl 分不同 uid + 共享 `acb-upgrade` 组，给未来
  多用户部署留边界；同 uid 部署也能工作，但安全边界只能靠代码约定。
- 审计：`requests`、`history`、`supervisor.log`、`state/events/` 都保留；日志不打印原始 token。

## 8. 落地拆分

每一步都能独立验证；前一步不完成，后一步不接。

| 步骤 | 内容 | 现状 | 验证 |
|---|---|---|---|
| **S1** 启动入口与能力声明 | `acbl` 变成真正的 launcher（或 systemd 直接启 `acb-launcher`）；`acbctl run` 写 `restart_owner`；写 `capabilities.json` | **现在完全不通**：`acbl` 是 `exec acb stack` shim，没有进程消费请求 | 三种方式都能启动；75/78 仍按现有语义；无重拉方时 `restart_owner=none` |
| **S2** 请求 mailbox + 只读校验 | ACB `POST /api/upgrade` 写请求；acbl 轮询、认领、校验、写结果，但**不切版本** | 控制台没有任何升级入口 | 重复/并发/过期/伪造/父子目录校验；`current.json` 不变；结果可查 |
| **S3** 原子指针写入器 | `current.json` 提交 + `profile/current` 投影 + journal + 启动对账；supervisor 回滚也改用它 | `profile switch` 只改软链，不同步 `current.json` | 在提交前后 `kill -9` 注入，启动后对账；`current.json` 与投影一致；回滚不再漂移 |
| **S4** 切换执行 + 75 重拉 | acbl 停旧、提交、退出 75；systemd / `acbctl run` 重拉；请求结果 | **现在完全不通**：无自升级路径 | systemd 场景和 `acbctl run` 场景各跑一次真实升级；direct 模式拒绝 |
| **S5** 观察、LKG、回滚闭环 | `observing` 状态、成功提升、失败回滚、78 止损、请求历史 | supervisor 已有策略，但从未和升级请求串起来；本机无 `lkg.json` | 强制新版本崩溃/健康失败；确认回滚到 LKG；确认第二次失败 78；确认 GC 保护 LKG |
| **S6** 升级层退避与冻结 | `failures.json`、冷却阶梯、3 次冻结、限速 | 无 | 连续提交坏版本，确认冷却与冻结；人工 reset 后可恢复 |
| **S7** Windows / copy / 指针模式 | `current.pointer`、copy-only、无 systemd 的父进程重拉 | 软链和 store 在 Windows 不可用 | Windows 上 copy 升级全链路；store 明确报错；无 symlink 依赖 |
| **S8** 运维闭环 | `acbctl upgrade status/cancel/reset`、手工恢复文档、审计读取 | 无 | 操作员能看清 `active`/`journal`/`history`；异常后按文档恢复 |
| **S9** 事件流水骨架 | 事件 envelope、producer 分目录、intent/result 写前日志、启动补录、滚动保留 | 无；现有 `supervisor.log` 只有散落审计 | 每个关键动作有事件；在切换/回滚各点 `kill -9` 后重启，事件能补录；保留策略可验证 |
| **S10** issue 时间线投放 | issue-service 受限事件摄入接口、ACB relay、幂等和积压补投 | 现有 `issue_events` 只能由领域动作写入，没有通用实现 | 带 `issue_id` 的升级事件出现在 issue 时间线；重复投递不重复；ACB 停机期间不丢 |

**最大改动面：S3。** 它同时碰 `profile`（current 的写法）、`supervisor`（回滚不再
直接 `save_current`）、`launcher`（启动先对账）、`acbctl run`/`status`（读侧和状态输出）。
S9/S10 是第二大的横切面（launcher、supervisor、activation writer、issue-service 都要接），
建议与 S2/S3 同步做，不要等 S4 之后再补。S1 虽然不是最大改动，但它是**阻塞项**：
`acbl` 不变成 launcher，S2/S4 都没有执行者。

## 9. 事件流水：升级、回退、自愈都入账

### 9.1 三种本地事实，别混在一起

- `current.json` / `lkg.json` / `journal.json`：**状态**。回答“现在跑谁、回退到谁”。
- `supervisor.log`：**原始本地审计**。保留，用于排障，不承担跨进程投递。
- `state/events/`：**append-only 事件流水**。回答“谁在什么时候做了什么、结论是什么、
  为什么”。issue 时间线和系统流都从这里出。

目录：

```text
state/events/
  producers/
    bridge/
      system/<YYYYMMDD>/<at_ms>-<event_id>.json
      issue/<YYYYMMDD>/<at_ms>-<event_id>.json
    launcher/
      system/<YYYYMMDD>/...
      issue/<YYYYMMDD>/...
    supervisor/
      system/<YYYYMMDD>/...
      issue/<YYYYMMDD>/...
    adapter/
      system/<YYYYMMDD>/...
  delivery/<event_id>.json     # issue 投放状态，relay 专用
  emergency.jsonl              # 流水写失败时的降级通道
```

- 写事件时按 `correlation.issue_id` 直接分到 `issue/` 或 `system/` 子目录；落点在一开始
  就确定，不靠事后路由。
- 一条事件一个不可变文件；任何 producer 都不能改别人的段。bridge 只写
  `producers/bridge/`，acbl 写 launcher/supervisor/adapter。
- 写入：临时文件 → `fsync` 文件 → `rename` 到最终名 → `fsync` 目录。绝不覆盖已有事件。
- 文件名用事件时间 `at_ms` 排序；`event_id` 是 ULID，重复投递靠它幂等。

统一 envelope：

```json
{
  "schema": 1,
  "event_id": "evt-01J8...",
  "at_ms": 1780000000123,
  "recorded_at_ms": 1780000000456,
  "producer": "launcher",
  "kind": "upgrade_switched",
  "phase": "result",
  "action_id": "act-1780000000123-01",
  "outcome": "succeeded",
  "reason_code": "pointer_committed",
  "reason": "current.json committed, profile/current projected",
  "correlation": {
    "request_id": "req-...",
    "generation_id": "gen-...",
    "from_generation_id": "gen-...",
    "issue_id": "iss-...",
    "attempt": 1
  },
  "payload": {
    "strategy": "store",
    "exit_code": null
  },
  "recovered": false
}
```

- `producer` 固定为 `launcher` / `supervisor` / `bridge` / `adapter`。
- `outcome` 是结论：`info` / `succeeded` / `failed` / `rejected` / `rolled_back` /
  `abandoned`；`reason_code` 是机器可判定的原因，`reason` 给人看。
- `correlation.request_id` 必填于升级链事件；`generation_id` 尽量填；`issue_id` 只
  在请求带了 issue 时出现。
- `phase` 为 `intent` / `result` / `single`；`recovered=true` 表示事件是崩溃后补录的，
  `at_ms` 用原动作时间，`recorded_at_ms` 用补录时间。

### 9.2 关键动作与事件

| `kind` | producer | 触发 / 结论 | 落点 |
|---|---|---|---|
| `upgrade_requested` | bridge | 请求文件已提交；`outcome=info` | issue / system |
| `upgrade_rejected` | bridge / launcher | 鉴权、TTL、busy、stale、目标非法；带 `reason_code` | 原请求的 issue / system |
| `upgrade_claimed` | launcher | 请求 rename 到 `inflight/` | issue / system |
| `upgrade_staged` | launcher | generation 已 add；带 strategy、payload 路径 | issue / system |
| `upgrade_switch_intent` | launcher | 即将停旧并提交指针 | issue / system |
| `upgrade_switched` | launcher | `current.json` 已提交、投影已更新 | issue / system |
| `upgrade_switch_aborted` | launcher | 停旧/提交失败，指针未切或已补偿 | issue / system |
| `upgrade_restart_requested` | launcher | 即将退出 75；带 `exit_code=75` | issue / system |
| `launcher_started` | launcher | 重启后启动；带 `restart_reason=exit75/systemd/adapter` | issue / system |
| `upgrade_observing` | supervisor | 新版本已启动并进入观察 | issue / system |
| `upgrade_stable` | launcher / supervisor | 稳定窗口通过；带健康时长和 LKG 结论 | issue / system |
| `lkg_promoted` | supervisor | **复用现有事件名**；LKG 提升到新 generation | issue / system |
| `upgrade_rollback_decided` | supervisor | 判定回退；带失败类别、目标 LKG | issue / system |
| `upgrade_rolled_back` | supervisor / launcher | 指针已写回 LKG，回退落定 | issue / system |
| `upgrade_abandoned_78` | launcher / supervisor | 放弃自动恢复，退出 78 | issue / system |
| `adapter_respawn_scheduled` / `adapter_gave_up` | adapter | 重启方决定重拉 / 放弃 | system |
| `journal_recovered` / `projection_repaired` / `lkg_seeded` / `staging_removed` | launcher / supervisor | 启动自愈动作 | issue（有在途请求时）/ system |

systemd、容器 runtime 本身不写 ACB 文件，因此不作为事件 producer；它们的重拉事实由
新 launcher 的 `launcher_started` 带 `restart_reason` 记录。只有 ACB 自有的重启方
（`acbctl run` 父循环、Windows 包装器）直接写 `adapter_*` 事件。

噪声事件不进流水：`probe`、`decision`、`failure`、`start` 继续只写
`supervisor.log`；只有当它们导致回退、放弃或自愈时，才额外产出一条上述关键事件。

### 9.3 两类落点与现有审计的关系

- **有 `correlation.issue_id`**：写入 `producers/*/issue/`，由 relay 投递到 issue 时间线。
  复用 `issue_events` 表，不建新表：
  - `kind = "acb_" + kind`（例如 `acb_upgrade_rolled_back`），`actor = producer`，
    `summary = reason`，`payload` 放完整 envelope，`idempotency_key = event_id`。
  - 需要给 issue-service 增加一个窄的“操作事件摄入”接口（service token + issue
    scope）；现有表结构、`events_for_issue` 读路径和 outbox 投递都复用。
  - 不使用 `issue.comment` 伪装升级事件，也不改 issue 状态；升级事件是时间线上的
    操作记录，不是评论或状态迁移。
- **无 `issue_id`**：写入 `producers/*/system/`，即系统流。系统流不是一个另存的文件，
  而是这些 `system/` 天目录的只读合并视图（按 `at_ms` + `event_id` 排序）；启动自愈
  和被动回滚默认落这里。
- `issue_id` 非法或 issue 不存在：不丢事件，追加一条 `issue_delivery_failed` 系统
  事件，并保持原事件在 producer 段等人工处理。
- 与 `lkg_promoted` 的关系：**复用事件名，扩展字段，不另造语义**。`maybe_promote`
  在 `save_lkg` 前后分别写 intent/result；原有 `supervisor.log` 的那行继续保留，
  两者用同一个 `at_ms` 和同一个 `generation_id`。如果崩溃后只看到 LKG 已提升、
  没有 result，启动时补一条 `recovered=true` 的 `lkg_promoted`。supervisor 从
  `active.json` 读取当前请求的 `request_id` / `issue_id` 并注入事件；没有在途请求
  就是无主事件，落系统流。
- `supervisor.log` 不被事件流替代；它保留 `probe/decision/failure/lkg_write_failed`
  等原始审计，适合本机排障。事件流只收“状态改变或需要对外可读”的关键动作。

### 9.4 写入时机与崩溃窗口

原则：**不可逆动作先写 `intent`，动作完成再写 `result`；事件落盘失败就不做动作。**
回退是例外：回退必须执行，事件写失败时降级到 `emergency.jsonl` 和 stderr，启动补录。

| 动作 | intent 写在 | result 写在 |
|---|---|---|
| 切换 | 停旧进程之前 | `current.json` 提交 + 投影更新之后 |
| 回退 | 停旧进程之前，intent 里带 LKG 目标 | 指针写回 LKG 之后 |
| LKG 提升 | `save_lkg` 之前 | `save_lkg` 成功之后 |
| 退出 78 | 调用 exit 之前 | 由 adapter 或下次启动补 `launcher_started` |

崩溃窗口：

| 被杀时机 | 保证 |
|---|---|
| intent 之前 | 动作还没开始；恢复时发 `upgrade_switch_aborted`，请求标失败 |
| intent 之后、动作之前 | intent 文件在盘；启动补一条 `..._not_applied`，不误报成功 |
| 动作之后、result 之前 | 启动读 `journal.json` / `current.json` / `lkg.json`，补发 result，`recovered=true` |
| 回退途中（已停旧、未写指针） | `upgrade_rollback_decided` intent 带 LKG 目标；启动先补完回退，再起进程，补发 `upgrade_rolled_back` |
| result 已写、issue 未投递 | `delivery` 未完成；relay 重试，`event_id` 幂等 |
| 写事件时磁盘满/只读 | 升级：不执行不可逆动作；回退：写 emergency 后继续，启动补录并告警 |
| 写临时文件时被杀 | 临时文件被忽略/清理；没有半条事件 |

回退过程中进程被杀时，**至少已有一条 fsync 过的 `upgrade_rollback_decided`**；
启动恢复用它把指针补到 LKG，再发结果事件。事件不会因为进程死亡而消失。

### 9.5 保留策略

- producer 段按天滚动（`YYYYMMDD`）；当前天永不删；只删完整的旧目录，不改写活动文件。
- 系统流默认：90 天 / 50,000 条 / 64 MB，先到先限；从最旧的整天 `system/` 目录开始删。
- issue 待投递事件：**未投递永不删**。某个 `issue/<YYYYMMDD>/` 目录里还有未投递事件时，
  整个目录 pin 住；全部投递成功后，按 30 天 / 10,000 条清理本地副本。issue 时间线本身
  按 issue 生命周期保留，不在升级链里删。
- `supervisor.log`：8 MB × 5 个滚动文件（或 30 天）；它只是原始审计。
- `emergency.jsonl`：不自动清理；启动 ingest 后按需归档。
- 若保留上限已到但仍有未投递 issue 事件：保留 pin，发告警，并冻结新的自动升级，
  避免一边丢事件一边继续升级。

## 10. 需要确认的取舍

1. **并发语义**：本文默认“拒绝 busy，不排队”。若要排队，需要给请求加优先级和
   “过期即作废”，否则旧意图会在新版本上突然生效。
2. **target 形态**：默认同时接受“已存在 generation”和“payload + strategy（acbl add）”。
   若只想让 ACB 切已构建好的版本，可以砍掉 payload 形态，S3/S4 更小。
3. **升级前是否强制有 LKG**：本文默认强制。没有 LKG 时只允许显式
   `--allow-no-lkg`，失败即 `78`。本机现在没有 `lkg.json`，首次部署必须先写初代 LKG。
4. **同 uid 还是分组部署**：默认同 uid，文件协议即可；若要求防跨用户伪造，
   走 S2 的 HMAC/分组方案。

## 11. 当前最小实现（只版本化 bridge）

已落地、且只覆盖“bridge 换代”这一条链：

- **请求信箱**（`crates/supervisor/src/upgrade.rs`）
  - 根目录 `<state_dir>/upgrade/`：`requests/`、`inflight/`、`history/`、`active.json`、
    `capabilities.json`。
  - 请求 schema = 1，字段：`request_id`、`idempotency_key`、`created_at_ms`、
    `expires_at_ms`、`action`、`target`（`generation` 或 `payload + strategy`）、
    `expected_current_id`、`issue_id`、`reason`、`requested_by`。
  - 去重：同 `request_id`、同 `idempotency_key`、同 target 任一命中都返回已有请求；
    history 里的请求也参与去重。
  - 忙则拒绝：`active.json` 处于非终态时新请求返回 `busy`，不排队。
  - 无重拉方拒绝：`capabilities.json` 的 `restart_owner` 必须是
    `systemd | acbctl-run | container | windows-wrapper`，且 `launcher_pid` 仍存活；
    否则 `rejected reason_code=no_restart_owner`。
  - 原子写：临时文件 + `fsync` + rename + 目录 sync。
- **`acbctl upgrade`**（`crates/acb/src/upgrade.rs`）
  - 只写请求，不切代：`--generation ID` 或 `--payload PATH --strategy store|copy`。
  - 输出统一为 `queued | duplicate | busy | rejected`，并带 request id 与路径。
  - `acbctl upgrade status [REQUEST_ID]` 读取 `active.json` / history 结果。
- **acbl 认领**（`crates/launcher/src/main.rs`）
  - 启动写 `capabilities.json`；有 `restarting` 的 active 时恢复为 `observing`。
  - 认领 → 校验（schema/target/`expected_current_id`/重拉方）→ stage
    （`payload` 调既有 `acbctl profile add`；已有 generation 只做存在性校验）→
    停旧 → 提交 → 退出 `75`。
  - `generation` 元数据以 `current.json` 为准，避免重启后 LKG/回滚记错代。
  - 稳定窗口后 LKG 提升为目标代 → 结果 `stable`；回滚 → `rolled_back`；
    `78` → `failed`。
- **提交点**：复用另一路已落地的 `crates/supervisor/src/pointer.rs`
  `ActivationWriter`（锁 + journal + `current.json` + `profile/current` 投影一次性写）。
  launcher 不再自己拼 symlink/指针写入。
- **退出码**：提交后 `75` 请求外部重拉；回滚本身也是 `75`（重拉后跑 LKG）；
  回滚后的 LKG 再失败才 `78`。升级请求在回滚时立即终态化（`rolled_back`），不会自动重试。
- **unit 模板**：新增 `acb-launcher.service`（系统级，`multi-user.target`）与
  `acb-launcher-user.service`（用户级，`default.target`）。新增白名单键
  `ACB_UNIT_LAUNCHER_BIN`、`ACB_UNIT_STATE_DIR`、`ACB_UNIT_SUPERVISOR_CONFIG_FILE`；
  默认值从 `ACB_UNIT_PROFILE_DIR` / `ACB_UNIT_BIN_DIR` 派生，不写死 NixOS 路径，
  也不引用 `/run/current-system` 或 `ACB_UNIT_SHELL`。

### 尚未接通

- `profile.rs` 仍是另一路的改动区；本次未改。stage 仍通过 `acbctl profile add`
  子进程完成，提交走 supervisor 的 `ActivationWriter`。
- 启动对账只做“请求 active 状态恢复”，没有做 `current.json` 与投影的全量对账
  （由 `pointer` 的 journal 负责启动时 reconcile）。
- 事件流水（§9）未接入；当前只有 `supervisor.log` 的原始审计和升级请求结果文件。
- 回滚到 LKG 后如果保护性停止，需要外部再拉起一次以运行 LKG；`78` 仍是
  “不再拉起”的终态，不是回滚动作本身。

## 12. 期望 `profile.rs` 后续提供的接口（本次未改）

为了彻底消除“stage 用子进程 + commit 用 supervisor”两套入口，`profile.rs` 最好导出：

```
activate_generation(profile_root: &Path, state_dir: &Path, generation_id: &str)
    -> Result<CommandSpec, String>
```

语义：

- 校验 generation 存在、payload `root/bin` 可执行；
- 通过 `ActivationWriter`（或它导出的单写者）一次性写 `current.json` 与投影；
- 返回最终 `CommandSpec`（program/args/generation），供 launcher 重启后直接启动；
- 失败时不留下半写状态，返回可判定的原因码。

`add` 也可以提供库函数（`add_payload(profile_root, path, strategy) -> GenerationRef`），
让 launcher 不再 shell out。当前 launcher 的调用点集中在
`maybe_handle_upgrade` / `add_generation` / `spec_for_generation`，替换时只动这三处。

