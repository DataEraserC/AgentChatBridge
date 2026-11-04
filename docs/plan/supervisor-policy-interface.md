# Supervisor 决策接口（已定）

状态：**已定（2026-11-04）**。本文定义接口与策略边界；实现尚未开始。
相关决策：[supervision-packaging-issues.md](supervision-packaging-issues.md) 第 1、4 节。

## 0. 目标

把「回滚判据」「WASM/外部策略插件」「内置策略」统一到一层：

> **supervisor 采集状态快照 → 调用一个策略 → 得到一份决策 → supervisor 执行。**

策略不直接杀进程、不切版本、不碰 store path、不探测远端；它只回答
`fail / hold / rollback + reason (+ delay_ms)`。同一快照必须给出确定结果。

## 1. 调用点

| 触发 | 时机 | 当时能拿到什么 | 决策路径 |
|---|---|---|---|
| `startup_initial` | supervisor 拉起进程后、首次探测前 | 进程状态、启动时间、版本、LKG；健康序列为空 | **内置策略（即时、同步）**，不调用外部载体 |
| `startup_grace_expired` | 启动宽限期到点时一次 | 上述 + 宽限期状态 + 已发生的失败次数 | **内置策略（即时、同步）**，不调用外部载体 |
| `health_probe` | 每个健康探测间隔 | 完整快照：探测序列、失败计数、活跃轮/队列、版本 | 统一接口；外部/WASM 限时 1000ms |
| `process_exit` | 进程退出被 wait 到时立即 | 退出码/信号、运行时长、最近失败、版本/LKG；运行时指标可能缺失 | **内置策略（即时、同步）**，不调用外部载体 |
| `reload_failed` | 配置热重载或插件重载失败后 | 重载错误类别/详情、插件错误计数、版本、运行时指标 | 统一接口；外部/WASM 限时 1000ms |
| `manual_evaluate` | 管理员要求重新评估 | 完整快照 + 操作者信息 | 统一接口；走策略，不绕过 |
| `manual_force` | 管理员明确要求某个动作 | 完整快照 + 请求动作 | **绕过策略**，记录 `operator_override` |
| `stable_window` | 进程持续健康达到稳定窗口 | 完整快照 + 当前/LKG 版本 | supervisor 内部/内置；不调用外部，用于 LKG 提升 |

**原则：故障路径上不许有等待；判断可以慢，恢复不能慢。**

- **即时触发**（`startup_initial`、`startup_grace_expired`、`process_exit`）：只走内置策略，
  同步执行，不调用外部进程/WASM/`.so`，避免在故障路径上引入最多 1s 等待。
- **周期触发**（`health_probe`、`reload_failed`、定时、`manual_evaluate`）：走统一策略接口；
  外部进程/WASM/`.so` 策略限时 1000ms，超时按第 6 节 `hold + 告警`。
- `manual_force` 与 `stable_window` 不按上述两条路径。

约束：

- supervisor 串行处理触发，同一时刻只有一个决策在途；`decision_id` 用于幂等与审计。
- 快照在触发时冻结；策略调用期间不读实时进程/文件/网络。
- 健康探测是周期性的，但策略不应把「两次探测之间」当成健康或故障。
- LKG 提升不属于 `fail/hold/rollback` 决策，单独由 supervisor 的稳定窗口流程处理。

## 2. 输入契约：状态快照

定稿形状（字段名是接口的一部分）：

```text
SupervisorSnapshot {
  snapshot_version: u32,
  decision_id: string,
  trigger: Trigger,
  observed_at_ms: i64,              // supervisor 时钟；策略不得自己取时间
  process: ProcessSnapshot,
  startup: StartupSnapshot,
  failures: FailureSnapshot,
  health: HealthSnapshot,
  runtime: RuntimeSnapshot,
  versions: VersionSnapshot,
  reload: Option<ReloadSnapshot>,
  manual: Option<ManualSnapshot>,
  extras: Map<string, Value>,       // 命名空间化；core 不解释
}
```

