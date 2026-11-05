# Supervisor 接入 systemd：launcher 层与重启契约

状态：**设计 + 最小实现（2026-11-05）**。策略与回滚逻辑在
`acb-supervisor-policy` / `acb-supervisor` 已有实现；本文件只定义部署拓扑、
重启责任与退出码，不改变策略判定。

## 1. 为什么需要 launcher

systemd 的 `ExecStart` 是静态的：它只会一遍遍启动同一个命令。自动回滚要在运行期
换版本，因此不能让 systemd 直接管 `acb-bridge`，中间必须有一层
`acb-launcher`：

```text
systemd (acb-launcher.service, Type=notify)
  └─ acb-launcher                         ← 唯一被 systemd 拉起的进程
       ├─ 读 state_dir/current.json       ← 决定启动哪个版本
       ├─ acb-supervisor 策略循环          ← 采样进程/健康/失败，产出 fail/hold/rollback
       └─ 子进程（acb-bridge 或包装器）    ← launcher spawn 后 wait
```

**launcher 不 `exec` 替换自己。** 如果 launcher `exec` 成 acb，就没有进程能在回滚时
停掉子进程、按契约退出 75；systemd 看到的也只是 acb 的退出码。这里采用
`spawn + wait`：launcher 始终是 systemd 的 main PID，负责停子进程、写指针、决定退出码。

### current.json 与 lkg.json

- `state_dir/current.json`：launcher **这次应该启动**的命令。
  `{"command":{"program":"...","args":[...]},"updated_at_ms":...,"updated_by":"acb-launcher"}`。
  launcher 启动时如果文件不存在（或损坏）就写配置里的 `supervisor.program/args`。
- `state_dir/lkg.json`：supervisor 提升的 last-known-good，由稳定窗口
  （`stable_window_secs`）写入。
- **回滚 = 把 LKG 写进 current.json，然后退出 75**。systemd 重新拉起 launcher，
  launcher 读到新指针，启动 LKG；supervisor 自身不再直接起 LKG。
- 回滚后 `lkg.json` 仍指向刚回滚到的版本；新 launcher 发现「LKG 等于 current」时
  视为没有 LKG。这是有意的：只允许一次自动回滚，回滚后的版本再失败就进入终态。

## 2. 谁在什么情况下重启/回滚

| 触发 | 决策方 | 动作 | systemd 是否参与 | 退避 |
|---|---|---|---|---|
| 子进程非 0 退出 / panic | supervisor 内置策略 | 宽限期内 `hold`；达到连续/窗口/panic 阈值后 `rollback`（有 LKG）或 `fail`（无 LKG） | 否（launcher 还活着） | supervisor 的 `restart_backoff_secs=[1,2,4,8,30]` |
| 健康探针持续失败 ≥ `unhealthy_secs=60` | supervisor | 有 LKG → `rollback`；无 LKG → `fail` | 否 | 无（直接决策） |
| 策略 `rollback` | launcher | 停子进程 → 写 `current.json`=LKG → 退出 **75** | 是：`RestartForceExitStatus=75` 重新拉起 launcher | `RestartSec=1`（仅 launcher 层） |
| 策略 `fail` / 回滚写指针失败 / 回滚后版本再失败 | launcher | 停子进程 → 退出 **78** | 是：`RestartPreventExitStatus=78` **不再拉起**，unit 停在 failed | 无 |
| launcher 自己崩溃 / OOM | systemd | 按 `Restart=on-failure` 重新拉起 launcher | 是 | `RestartSec=1` + `StartLimitBurst=3/60s` |
| launcher/监督循环挂死，`WatchdogSec=120` 内没有心跳 | systemd | 杀整个 cgroup 并重启 launcher | 是 | `RestartSec=1` + StartLimit |
| 操作员 `systemctl stop` | systemd | SIGTERM 发整个 cgroup；launcher 捕获后按契约退出 0 | 是（stop，不是 restart） | 无 |
| 连续 3 次 launcher 拉起（含回滚）在 60s 内失败 | systemd | `StartLimitAction=none` → 停在 failed | 是 | `StartLimitBurst=3` |

### 为什么不会出现两套退避互相打架

