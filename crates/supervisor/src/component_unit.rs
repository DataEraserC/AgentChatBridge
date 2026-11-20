//! 把组件放进**独立 systemd user unit** 启动（ADR-0034）。
//!
//! 为什么不能直接 `Command::spawn`：组件会落在 `acb-bridge.service` 的 cgroup
//! 里，而换代是「launcher 退出 75 → systemd 重拉 unit」——systemd 在 unit
//! 停止时会清空整个 cgroup。`KillMode=process` 只免除「systemd 主动杀子进程」，
//! 不免除「unit 停止 → 重启这个动作本身」。实测：日志打了
//! `leaving components running`，组件 pid 仍然全变。
//!
//! 为什么不能用 `systemd-run --scope`：它与调用方同生命周期，会阻塞到子进程
//! 退出（实测 3 秒子进程 → 调用方卡 3 秒），而 launcher 的 tick 循环不能卡。
//!
//! 所以用 `systemd-run --user --unit=acb-component-<name>`：立即返回，独立
//! cgroup，unit 重启不受影响。实测这三种机制的差异见 ADR-0034。

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// 组件 unit 名的前缀。`acb-component-<name>` 与 `acb-bridge.service` 平级，
/// 所以换代重启 bridge 时它不在被清理的 cgroup 里。
pub const UNIT_PREFIX: &str = "acb-component-";

/// 组件对应的 unit 名（含 `.service` 后缀）。
pub fn unit_name(name: &str) -> String {
    format!("{UNIT_PREFIX}{name}.service")
}

/// `systemd-run` 是否可用。不可用时调用方回退到直接 spawn——那种情况下换代
/// 会带走组件，但至少组件能跑起来（不能因为隔离机制缺失就让整个桥不可用）。
pub fn available() -> bool {
    // 显式关闭隔离：测试要断言「子进程语义」（trap、退出码、进程组停止顺序）时
    // 用这条路径。它与生产路径是两套实现，各自的测试分开写，不混着断言——
    // 混着断言只会测出「两套行为不一致」这个已知事实。
    //
    // 生产环境不要设它：设了就回到「换代带走组件」。
    if std::env::var("ACB_COMPONENT_NO_UNIT")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            value == "1" || value == "true" || value == "yes"
        })
        .unwrap_or(false)
    {
        return false;
    }
    run_with_deadline(
        Command::new("systemd-run")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(5),
    )
    .map(|output| output.status.success())
    .unwrap_or(false)
}

/// 一次组件启动的完整参数。
///
/// 单独建结构体而不是一堆函数参数：这些字段全部来自 `ResolvedComponent`，
/// 传错一个不会有编译错，只会让组件起不来或环境不全。
pub struct Spec<'a> {
    pub name: &'a str,
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: Option<&'a str>,
    /// 环境变量（含 `ACB_COMPONENT_*` 与用户配置的 env）。
    pub env: Vec<(String, String)>,
    /// 组件日志文件；stdout/stderr 都追加到这里。
    pub log_path: &'a Path,
}

/// `systemd-run` 的报错是否属于「同名 unit 还占着」这种可自愈竞态。
///
/// 两种来源：
/// 1. `stop` 到 `--collect` 真正卸载 unit 之间有个窗口，重启路径撞到自己
///    （既有注释描述的场景）；
/// 2. launcher 换代瞬间 user bus 短暂不可用（`Connection refused`），下面的
///    `stop`/`reset-failed` 被静默跳过，bus 恢复后残留的 unit 还在——实测
///    rebuild 后 drivers/acp 连续 5 次撞这个再进 failed 终态。
fn is_name_collision(error: &str) -> bool {
    error.contains("was already loaded") || error.contains("has a fragment file")
}

/// 清掉同名 unit 的残留：`stop` + `reset-failed`。刻意静默——unit 不存在、
/// bus 短暂不可用都会返回错误，而 caller 会在 `systemd-run` 那一步拿到明确
/// 结果（成功或撞名），这里不需要额外日志。
fn clear_stale_unit(unit: &str) {
    let _ = run_with_deadline(
        Command::new("systemctl")
            .args(["--user", "stop", unit])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(10),
    );
    let _ = run_with_deadline(
        Command::new("systemctl")
            .args(["--user", "reset-failed", unit])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        Duration::from_secs(10),
    );
}