| 字段 | 类型 | 缺失/空值语义 |
|---|---|---|
| `snapshot_version` | `u32` | 必填；接口版本 |
| `decision_id` | `string` | 必填；同一触发唯一，重试幂等 |
| `trigger` | `string` enum | 必填；见第 1 节 |
| `observed_at_ms` | `i64` | 必填；Unix 毫秒；策略内部不得另取时间 |
| `process.state` | `"running" \| "exited" \| "unknown"` | `unknown` = supervisor 无法观察，不是健康 |
| `process.pid` | `Option<u32>` | `None` = 未知/未运行 |
| `process.started_at_ms` | `Option<i64>` | `None` = 不知道启动时间 |
| `process.uptime_ms` | `Option<i64>` | `None` = 无法计算 |
| `process.exit_code` | `Option<i32>` | `None` = 未退出、未知或非正常退出（看 `signal`） |
| `process.signal` | `Option<string>` | `None` = 非信号退出或未知；跨平台用平台信号名/编号的字符串 |
| `process.last_exit_at_ms` | `Option<i64>` | `None` = 本进程内没有退出记录 |
| `startup.grace_secs` | `u32` | 必填；来自 supervisor 配置 |
| `startup.grace_elapsed` | `bool` | 必填；按 supervisor 时钟计算，不由策略算 |
| `startup.ever_healthy` | `bool` | 必填；本进程内是否曾通过健康探测 |
| `startup.startup_at_ms` | `Option<i64>` | `None` = 不知道启动时间 |
| `failures.consecutive` | `u32` | `0` = 没失败；不是「未知」 |
| `failures.window_count` | `u32` | `0` = 窗口内没失败 |
| `failures.window_secs` | `u32` | 必填；窗口定义 |
| `failures.last_failure` | `Option<FailureInfo>` | `None` = 没有失败记录 |
| `failures.last_success_at_ms` | `Option<i64>` | `None` = 本进程内从未成功 |
| `health.probe_interval_secs` | `u32` | 必填 |
| `health.recent` | `Vec<ProbeResult>` | 空 = 尚未探测；不是「全部健康」 |
| `health.consecutive_unhealthy` | `u32` | `0` = 没有连续不健康证据 |
| `health.healthy_since_ms` | `Option<i64>` | `None` = 从未健康/未知；不是 0 |
| `health.last_probe_at_ms` | `Option<i64>` | `None` = 没有探测记录 |
| `runtime.active_turns` | `Option<u32>` | `None` = 指标不可用；不得当成 0 |
| `runtime.queued_turns` | `Option<u32>` | 同上 |
| `runtime.running_tasks` | `Option<u32>` | 同上 |
| `runtime.queued_tasks` | `Option<u32>` | 同上 |
| `runtime.draining` | `Option<bool>` | `None` = 未知 |
| `versions.current` | `VersionInfo` | 必填；当前运行版本标识 |
| `versions.lkg` | `Option<VersionInfo>` | `None` = 没有 LKG；不可回滚 |
| `since_last_healthy_ms` | `Option<i64>` | `None` = 本进程内从未健康；不是 0 |
| `reload` | `Option<ReloadSnapshot>` | `None` = 本次触发不是重载相关 |
| `manual` | `Option<ManualSnapshot>` | `None` = 非手动触发 |
| `extras` | object | 空对象 = 没有额外数据；未知键由策略忽略 |

嵌套类型：

```text
ProcessSnapshot {
  state, pid?, started_at_ms?, uptime_ms?, exit_code?, signal?, last_exit_at_ms?
}

FailureInfo {
  at_ms: i64,
  class: string,          // 现有错误分类，见下
  message: string,
  phase: "startup" | "health" | "exit" | "reload" | "runtime" | "manual"
}

ProbeResult {
  at_ms: i64,
  ok: bool,
  latency_ms?: u64,
  class?: string,         // ok=false 时的错误类别
  detail?: string
}

VersionInfo {
  id: string,             // 可读标识：git sha / 版本号 / generation id
  store_path?: string,    // Nix store path；非 Nix 平台可空
  build_id?: string       // 可选构建标识
}

ReloadSnapshot {
  at_ms: i64,
  outcome: "failed",
  class: string,
  detail: string,
  plugin_errors: u32
}

ManualSnapshot {
  action: "evaluate" | "force",
  requested?: "fail" | "hold" | "rollback",
  operator?: string
}
```

错误类别（`class`）沿用现有分类，只追加不重命名。当前已知示例：

- 进程/载体：`panic`、`provider_parse`、`unavailable`、`closed`、`internal`、`timeout`；
- 配置：`config`、`reload_failed`；
- 健康：`health`；
- 进程退出：`exit_nonzero`、`signal`。

`panic` 与 `provider_parse` 是内置策略做升级判断的关键类别；其它类别默认走累计阈值。

## 3. 输出契约

```text
SupervisorDecision {
  decision_version: u32,
  decision: "fail" | "hold" | "rollback",
  reason: {
    code: string,          // 机器可判定、稳定、追加式
    message: string,       // 给人读的一句话
    data?: object          // 可选审计字段；core 不解释
  },
  delay_ms?: u64           // 可选；缺省 = 0
}
```

语义：

- `hold`：保持当前版本，不切换 LKG；如果进程已退出，supervisor 按
  `1s → 2s → 4s → 8s → 封顶 30s` 的退避重启当前版本。`hold` 只表示
  「不判死、不回滚」。
