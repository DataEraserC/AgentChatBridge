# ADR-0034 换代不重启组件：组件跨代接管

来源：用户报障（2026-11-17）：「每次你进行换代都会导致 dsh 会话中断」；
裁定：「换代不涉及到的程序不应该被重启」，并选择方案 A（组件管理独立）。

## 这条决定了什么行为

**换代（payload/generation 切换）只重启 bridge，不重启组件。**
组件（dsh / opencode / snowluma / acp / drivers）在换代中继续运行，
新 launcher 启动时**接管**它们而不是新起一份。

## 现状：为什么换代一定会断 dsh

链路（`crates/launcher/src/main.rs`）：

```
maybe_handle_upgrade():
    supervisor.stop_child()            // 停 bridge
    profile::activate_generation(...)  // 提交新代指针
    supervisor.request_exit(75)        // launcher 退出
→ systemd 重拉 launcher
→ 主循环退出后 converge_shutdown() 已经先停掉了所有组件
→ 新 launcher 重新 spawn 全部组件
```

`ps` 实测：所有组件都是 launcher 的直接子进程（ppid = launcher pid）。
所以 launcher 一退出，`converge_shutdown` 会把组件全停掉。

`KillMode=process` 保证了 systemd 不会连带杀组件的 cgroup，但
**launcher 自己在退出前主动收敛了它们**——保护被自己的收尾逻辑绕过。

## 决策：换代路径不收敛组件，新 launcher 接管

### 1. 区分「换代退出」与「停机退出」

`converge_shutdown` 现在被两处调用，语义不同：

| 路径 | 该做什么 |
|---|---|
| `shutdown::requested()`（systemd stop / 关机） | 收敛组件 + bridge + 残留后代 |
| 升级提交后 exit 75（换代） | **只停 bridge，组件留给新 launcher** |

所以引入 `ExitReason::Upgrade`：换代路径不调用 `converge_shutdown`，
只做 bridge 的停止（`supervisor.stop_child()` 在 `maybe_handle_upgrade` 里已经做了）。

失败回滚路径（stage 失败 / 提交失败 / 目标不可用）仍然收敛：
那时新旧代都没起来，留着组件没有归属，反而会挡住下一次启动。

### 2. 新 launcher 启动时接管存活组件

现状：`ComponentManager::new` 无条件 spawn 所有 enabled 组件。
直接不收敛会导致**同一组件两份进程**。

接管判据（三条全中才接管）：

1. `components/<name>.json` 里记着 `pid` 且非空
2. `kill(pid, 0)` 成功（进程还在）
3. `/proc/<pid>/exe` 解析出的路径**与本次要启动的 program 相同**

第 3 条是必须的：pid 会被复用，仅凭「pid 还活着」可能接管一个毫不相干的进程。
比较用 `instance.rs` 里已有的 `same_program`（它已经处理了 nix `.<name>-wrapped`
包装器，见上一次换代死循环的教训）。

接管后：不 spawn，把 state 里的 `phase` 保持为 `healthy`，继续正常 probe。
probe 失败时按既有重启策略处理——接管来的进程与原生起的没有区别。

### 3. 幂等与去重

`acbctl component status` 已经在读 `components/<name>.json`，接管不改变它的形状。
唯一新增的语义：启动时如果接管成功，日志里明确写一句
`adopted running component <name> pid=<pid>`，否则「为什么没重启」无从查起。

## 不做的事

- **不把组件完全从 launcher 剥离成独立 systemd 单元**。评估过：`ComponentManager`
  有 1149 行，含依赖排序、oneshot 语义、`leave_running`、proctree 收敛、
  日志轮转。拆成独立单元要重新实现与 launcher 的状态同步，改动量 300–500 行，
  而收益与「接管」相同。先做接管，够用就不拆。
- **不做 `exec` 原地换代**。它同样能保住子进程，但 launcher 的可执行文件路径
  由 generation 决定，`exec` 后仍需重建全部状态机，且失败回退更难做。

## 验收标准

1. 换代（`acbctl upgrade --payload ...`）后：
   - bridge 的 pid 变了
   - **组件的 pid 不变**
   - `dsh` 会话不中断（用控制台 `/api/dependencies` 的 `since` 字段验证：
     组件的 `since` 时间戳在换代前后一致）
2. `systemctl --user restart acb-bridge`（非换代）**仍然重启组件**：
   这是「用户要求停掉服务」的语义，不能因为换代需求而改变。
3. 组件进程已死但 state 里残留 pid → 正常 spawn，不接管。
4. pid 活着但 exe 不匹配 → 正常 spawn，不接管（pid 复用防护）。
5. `deploy/verify.sh` 全绿。

## 已知代价

- 换代后组件仍跑**旧代的可执行文件**（如果组件 program 路径带 generation 前缀）。
  这是有意的：用户要的是「不相关的程序不要重启」。如果换代确实改了组件本身，
  需要显式 `systemctl --user restart acb-bridge` 或单独重启该组件。
  **这条要写进换代的输出里**，否则「换代了但组件没升级」会成为一个安静的坑。
- 接管依赖 state 文件的准确性。如果 state 被清掉但进程还在，会出现两份。
  缓解：接管日志 + `acbctl component status` 显示真实 pid。