/// 一次完整启动尝试：清残留 → `systemd-run`。`Err` 表示这次没起来。
fn start_once(spec: &Spec<'_>, unit: &str, log: &str) -> Result<(), String> {
    // 三处 systemctl/systemd-run 调用都必须有 deadline。`Command::status()` /
    // `output()` 是阻塞的：bus 忙、unit 带着挂起 job 时整个 `start` 会停在这里，
    // launcher 主循环随之停转，systemd `WatchdogSec=2min` 到点直接 SIGABRT
    // （2026-11-19 02:32 实测，栈顶 start → Command::status → wait4）。
    // 超时按「这一步没成功」处理：宁可组件重启失败一次，也别让 launcher 被打死。
    clear_stale_unit(unit);

    let mut command = Command::new("systemd-run");
    command
        .arg("--user")
        .arg("--collect")
        .arg("--quiet")
        .arg(format!("--unit={unit}"))
        // 输出走组件自己的日志文件，与直接 spawn 时保持一致；
        // 这样 `logs/<name>.log` 的语义不因隔离机制改变。
        .arg(format!("--property=StandardOutput=append:{log}"))
        .arg(format!("--property=StandardError=append:{log}"))
        // `mixed`：TERM 只打主进程（子进程能跑自己的 trap），超时后 KILL 再整组。
        // 与原先 `process_group(0)` + 按组 TERM 的行为不完全一致，但更贴近
        // 「组件自己收尾」的语义——脚本的 trap 能被执行到（实测 control-group
        // 会把 TERM 直接发给组里所有进程，trap 计数对不上）。
        .arg("--property=KillMode=mixed")
        // 组件是长期进程；默认 Type=simple 就够，显式写出来免得被未来的默认值改掉。
        .arg("--property=Type=simple");

    if let Some(cwd) = spec.cwd.filter(|cwd| !cwd.is_empty()) {
        command.arg(format!("--working-directory={cwd}"));
    }
    for (key, value) in &spec.env {
        command.arg(format!("--setenv={key}={value}"));
    }

    // 程序与参数放在 `--` 之后：避免被 systemd-run 自己的选项吞掉。
    // program 是 nix store 绝对路径（配置里给的），不需要 PATH 解析。
    command.arg("--").arg(spec.program).args(spec.args);

    let output = run_with_deadline(&mut command, Duration::from_secs(30))
        .map_err(|error| format!("systemd-run {unit}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "systemd-run {unit} failed: {}",
            stderr.trim().replace('\n', "; ")
        ));
    }
    Ok(())
}

/// 用 `systemd-run` 起一个独立 unit，立即返回。返回 unit 名。
///
/// `--collect`：unit 退出后自动清理，避免 `systemctl` 里堆一堆 failed unit。
/// `--quiet`：不把 "Running as unit: ..." 打到我们的日志里（我们自己会记）。
///
/// 撞名自愈：换代时 user bus 可能短暂不可用，首次 `stop`/`reset-failed` 被
/// 跳过，bus 恢复后同名 unit 还在。这类竞态不该耗掉外层的 `max_attempts`，
/// 否则 rebuild 后组件直接进 failed 终态再也不被拉起（实测 drivers/acp）。
pub fn start(spec: &Spec<'_>) -> Result<String, String> {
    let unit = unit_name(spec.name);
    let log = spec.log_path.display().to_string();

    let result = match start_once(spec, &unit, &log) {
        Err(error) if is_name_collision(&error) => {
            eprintln!("acb-component: {unit} name collision; forcing cleanup and retrying once");
            // `--collect` 从 stop 到真正卸载 unit 有窗口，留一点时间再重试；
            // start_once 内部会再 stop/reset 一次。只重试一次：持续撞名是真
            // 故障，交给外层的 fail_or_backoff 处理。
            std::thread::sleep(Duration::from_millis(200));
            start_once(spec, &unit, &log)
        }
        other => other,
    };
    result.map(|_| unit)
}