- `fail`：停止自动重启/自动回滚，标记 failed，等待人工处理；不切换 LKG。
  不主动杀死正在运行的进程。
- `rollback`：切换到 LKG 版本并重启；**要求 `versions.lkg` 存在**。没有 LKG 时
  该决策视为非法，按第 6 节保守处理。内置策略在无 LKG 时返回 `fail`，且只在达到
  阈值后返回，不是首次失败即停。

`delay_ms`：

- `hold`：下一次决策/重启不早于该延迟；
- `fail`/`rollback`：执行前等待，用于排空在途轮次；
- 缺省或 0 = 不额外等待；
- 超过 supervisor 上限（建议 300000ms）视为非法值。

确定性要求：

- 同一 `SupervisorSnapshot` + 同一策略版本 + 同一 `options` 必须产生相同输出。
- 策略内部不得使用随机数、当前时间、环境变量、文件、网络或隐藏可变状态。
- 需要历史/计数时，数据必须已经在快照里；策略不能自己累计「第几次」。
- `reason.code` 是稳定的机器码（如 `startup_grace`、`consecutive_failures`、
  `window_failures`、`panic`、`provider_parse`、`health_unhealthy`、`manual_force`），
  `reason.message` 面向人。

## 4. 调用方与被调用方

- **调用方**：supervisor core。它负责触发、快照、策略选择、超时/异常兜底、决策校验、
  执行动作、审计记录。
- **被调用方（策略）**：
  - 内置策略（native，默认）；
  - 外置进程策略插件（JSON 快照 → JSON 决策；仅周期触发）；
  - 动态库 `.so` 策略 provider（in-process，C ABI + JSON；仅周期触发）；
  - WASM 策略模块（独立 host 或 `.so` host；同一份 JSON 契约；仅周期触发）。
- 配置：

```toml
[supervisor.policy]
provider = "acb.supervisor.default"   # 缺省用内置
# options 是自由表，core 不解释；内置策略读取自己的键
```

- 策略只做决策，不执行动作；`rollback` 的 store path/LKG 读取、进程重启、排空都由
  supervisor 执行。
- 载体无关性：周期触发所有载体都输入同一份 `SupervisorSnapshot`、输出同一份
  `SupervisorDecision`；即时触发由内置策略同步执行，不调用外部载体。外部进程/WASM
  只负责传输，不允许直接访问进程、文件或网络。
- 审计：每次决策记录 `decision_id`、`snapshot_version`、`policy.id`、`policy.version`、
  `decision`、`reason`、`executed_at`、执行结果；策略不能改写审计。

## 5. 内置策略清单

默认参数与配置键如下（已定）；都放在 `[supervisor.policy]` 下，`options` 原样交给策略。

| 规则 | 默认 | 配置键 | 决策 |
|---|---|---|---|
| 启动宽限 | 30s | `startup_grace_secs = 30` | 宽限期内 `hold`；panic 类仍按 panic 规则；进程在宽限内退出仍计失败 |
| 连续失败 | 3 次 | `consecutive_failures = 3` | 有 LKG → `rollback`；无 LKG → `fail`（达到阈值后才 fail） |
| 窗口累计失败 | 600s 内 5 次 | `window_failures = 5`、`window_secs = 600` | 同连续失败 |
| panic 类升级 | 1 次即回滚 | `panic_classes = ["panic"]`、`panic_rollback = true` | 有 LKG → `rollback`；无 LKG → `fail` |
| provider 解析失败 | 走累计阈值 | `provider_parse_uses_window = true` | 达到窗口/连续阈值才动作 |
| 健康持续不健康 | 60s 或连续 3 次探测 | `unhealthy_secs = 60`、`unhealthy_probes = 3` | 有 LKG → `rollback`；无 LKG → `fail` |
| 健康探针窗口 | 每 5s 一次，保留最近 16 条 | `probe_interval_secs = 5`、`probe_history = 16` | 供健康规则与审计使用 |
| 当前版本重启退避 | 1s → 2s → 4s → 8s → 封顶 30s | `restart_backoff_secs = [1, 2, 4, 8, 30]` | `hold` 重启当前版本时使用 |
| 外部策略超时 | 1000ms | `decision_timeout_ms = 1000` | 仅周期触发；超时 `hold + 告警` |
| 策略连续报错升级 | 3 次 | `policy_error_fail_after = 3` | 连续报错升级为 `fail` + 运维事件 |
| 策略错误降级 | `hold` | `on_policy_error = "hold"` | 默认不自动回退内置策略 |
| 人工强制 | 无默认 | 手动触发 | `manual_force` 绕过策略；`manual_evaluate` 走策略 |

