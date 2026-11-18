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
    Command::new("systemd-run")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
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

/// 用 `systemd-run` 起一个独立 unit，立即返回。返回 unit 名。
///
/// `--collect`：unit 退出后自动清理，避免 `systemctl` 里堆一堆 failed unit。
/// `--quiet`：不把 "Running as unit: ..." 打到我们的日志里（我们自己会记）。
pub fn start(spec: &Spec<'_>) -> Result<String, String> {
    let unit = unit_name(spec.name);

    // 同名 unit 还挂着就先停掉再起。
    //
    // 不加这一步会失败在 `Unit ... was already loaded`：组件的重启路径
    // （崩溃后 `Restarting` → 再 spawn）必然撞到自己；`--collect` 只在
    // unit 退出后清理，停止到清理之间有个窗口。
    let _ = Command::new("systemctl")
        .args(["--user", "stop", &unit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "reset-failed", &unit])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let log = spec.log_path.display().to_string();

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

    let output = command
        .output()
        .map_err(|error| format!("systemd-run {unit}: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "systemd-run {unit} failed: {}",
            stderr.trim().replace('\n', "; ")
        ));
    }
    Ok(unit)
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
fn run_with_deadline(
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
    // 所以超时由我们自己管：发 stop，然后按 `timeout_secs` 轮询 ActiveState。
    // `systemctl stop` **必须有独立上限**：它是阻塞调用，unit 停得慢时
    // `Command::output()` 会一直等下去。而 launcher 的停机总预算是 45 秒
    //（unit 的 `TimeoutStopSec`），预算用完之后 systemd 直接 SIGKILL launcher
    // ——于是组件和 bridge 都留在原地变成孤儿，下次换代报
    // `no_restart_owner`（2026-11-18 实测两次）。
    //
    // 这里给 stop 留 `timeout_secs` 的一半（至少 3 秒），剩下的时间用来确认
    // ActiveState。超时就杀掉 systemctl 子进程并按「没能停住」处理：比让整个
    // launcher 被 SIGKILL 好。
    let wait_budget = Duration::from_secs((timeout_secs / 2).max(3));
    match run_with_deadline(
        Command::new("systemctl").args(["--user", "stop", unit]),
        wait_budget,
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
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    while Instant::now() < deadline {
        match state(unit) {
            // `--collect` 生效后 unit 会整个消失，也算停住了。
            None => return Ok(()),
            Some(current) if current.active_state != "active" => return Ok(()),
            Some(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    Err(format!("{unit} still active after {timeout_secs}s"))
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
}