- **子进程的重启**由 supervisor 的 `restart_backoff_secs` 控制；这些重启发生在
  launcher 内部，systemd 完全看不见，也不会叠加 `RestartSec`。
- **launcher 的重启**只在两种情况下发生：回滚（75）或 launcher 自己崩了。
  回滚时 supervisor 已经把「旧版本失败」处理完，systemd 的 `RestartSec=1` 只负责
  把新 launcher 拉起来，不会和 hold 退避同时计时。
- `StartLimitBurst=3` / `StartLimitIntervalSec=60` 是 launcher 层的熔断，不是子进程
  退避：即使回滚指针写错、systemd 反复拉起，也会在 3 次内停下。

## 3. 退出码契约

| 退出码 | 含义 | systemd 配置 |
|---|---|---|
| `0` | 正常停止（操作员 stop / 手动运行时的 SIGTERM） | 不重启 |
| `75` | 需要回滚后重启；`current.json` 已换成 LKG | `RestartForceExitStatus=75`，`Restart=on-failure` |
| `78` | 监督器放弃自动恢复（`fail`、无 LKG、回滚写指针失败、回滚后版本再失败） | `RestartPreventExitStatus=78`，不重启，停在 failed |
| 其它 | 普通失败 | `Restart=on-failure` + `RestartSec` + StartLimit |

常量在 `crates/supervisor/src/lib.rs`：
`EXIT_OK=0`、`EXIT_ROLLBACK=75`、`EXIT_GIVE_UP=78`。unit 模板、launcher 与文档
只引用这三个值，不在各处写魔法数字。

**75 不能被当成成功**：不要给 unit 加 `SuccessExitStatus=75`；`RestartForceExitStatus=75`
的作用是即使以后有人加了成功码，也强制重启。

## 4. Watchdog 与 unhealthy 的先后

- `WatchdogSec=120`；launcher 按 `WATCHDOG_USEC/2`（60s）发 `WATCHDOG=1`。
- supervisor `unhealthy_secs=60`、`probe_interval_secs=5`。子进程假死/健康端点不响应时，
  supervisor 在约 60s 做出 `rollback`，launcher 退出 75，systemd 重启 launcher。
  **watchdog 不会先触发**。
- watchdog 兜的是 launcher/监督循环自己挂死：这时没有进程能做决策，120s 后 systemd
  杀 cgroup 并重启 launcher。两条路径不会互相掩盖，因为它们的观察对象不同：
  supervisor 看子进程健康，watchdog 看 launcher 是否还活着。
- `Type=notify` 的 `READY=1` 只表示 launcher 已启动并尝试拉起当前版本，不表示子进程
  已健康；健康仍由 supervisor 判定。
- `TimeoutStopSec=45` > acb 默认排空 30s，避免 stop 时提前 SIGKILL。

## 5. 部署形态

### 普通 Linux

`deploy/systemd/acb-launcher.service` 是完整模板：

- `Type=notify`、`WatchdogSec=120`、`Restart=on-failure`、`RestartSec=1`、
  `RestartForceExitStatus=75`、`RestartPreventExitStatus=78`、
  `StartLimitIntervalSec=60`、`StartLimitBurst=3`、`StartLimitAction=none`、
  `KillMode=control-group`、`TimeoutStopSec=45`。
- `ExecStart=/usr/local/bin/acb-launcher --config /etc/acb/supervisor.toml`，
  `WorkingDirectory=/var/lib/acb`。按实际安装路径改。
- 不需要 NixOS generation，也不需要 nix store 路径；任何有 `acb-launcher` 二进制的
  systemd 发行版都能用。

### Nix（可选，不默认启用）

- `deploy/nix/nixos.nix`：`services.acb-launcher.enable = true` 后生成
  `systemd.services.acb-launcher`。`package` 指向包含 `acb-launcher` 的包，
  `configFile` 指向 supervisor.toml。
- `deploy/nix/home-manager.nix`：同样的 option，生成
  `systemd.user.services.acb-launcher`，适合 Home Manager 的 user systemd。
- 两个 module 都只是可选 import；仓库的默认构建、flake 输出和用户环境不会被自动改动。
- module 不读取 NixOS generation：`ExecStart` 只有 launcher 路径 + 配置路径，
  版本切换仍走 `current.json`。

