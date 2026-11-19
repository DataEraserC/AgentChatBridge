//! 组件跑在独立 systemd unit 里的行为（ADR-0034）。
//!
//! 与 `component_runtime.rs` 分开：那边断言「子进程语义」（`Child`、退出码、
//! trap、停止顺序），这边断言「unit 语义」（独立 cgroup、换代不带走、接管）。
//! 两套实现本来就不同，混在一个文件里只会测出这个已知事实。
//!
//! 这些测试会真的创建 systemd user unit（`acb-component-<name>.service`），
//! 跑完自清理。没有 systemd user session 的环境会被跳过而不是失败——那种
//! 环境本来就跑不了这条路径（生产上 launcher 一定带 systemd）。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use acb_supervisor::component::PresetRegistry;
use acb_supervisor::component_runtime::{ComponentManager, Phase};
use acb_supervisor::component_state::ComponentState;
use acb_supervisor::now_ms;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(label: &str) -> TempDir {
    let base = std::env::temp_dir().join(format!(
        "acb-unit-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&base).expect("mkdir");
    TempDir(base)
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    // shebang 用 `/usr/bin/env`：NixOS 没有完整 FHS，systemd 直接执行
    // `#!/bin/sh` 会报 `Exec format error`（203/EXEC）。实测。
    fs::write(&path, format!("#!/usr/bin/env sh\n{body}\n")).expect("write script");
    let mut permissions = fs::metadata(&path).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod");
    path
}

fn state_dir(dir: &Path) -> PathBuf {
    dir.join("state").join("supervisor")
}

/// 确认 systemd-run 可用；不可用就跳过（返回 false）。
///
/// 不把「环境没有 systemd」当成测试失败：那会让 CI 与本地行为不一致，
/// 而这条路径在生产上一定跑得起来（launcher 由 systemd 拉起）。
fn unit_mode_available() -> bool {
    // 显式打开：本文件就是要测 unit 路径。
    // SAFETY: 测试进程内设置环境变量。
    unsafe { std::env::remove_var("ACB_COMPONENT_NO_UNIT") };
    acb_supervisor::component_unit::available()
}

fn manager(dir: &Path, text: &str) -> ComponentManager {
    let (_file, table) = acb_supervisor::component::parse_config_file(
        text,
        &PresetRegistry::empty(),
        std::path::Path::new("/tmp"),
    )
    .expect("parse config");
    ComponentManager::new(
        &state_dir(dir),
        &table,
        "test-sha",
        Some("inst-unit"),
        now_ms(),
    )
    .expect("create manager")
}

fn wait_for(
    manager: &mut ComponentManager,
    limit: Duration,
    mut predicate: impl FnMut(&mut ComponentManager) -> bool,
) {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        manager.tick(now_ms());
        if predicate(manager) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("condition not reached within {limit:?}");
}

fn read_state(dir: &Path, name: &str) -> ComponentState {
    ComponentState::load(&state_dir(dir), name)
        .expect("read state")
        .expect("state exists")
}

/// 清掉一个组件 unit（测试自清理，避免污染下次运行）。
fn cleanup_unit(name: &str) {
    let unit = acb_supervisor::component_unit::unit_name(name);
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "stop", &unit])
        .output();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "reset-failed", &unit])
        .output();
}

/// 组件跑在**独立 unit** 里，不在 launcher 的 cgroup 中。
///
/// 这是「换代不带走组件」的机制前提：unit 名不同 → cgroup 不同 →
/// `acb-bridge.service` 重启时它不被清理。
#[test]
fn component_runs_in_its_own_unit() {
    if !unit_mode_available() {
        eprintln!("skip: systemd-run unavailable");
        return;
    }
    let dir = temp_dir("own-unit");
    let script = script(&dir.0, "stay.sh", "while :; do sleep 0.2; done");
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "utalpha"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(10), |manager| {
        manager.phase("utalpha") == Some(Phase::Healthy)
    });

    let unit = acb_supervisor::component_unit::unit_name("utalpha");
    let state = acb_supervisor::component_unit::state(&unit).expect("unit exists");
    assert_eq!(state.active_state, "active", "unit 应当是 active");
    assert_ne!(state.main_pid, 0, "unit 应当有 MainPID");

    // cgroup 必须与 bridge 的不同：这是「不被连带清理」的直接原因。
    let cgroup_of = |unit: &str| {
        std::process::Command::new("systemctl")
            .args(["--user", "show", unit, "-p", "ControlGroup", "--value"])
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let component_cgroup = cgroup_of(&unit);
    assert!(
        component_cgroup.contains("acb-component-utalpha"),
        "组件应当在自己的 cgroup 里：{component_cgroup}"
    );

    manager.stop_all(now_ms());
    assert_eq!(manager.phase("utalpha"), Some(Phase::Stopped));
    cleanup_unit("utalpha");
}