/// 等 unit 拿到 MainPID（启动是异步的：`systemd-run` 返回时进程可能还没 exec）。
///
/// 不无限等：`timeout` 到了就返回最后一次读到的状态，由调用方决定怎么处理。
/// 组件启动本身有 `startup_grace_secs` 的超时，这里只负责把「刚起来」这段
/// 竞态收掉——否则刚 spawn 就判失败会白白耗掉一次重启配额。
pub fn wait_main_pid(unit: &str, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(state) = state(unit) {
            if state.main_pid != 0 && state.active_state == "active" {
                return Some(state.main_pid);
            }
            // 明确失败就别再等了。
            if state.active_state == "failed" {
                return None;
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 跑一条命令，最多等 `budget`。超时杀掉子进程并返回原因。
///
/// 标准库没有 `wait_timeout`，所以自己轮询 `try_wait`。轮询间隔 20ms：这个
/// 函数只在停机路径上跑，几十毫秒的延迟无所谓，而 `systemctl` 通常几十毫秒
/// 就返回了。
pub fn run_with_deadline(
    command: &mut Command,
    budget: Duration,
) -> Result<std::process::Output, String> {
    use std::process::Stdio;
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动：{error}"))?;
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child
                    .wait_with_output()
                    .map_err(|error| format!("读取输出失败：{error}"));
            }
            Ok(None) if Instant::now() >= deadline => {
                // 杀不掉也要往下走：留下僵尸比让 launcher 被 SIGKILL 好。
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("超过 {} 秒未返回，已终止", budget.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(format!("等待失败：{error}")),
        }
    }
}

/// 停掉一个组件 unit。返回它是否确实进入非 active 状态。
pub fn stop(unit: &str, timeout_secs: u64) -> Result<(), String> {
    // `systemctl` **没有** `--timeout` 选项（那是 `systemd-run` 的）。写了会得到
    // "unrecognized option '--timeout'"；更糟的是它退出码仍是 0，错误只出现在
    // stdout 上，于是停止静默失败、`systemctl stop` 之后组件还活着（实测）。
    //
    // 所以超时由我们自己管：发 stop，然后按预算轮询 ActiveState。
    //
    // ## 预算是**一次性的**，不是每步各给一份
    //
    // `timeout_secs` 是调用方给的总预算（`stop_one` 已经把它压到「距离全局
    // deadline 还剩多少秒」）。这里必须把它当成**整段**的花费上限：
    //
    //     发出 stop 与等它返回  +  轮询 ActiveState   ≤  timeout_secs
    //
    // 早先的写法给 systemctl 切一半、剩下又完整用一遍 `timeout_secs` 轮询，
    // 实际最多花 1.5 倍预算。组件是串行停的，几个加起来就吃穿了 launcher 的
    // 40 秒，于是 systemd 在 `TimeoutStopSec=45s` 处 SIGKILL launcher，bridge
    // 和组件全留下来变孤儿（2026-11-18 实测两次 `no_restart_owner`）。
    //
    // 另一个坑：`.max(3)` 这种「保底」会把小预算偷偷抬高。剩余 2 秒时它给 3 秒，
    // 多花的 1 秒正是压垮骆驼的那一秒。这里只给下限 1 秒——`Duration` 不接受 0，
    // 而 0 秒预算本来就意味着「已经没时间了，发一下就返回」。
    let total = Duration::from_secs(timeout_secs.max(1));
    let started = Instant::now();
    // stop 拿总预算的一半（但不超过 10 秒：`systemctl stop` 正常情况下几十毫秒
    // 就返回，给它几十秒毫无意义，只是在卡住时白等）。
    let stop_budget = (total / 2).min(Duration::from_secs(10));
    match run_with_deadline(
        Command::new("systemctl").args(["--user", "stop", unit]),
        stop_budget,
    ) {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "systemctl stop {unit} failed: {}",
                stderr.trim().replace('\n', "; ")
            ));
        }
        Err(reason) => return Err(format!("systemctl stop {unit}: {reason}")),
    }

    // 等它真的不再是 active。`stop` 返回不等于进程已经退出。
    //
    // 用**总预算剩下的部分**，不再另给一份。
    let deadline = started + total;
    while Instant::now() < deadline {
        match state(unit) {
            // `--collect` 生效后 unit 会整个消失，也算停住了。
            None => return Ok(()),
            Some(current) if current.active_state != "active" => return Ok(()),
            Some(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    Err(format!("{unit} still active after {}s", total.as_secs()))
}

/// 查一个组件 unit 的 `ACB_COMPONENT_NAME`，用来确认它确实是这个组件。
///
/// 为什么不用命令行：组件脚本常用 `exec` 换成真正的程序，脚本名从 cmdline
/// 里消失（dsh 就是：`dsh_nix.sh` 内部 `exec node ...`）。而我们在
/// `--setenv` 里注入的 `ACB_COMPONENT_NAME` 是 unit 的属性，`exec` 抹不掉。
pub fn component_name_of(unit: &str) -> Option<String> {
    // 同样有界：这个函数在 adopt 探测里调用，卡住会拖死整个 reconcile。
    let output = run_with_deadline(
        Command::new("systemctl").args([
            "--user",
            "show",
            unit,
            "--property=Environment",
            "--no-pager",
        ]),
        Duration::from_secs(5),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // 输出形态是 `Environment=K=V K=V ...`（不带 `--value` 时 systemd 会加
    // 属性名前缀）。先剥掉它，再按空格切出各对。
    let body = text
        .trim()
        .strip_prefix("Environment=")
        .unwrap_or(text.trim());
    body.split_whitespace()
        .find_map(|pair| pair.strip_prefix("ACB_COMPONENT_NAME="))
        .map(str::to_owned)
}

/// 一个组件 unit 的当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    /// `active` / `inactive` / `failed` / `activating` …
    pub active_state: String,
    /// 主进程 pid；`0` 表示没有。
    pub main_pid: u32,
}

/// 查一个组件 unit 的状态。unit 不存在时返回 `None`。
pub fn state(unit: &str) -> Option<UnitState> {
    // 有界：这个函数在停机轮询里每 50ms 调一次。一旦 `systemctl` 卡住（bus 忙、
    // unit 状态迁移中），无超时的 `output()` 会把整条停机路径拖死，最后被
    // systemd SIGKILL（2026-11-18 实测两次）。5 秒足够正常返回。
    let output = run_with_deadline(
        Command::new("systemctl").args([
            "--user",
            "show",
            unit,
            "--property=ActiveState",
            "--property=MainPID",
            "--no-pager",
        ]),
        Duration::from_secs(5),
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut active_state = None;
    let mut main_pid = 0;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("ActiveState=") {
            active_state = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("MainPID=") {
            main_pid = value.trim().parse().unwrap_or(0);
        }
    }
    let active_state = active_state?;
    // `inactive` 且 pid=0 说明 unit 不在（`--collect` 已清掉）或从未启动。
    Some(UnitState {
        active_state,
        main_pid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// unit 名必须带前缀与 `.service` 后缀，且与 bridge 的 unit 不同级——
    /// 这正是「换代不带走组件」的前提。
    #[test]
    fn unit_name_is_prefixed_and_not_the_bridge_unit() {
        assert_eq!(unit_name("dsh"), "acb-component-dsh.service");
        assert_ne!(unit_name("dsh"), "acb-bridge.service");
        assert!(unit_name("dsh").starts_with("acb-component-"));
    }

    /// 带特殊字符的组件名也要能稳定映射。
    #[test]
    fn unit_name_keeps_the_component_name_intact() {
        assert_eq!(
            unit_name("dsh-presets"),
            "acb-component-dsh-presets.service"
        );
    }
    /// 卡住的命令必须被上限截断，不能一直等下去。
    ///
    /// 2026-11-19 02:32 实测事故：`component_unit::start()` 里的
    /// `Command::status()` 没有上限，`systemd-run` 卡住时整个 launcher 停转，
    /// systemd 的 `WatchdogSec=2min` 到点直接 SIGABRT（core dump 栈顶就是
    /// `start` → `Command::status` → `wait4`）。
    ///
    /// 这条测试盯住机制本身：给一个睡 60 秒的命令一个 1 秒上限，它必须在大约
    /// 1 秒返回 `Err`，而不是等满 60 秒。
    #[test]
    fn run_with_deadline_kills_a_stuck_command() {
        let started = Instant::now();
        let outcome =
            run_with_deadline(&mut Command::new("sleep").arg("60"), Duration::from_secs(1));
        let elapsed = started.elapsed();

        assert!(outcome.is_err(), "超时必须返回错误，不能假装成功");
        assert!(
            elapsed < Duration::from_secs(10),
            "必须在 ~1 秒返回，实际 {elapsed:?}——上限没生效的话 launcher 会被 watchdog 打死"
        );
    }

    /// 正常返回的命令不该被误杀。
    #[test]
    fn run_with_deadline_returns_output_for_a_fast_command() {
        let outcome = run_with_deadline(&mut Command::new("true"), Duration::from_secs(10));
        let output = outcome.expect("能跑完");
        assert!(output.status.success());
    }

    /// 撞名识别必须覆盖日志里出现的两种真实错误文本。
    #[test]
    fn is_name_collision_matches_real_systemd_errors() {
        assert!(is_name_collision(
            "systemd-run acb-component-drivers.service failed: Failed to start transient service unit: Unit acb-component-drivers.service was already loaded or has a fragment file."
        ));
        assert!(is_name_collision(
            "systemd-run acb-component-acp.service failed: Unit acb-component-acp.service has a fragment file."
        ));
    }

    /// bus 不可用、超时等真实故障不能被当成撞名自愈掉——那是要交给外层
    /// fail_or_backoff 的，误判会掩盖真正的启动问题。
    #[test]
    fn is_name_collision_rejects_non_collision_errors() {
        assert!(!is_name_collision(
            "systemd-run acb-component-dsh.service failed: Failed to connect to user scope bus via local transport: Connection refused"
        ));
        assert!(!is_name_collision(
            "systemd-run acb-component-dsh.service: 超过 30 秒未返回"
        ));
        assert!(!is_name_collision("program not found"));
    }
}