### 与 `acb-supervisor` 的关系

`acb-supervisor` 仍保留给「没有 systemd、手动跑」的场景：它内部自己起 LKG
（`RollbackMode::Internal`）。systemd 部署用 `acb-launcher`（`RollbackMode::External`）；
两者共用同一套策略、探针、LKG 读写和审计格式，不维护第二套决策逻辑。

## 6. 验证方式

脚本：`deploy/verify-launcher.sh`（使用 `systemd-run --user`，不修改持久 unit）。

覆盖三个场景：

1. **正常重启**：子进程退出 → supervisor `hold` + 退避 → 同一 launcher 内拉起。
2. **回滚后换版本**：策略返回 `Rollback` → launcher 以 75 退出 → systemd 重拉 →
   `current.json` 指向 LKG，新版本启动。
3. **连续回滚失败**：LKG 也失败且没有第二个 LKG → launcher 以 78 退出 →
   `RestartPreventExitStatus=78` 生效，unit 停在 failed，不再复活。

验证时同时看：

- `systemctl --user status <unit>` 的 `Active:` / `Main PID:` / `Result:`
- `journalctl --user -u <unit>` 的关键行：`READY=1` 对应的 `acb-launcher: started`、
  `supervisor decision ... rollback`、`rollback requested`、退出码 75/78。
- `state_dir/current.json` 的 `command.program` 是否切到 LKG。
- `state_dir/supervisor.log` 的 `execute` 审计事件（`rollback_requested` /
  `stopped_auto_recovery`）。

## 7. 已知边界

- launcher 只识别文件系统路径的 `NOTIFY_SOCKET`；少数 systemd 部署使用抽象
  socket（`@...`），此时 `READY=1`/`WATCHDOG=1` 会被跳过并打日志。标准 systemd
  发行版默认是 `/run/systemd/notify`。
- 回滚只支持一个 LKG（没有多级历史）；回滚后的版本再失败就停机，这是为了避免
  在错误版本之间来回震荡。
- `StartLimitBurst=3` 是保守值：一次回滚通常只消耗 1 次重启；如果部署需要更多
  launcher 层重试，调大它，但不要和 supervisor 的连续失败阈值混在一起。

## 8. 实测记录（2026-11-05，user systemd）

命令：`./deploy/verify-launcher.sh`（临时 unit，不改持久配置）。

### 场景 1：正常重启，launcher 不退出

```text
MainPID before=2706226 after=2706226（正常重启应保持不变）
Active: active (running) ...
supervisor audit: {"event":"execute","decision":"hold","result":"restarted",
  "target":{"program":".../current.sh"}}
... [v-current] start pid=2706318
```

说明：子进程退出由 supervisor 的 hold 退避在 launcher 内部重启，systemd 的
`MainPID` 不变。

### 场景 2：Rollback → 75 → systemd 重拉 → 换 LKG

```text
supervisor decision trigger=process_exit decision=rollback
  reason=consecutive_failures (consecutive failures reached 1)
supervisor rollback requested -> .../lkg-good.sh (exit 75)
systemd[...]: Main process exited, code=exited, status=75/TEMPFAIL
systemd[...]: Scheduled restart job, restart counter is at 1.
acb-launcher[...]: current.json selects .../lkg-good.sh (config says .../current.sh)
Active: active (running) ...
CGroup: .../lkg-good.sh
```

`current.json` 已切到 `lkg-good.sh`，systemd 用 `RestartForceExitStatus=75` 重新拉起。

### 场景 3：回滚后的版本再失败 → 78 → failed，不复活

```text
supervisor decision trigger=process_exit decision=fail
  reason=consecutive_failures (consecutive failures reached 1;
  no LKG available, fail instead)
systemd[...]: Main process exited, code=exited, status=78/CONFIG
systemd[...]: Failed with result 'exit-code'.
Active: failed (Result: exit-code)
MainPID after failed=0（应为空/0，证明没有复活）
```

`RestartPreventExitStatus=78` 生效：unit 停在 failed，`MainPID=0`，没有再拉起。