/// 停服务要真能停掉 unit 里的组件。
///
/// 接管的进程 / unit 化的进程都不是我们的子进程，`Child::kill` 对它们无效。
/// 这条不成立的话，`systemctl stop` 之后 dsh 会继续跑。
#[test]
fn stopping_tears_down_the_unit() {
    if !unit_mode_available() {
        eprintln!("skip: systemd-run unavailable");
        return;
    }
    let dir = temp_dir("stop-unit");
    let script = script(&dir.0, "stay.sh", "while :; do sleep 0.2; done");
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "utbeta"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(10), |manager| {
        manager.phase("utbeta") == Some(Phase::Healthy)
    });
    let unit = acb_supervisor::component_unit::unit_name("utbeta");
    let pid = acb_supervisor::component_unit::state(&unit)
        .expect("unit exists")
        .main_pid;
    assert_ne!(pid, 0);

    manager.stop_all(now_ms());

    // unit 应当不再是 active，进程也不该还活着。
    let after = acb_supervisor::component_unit::state(&unit);
    let still_active = after
        .map(|state| state.active_state == "active")
        .unwrap_or(false);
    assert!(!still_active, "停服务后 unit 不该还是 active");
    assert!(
        !acb_supervisor::instance::process_alive(pid),
        "停服务后组件进程 {pid} 不该还活着"
    );
    cleanup_unit("utbeta");
}

/// 换代后新 manager 认领已经 active 的 unit，而不是再起一份。
#[test]
fn existing_unit_is_adopted_instead_of_respawned() {
    if !unit_mode_available() {
        eprintln!("skip: systemd-run unavailable");
        return;
    }
    let dir = temp_dir("adopt-unit");
    let marker = dir.0.join("started");
    let script = script(
        &dir.0,
        "stay.sh",
        &format!(
            "echo started >> '{}'\nwhile :; do sleep 0.2; done",
            marker.display()
        ),
    );
    let text = format!(
        r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "utgamma"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
        script.display()
    );

    let first_pid = {
        let mut manager = manager(&dir.0, &text);
        wait_for(&mut manager, Duration::from_secs(10), |manager| {
            manager.phase("utgamma") == Some(Phase::Healthy)
        });
        let pid = manager.pid("utgamma").expect("有 pid");
        // 关键：不 stop_all。这是换代路径的语义——组件留给下一代。
        std::mem::forget(manager);
        pid
    };
    thread::sleep(Duration::from_millis(300));
    let runs_before = fs::read_to_string(&marker)
        .map(|text| text.lines().count())
        .unwrap_or(0);
    assert_eq!(runs_before, 1, "第一次应当只执行一次");

    let mut second = manager(&dir.0, &text);
    assert_eq!(
        second.pid("utgamma"),
        Some(first_pid),
        "新 manager 必须认领同一个 unit 的 pid，而不是新起一份"
    );

    thread::sleep(Duration::from_millis(1200));
    let runs_after = fs::read_to_string(&marker)
        .map(|text| text.lines().count())
        .unwrap_or(0);
    assert_eq!(runs_after, 1, "接管后不得再次 spawn（脚本只应执行过一次）");

    second.stop_all(now_ms());
    cleanup_unit("utgamma");
}

