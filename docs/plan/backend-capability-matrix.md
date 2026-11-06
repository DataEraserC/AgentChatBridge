# 部署后端能力对照：systemd / acbctl run / 容器

状态：2026-11-06。只列部署契约，不改 supervisor 策略判定。

结论先行：**只有 systemd 和 `acbctl run` 能区分 75（回滚后重拉）与 78（放弃）**。
容器 runtime 只能看到退出码并按自己的重启策略处理，区分不了这两个语义；容器侧必须让
`acb-launcher` 留在容器内消化回滚/放弃，不能把裸 bridge 直接当 entrypoint。

## 1. 三套后端

| 后端 | 入口 | 重启主体 | 备注 |
|---|---|---|---|
| systemd | `acb-launcher.service`（`Type=notify`） | systemd + launcher | 原生 `StartLimit`、`WatchdogSec`、`TimeoutStopSec` |
| `acbctl run` | `acbctl run --mode=launcher` | 父循环 + launcher | 无 systemd；父循环补齐 75/78、限流和信号转发 |
| 容器 | 容器 entrypoint 内的 `acb-launcher` | 容器 runtime + launcher | 无 systemd；75/78 必须在 launcher 内部消化 |

`acbctl run --mode=direct` 只用于最小前台验证：直接 spawn bridge，没有 launcher 的
LKG/健康循环，见 `examples/deploy/foreground-bridge.sh`。

## 2. 契约项对照

图例：**原生支持** = 后端自身提供；**ACB 侧补齐** = 由本仓库代码实现；
**不支持** = 当前后端无法表达该语义。

| 契约项 | systemd | `acbctl run` | 容器 |
|---|---|---|---|
| 75 / 78 区分 | 原生支持：`RestartForceExitStatus=75`、`RestartPreventExitStatus=78` | ACB 侧补齐：父循环读退出码，75 重读 `current.json` 重拉，78 打印原因退出 | 不支持：runtime 只能按退出码/重启策略处理，区分不了「回滚重拉」与「放弃」；必须由 launcher 内部消化 |
| 重启限流（60s / 3 次） | 原生支持：`StartLimitIntervalSec=60`、`StartLimitBurst=3`、`StartLimitAction=none` | ACB 侧补齐：父循环维护 60 秒窗口，第 3 次 75 后退出 78 并打印「已触发限流」 | ACB 侧补齐：容器 `--restart=on-failure:N` 只有重试次数，没有 60 秒窗口；等价限流仍需 launcher/入口脚本自己做 |
| Watchdog | 原生支持：`WatchdogSec=120` + launcher `sd_notify` 心跳 | 不支持：没有 `NOTIFY_SOCKET`/`WATCHDOG_USEC`，父循环只做进程等待与退出码处理 | 不支持：容器没有 systemd watchdog；healthcheck 是编排层探针，不是进程心跳 |
| 优雅停机 | 原生支持：`TimeoutStopSec=45`、`KillMode=control-group`，SIGTERM 发整个 cgroup | ACB 侧补齐：SIGTERM/SIGINT 转发子进程，最多等 45s，超时 SIGKILL | 原生支持：runtime 发 SIGTERM + stop timeout；进程树/信号语义按 runtime 实现 |
| LKG 回滚 | ACB 侧补齐：launcher/supervisor 写 `current.json`，systemd 只负责拉起 | ACB 侧补齐：同样的 launcher 路径；父循环只负责按 75 重拉 | ACB 侧补齐：同一个 launcher 留在容器内执行回滚；runtime 不参与版本选择 |
| 状态查询 | ACB 侧补齐：`acbctl status --mode=run` + `acbctl unit doctor`；status 不调用 systemctl | ACB 侧补齐：`run.json`、`current.json`、`lkg.json` + HTTP 探针 | ACB 侧补齐：同一套 `acbctl status --mode=run`，前提是 `run.json` 所在状态目录挂载到容器外可读位置 |

## 3. 换后端会丢什么

- **丢了 watchdog**：`acbctl run` 与容器都没有 systemd 的 `WatchdogSec`。进程挂死但
  端口还开着时，父循环/容器不会主动杀它；健康判定只能靠 `acbctl status` 的 HTTP 探针
  或编排层 healthcheck。
- **丢了 75/78 的原生区分**：容器 runtime 会把 75 和 78 都当成普通非零退出。容器部署
  必须让 `acb-launcher` 成为容器主进程，由 launcher 自己处理 75/78；不要直接把
  `acb-bridge` 作为 entrypoint。
- **限流语义不同**：systemd 是「60 秒窗口内 3 次失败」；容器 `--restart=on-failure:N`
  是「总共重试 N 次」。两者不能互换，容器侧需要 `acbctl run` 或等价入口脚本补窗口限流。
- **直接 bridge 没有回滚**：`--mode=direct` 只保证前台运行和基础重启/限流，不提供
  LKG、健康循环或 75/78 的 launcher 语义；它只适合最小验证。
- **status 不是 systemd 状态**：`acbctl status --mode=run` 只读文件与 HTTP 探针，不调用
  `systemctl`；systemd unit 是否加载、`FragmentPath` 是否被 `/etc` 托管，由
  `acbctl unit doctor` 单独检查。

## 4. 相关实现

- `crates/acb/src/run.rs`：父循环、75/78、限流、信号转发、`run.json`。
- `crates/acb/src/status.rs`：`status --mode=run`，不依赖 systemctl。
- `crates/launcher/src/main.rs`：launcher 的 75/78 与 `current.json` 读取。
- `deploy/systemd/acb-launcher.service`：systemd 侧 `StartLimit`/`WatchdogSec`/停机参数。