内置策略的判定优先级（自上而下，先命中先返回，保证确定性）：

1. `panic` 类且达到 1 次；
2. 启动宽限期内（`hold`）；
3. 健康持续不健康达到阈值；
4. 连续失败达到阈值；
5. 窗口累计失败达到阈值；
6. 其它情况 `hold`。

`hold` 不是「什么都不做」：如果进程已退出，supervisor 按
`restart_backoff_secs` 的阶梯重启当前版本；只有 `fail` 才停止自动重启。
`policy_error_fail_after` 是 supervisor 侧的兜底计数，不是策略自己累计的。

## 6. 错误与降级

策略本身失败时的默认行为（已定）：

- **单次失败**：`hold + 告警`，不自动回退内置策略，不自动回滚。
  - 覆盖超时、返回 `Err`、panic、返回未知 `decision`、`rollback` 但没有 LKG、
    `delay_ms` 越界、缺少必填字段等非法输出。
  - 非法决策只告警、不执行；保留 `decision_id` 与原始返回，便于审计。
- **同一策略连续 3 次报错**（`policy_error_fail_after = 3`）：升级为 `fail`，
  并记录一条运维可见事件。
  - 计数由 supervisor 维护，策略自己不累计；一次成功决策或人工 `manual_force`
    可清零。
  - `fail` 只停止自动重启/自动回滚并标记 failed，不主动杀死正在运行的进程。
- **不自动回退内置策略**：同一快照下两套策略可能给出不同结果，自动切换会造成
  不可预测的版本切换。默认 `on_policy_error = "hold"`；以后若需要回退，必须显式
  配置，不作为默认。
- **策略超时**：仅周期触发的外部进程/WASM/`.so` 策略受 `decision_timeout_ms = 1000`
  约束；即时触发只走内置策略，不引入等待。
- 理由：策略失败不等于进程/版本失败；`hold` 保持当前版本并继续现有重启/观察路径，
  不会静默切换版本，也不会让坏策略立刻把系统判死。
- 告警进入现有 OOB 日志 / `/healthz` / notifier；策略本身标记 `unhealthy`。
- 策略失败不会停止 supervisor 的下一次触发评估。

补充：

- 连续报错升级为 `fail` 是为了避免坏策略让系统永久 `hold`；这是 supervisor
  侧的安全阀，不是策略自身逻辑。
- `manual_evaluate` 仍然走策略；`manual_force` 绕过策略，可用于人工恢复。

## 7. 版本与兼容

- `snapshot_version` 与 `decision_version` 从 `1` 开始；策略 manifest 声明
  `policy_api_version` 与支持的 `snapshot_version` 范围。
- 新增字段：**只增不改**，默认值/缺失语义必须明确；旧策略必须忽略未知字段，
  新 supervisor 必须能读旧策略的缺字段输出。
- 不得重命名、删除、复用已有字段；语义变化必须升版本，并在至少一个过渡窗口内
  同时接受旧版本。
- 枚举值只追加：`trigger`、`decision` 新增值要旧策略能安全忽略或按保守值处理；
  `reason.code` 可追加，未知 code 只记录、不改变决策。
- 策略返回比自己声明更高的 `decision_version` → 视为不兼容，hold + 告警。
- 配置键只追加；旧键保留并给出弃用窗口，不静默改变含义。

## 8. 已定决策摘要（2026-11-04）

1. **`hold` 允许重启当前版本**：`hold` = 不回滚、不判死；进程已退出时按
   `1s → 2s → 4s → 8s → 封顶 30s` 的退避重启当前版本。

2. **无 LKG 时达阈值 → `fail`**：只在达到阈值之后 `fail`（默认连续 3 次失败），
   不是首次失败即停。

3. **策略失败 → `hold + 告警`，不自动回退内置策略**；同一策略连续 3 次报错
   （含非法输出）升级为 `fail`，并记录一条运维可见事件。原因是避免坏策略让系统
   永久 `hold`。

4. **`manual_force` 绕过策略**，记录为 `operator_override`；`manual_evaluate`
   仍走策略。

5. **触发分层**：
   - 即时触发（进程退出、启动宽限）→ 只走内置策略，同步执行，不调用外部载体，
     避免在故障路径上引入最多 1s 等待；
   - 周期触发（`health_probe`、`reload_failed`、定时、`manual_evaluate`）→ 走统一
     接口，外部进程/WASM 策略限时 1000ms，超时 `hold + 告警`；
   - 原则：**故障路径上不许有等待；判断可以慢，恢复不能慢。**
   - 若实测显示分层带来不便，再单独评估，不现在扩大范围。