/// 组件脚本用 `exec` 把自己换成别的程序时，仍然要能认出自己。
///
/// 这是线上实测踩到的（2026-11-17）：dsh 的 `dsh_nix.sh` 内部 `exec node ...`，
/// 于是 `/proc/<pid>/cmdline` 里**没有** `dsh_nix.sh`，只有 `node .../bin.js`。
/// 第一版用命令行复核身份，结果 dsh 每次换代都被重建——4 个组件里唯一失败的
/// 就是它，而它恰恰是会话所在的那个。
///
/// 修法：改用我们注入的 `ACB_COMPONENT_NAME`（unit 属性，`exec` 抹不掉）。
#[test]
fn component_that_execs_into_another_program_is_still_adopted() {
    if !unit_mode_available() {
        eprintln!("skip: systemd-run unavailable");
        return;
    }
    let dir = temp_dir("exec-adopt");
    let marker = dir.0.join("started");
    // 模拟 dsh：脚本 exec 成另一个程序，脚本名从 cmdline 里消失。
    let node = std::process::Command::new("sh")
        .args(["-c", "command -v sleep"])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|text| !text.is_empty())
        .expect("找得到 sleep");
    let script = script(
        &dir.0,
        "wrapper.sh",
        &format!("echo started >> '{}'\nexec {} 600", marker.display(), node),
    );
    let text = format!(
        r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "utdelta"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
        script.display()
    );

    let first_pid = {
        let mut manager = manager(&dir.0, &text);
        wait_for(&mut manager, Duration::from_secs(10), |manager| {
            manager.phase("utdelta") == Some(Phase::Healthy)
        });
        let pid = manager.pid("utdelta").expect("有 pid");
        std::mem::forget(manager);
        pid
    };
    thread::sleep(Duration::from_millis(300));

    // 真正的断言：认出自己，不重建。
    let second = manager(&dir.0, &text);
    assert_eq!(
        second.pid("utdelta"),
        Some(first_pid),
        "exec 掉脚本名的组件也必须被认领（否则每次换代都重建，会话就断了）"
    );

    let mut second = second;
    second.stop_all(now_ms());
    cleanup_unit("utdelta");
}

/// 组件 unit 必须拿到 `ACB_DATA_DIR`。
///
/// 这是 2026-11-17 线上实测踩到的：`systemd-run` 起的是独立 unit，**不继承**
/// launcher 的环境。少了这个变量，组件里的 `acbctl` 按相对路径 `state/acb.db`
/// + 进程 cwd（`$HOME`）找库，于是打开/新建了一个空库：读不到会话与库内密钥，
/// `acbctl sessions` 返回零个会话 → 驱动一个都不起 → dsh 全部失联，
/// 而脚本不报错（「零个会话」是合法输出）。
#[test]
fn component_unit_receives_data_dir() {
    if !unit_mode_available() {
        eprintln!("skip: systemd-run unavailable");
        return;
    }
    let dir = temp_dir("data-dir");
    let marker = dir.0.join("env");
    let script = script(
        &dir.0,
        "envwatch.sh",
        &format!(
            "echo \"ACB_DATA_DIR=${{ACB_DATA_DIR:-<unset>}}\" > '{}'\nwhile :; do sleep 0.2; done",
            marker.display()
        ),
    );
    let text = format!(
        r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "utepsilon"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
        script.display()
    );

    // 钉住一个可辨认的数据根，再从组件脚本里读回来。
    let saved = std::env::var("ACB_DATA_DIR").ok();
    // SAFETY: 持锁（本文件测试串行跑）——这里只影响本进程派生的 unit。
    unsafe { std::env::set_var("ACB_DATA_DIR", "/data/acb-probe") };

    let mut manager = manager(&dir.0, &text);
    wait_for(&mut manager, Duration::from_secs(10), |manager| {
        manager.phase("utepsilon") == Some(Phase::Healthy)
    });
    thread::sleep(Duration::from_millis(400));
    manager.stop_all(now_ms());
    cleanup_unit("utepsilon");

    match saved {
        Some(value) => unsafe { std::env::set_var("ACB_DATA_DIR", value) },
        None => unsafe { std::env::remove_var("ACB_DATA_DIR") },
    }

    let written = fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        written.contains("ACB_DATA_DIR=/data/acb-probe"),
        "组件 unit 必须收到 ACB_DATA_DIR，实际：{written}"
    );
}
