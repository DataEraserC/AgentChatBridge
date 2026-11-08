//! P2 第二半：真实 spawn 的黑盒测试。全部用假组件脚本，不依赖现场服务。

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
        if std::env::var_os("ACB_KEEP_TEMP").is_none() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn temp_dir(label: &str) -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "acb-component-runtime-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    TempDir(path)
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    let mut permissions = fs::metadata(&path).expect("stat script").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod script");
    path
}

fn state_dir(dir: &Path) -> PathBuf {
    dir.join("state").join("supervisor")
}

fn manager(dir: &Path, text: &str) -> ComponentManager {
    let (_file, table) =
        acb_supervisor::component::parse_config_file(text, &PresetRegistry::empty())
            .expect("parse config");
    ComponentManager::new(&state_dir(dir), &table, "test-sha", Some("inst-test"), now_ms())
        .expect("create manager")
}

fn wait_for(
    manager: &mut ComponentManager,
    timeout: Duration,
    mut predicate: impl FnMut(&ComponentManager) -> bool,
) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        manager.tick(now_ms());
        if predicate(manager) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("condition not reached within {:?}", timeout);
}

fn lines(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

fn read_state(dir: &Path, name: &str) -> ComponentState {
    ComponentState::load(&state_dir(dir), name)
        .expect("read state")
        .expect("state exists")
}

#[test]
fn disabled_component_is_never_spawned() {
    let dir = temp_dir("disabled");
    let marker = dir.0.join("spawned");
    let script = script(
        &dir.0,
        "marker.sh",
        &format!("echo ran >> '{}'\nexit 0", marker.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
command = ["{}"]
[component.readiness]
kind = "process"
"#,
            script.display()
        ),
    );
    assert_eq!(manager.enabled_count(), 0, "enabled 默认必须是 false");
    for _ in 0..20 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(25));
    }
    assert!(!marker.exists(), "disabled 组件不得 spawn");
    assert_eq!(manager.phase("alpha"), Some(Phase::Stopped));
    let state = read_state(&dir.0, "alpha");
    assert!(!state.enabled);
    assert_eq!(state.phase, "stopped");
    assert_eq!(state.pid, None);
  }

#[test]
fn simple_component_starts_and_stops() {
    let dir = temp_dir("simple-start");
    let marker = dir.0.join("started");
    let script = script(
        &dir.0,
        "stay.sh",
        &format!(
            "echo started >> '{}'\nwhile :; do sleep 0.2; done",
            marker.display()
        ),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
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

    wait_for(&mut manager, Duration::from_secs(3), |manager| {
        manager.phase("alpha") == Some(Phase::Healthy)
    });
    // process 探针只看子进程是否存活；给脚本一点时间真正执行第一行。
    thread::sleep(Duration::from_millis(250));
    let component_log = fs::read_to_string(dir.0.join("state/logs/alpha.log")).unwrap_or_default();
    assert!(marker.exists(), "组件脚本必须真的被执行；log:\n{component_log}");
    assert!(manager.pid("alpha").is_some(), "存活组件必须有 pid");
    assert!(read_state(&dir.0, "alpha").log_path.is_some());

    manager.stop_all(now_ms());
    assert_eq!(manager.phase("alpha"), Some(Phase::Stopped));
    assert_eq!(manager.pid("alpha"), None);
    let state = read_state(&dir.0, "alpha");
    assert_eq!(state.phase, "stopped");
    assert_eq!(state.pid, None);
}

#[test]
fn http_probe_marks_component_healthy() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let dir = temp_dir("http-probe");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake http");
    let port = listener.local_addr().expect("addr").port();
    let server = thread::spawn(move || {
        if let Ok((mut socket, _)) = listener.accept() {
            let mut request = [0u8; 256];
            let _ = socket.read(&mut request);
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    let script = script(
        &dir.0,
        "stay.sh",
        "while :; do sleep 0.2; done",
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
enabled = true
command = ["{}"]
[component.readiness]
kind = "http"
host = "127.0.0.1"
port = {}
path = "/healthz"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
            script.display(),
            port
        ),
    );

    wait_for(&mut manager, Duration::from_secs(3), |manager| {
        manager.phase("alpha") == Some(Phase::Healthy)
    });
    assert!(read_state(&dir.0, "alpha")
        .probe_url
        .as_deref()
        .is_some_and(|url| url.ends_with("/healthz")));
    manager.stop_all(now_ms());
    server.join().ok();
}

#[test]
fn non_active_exit_zero_becomes_stopped_early_without_restart() {
    let dir = temp_dir("stopped-early");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "once.sh",
        &format!("echo run >> '{}'\nexit 0", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
enabled = true
command = ["{}"]
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(2), |manager| {
        manager.phase("alpha") == Some(Phase::StoppedEarly)
    });
    assert_eq!(lines(&counter), 1, "exit 0 非主动退出只执行一次");
    for _ in 0..20 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines(&counter), 1, "stopped_early 不得被拉起");
    assert_eq!(read_state(&dir.0, "alpha").phase, "stopped_early");
}

#[test]
fn non_zero_exit_restarts_per_policy() {
    let dir = temp_dir("restart");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "fail.sh",
        &format!("echo run >> '{}'\nexit 1", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
enabled = true
command = ["{}"]
[component.readiness]
kind = "tcp"
port = 1
interval_secs = 5
failures_threshold = 10
startup_grace_secs = 0
[component.restart]
policy = "on-failure"
max_attempts = 5
backoff_secs = [0]
backoff_cap_secs = 0
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(3), |_| lines(&counter) >= 2);
    assert!(
        matches!(
            manager.phase("alpha"),
            Some(Phase::Starting | Phase::Restarting | Phase::Unhealthy)
        ),
        "非零退出后必须仍在重启路径上，实际 {:?}",
        manager.phase("alpha")
    );
    manager.stop_all(now_ms());
    assert_eq!(manager.phase("alpha"), Some(Phase::Stopped));
}

#[test]
fn hitting_max_attempts_enters_terminal_failed() {
    let dir = temp_dir("failed");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "fail.sh",
        &format!("echo run >> '{}'\nexit 1", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
enabled = true
command = ["{}"]
[component.readiness]
kind = "tcp"
port = 1
interval_secs = 5
failures_threshold = 10
startup_grace_secs = 0
[component.restart]
policy = "on-failure"
max_attempts = 2
backoff_secs = [0]
backoff_cap_secs = 0
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(3), |manager| {
        manager.phase("alpha") == Some(Phase::Failed)
    });
    assert_eq!(lines(&counter), 2, "max_attempts=2 表示总共只拉起两次");
    let failed = read_state(&dir.0, "alpha");
    assert_eq!(failed.phase, "failed");
    assert_eq!(failed.pid, None);
    assert!(failed.failed_at_ms.is_some());

    for _ in 0..10 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines(&counter), 2, "failed 是终态，之后不再拉起");
    manager.stop_all(now_ms());
    assert_eq!(manager.phase("alpha"), Some(Phase::Failed));
}

#[test]
fn active_stop_does_not_restart() {
    let dir = temp_dir("active-stop");
    let counter = dir.0.join("stops");
    let script = script(
        &dir.0,
        "stay.sh",
        &format!(
            "trap 'echo stopped >> \"{}\"; exit 0' TERM\nwhile :; do sleep 0.2; done",
            counter.display()
        ),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "alpha"
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

    wait_for(&mut manager, Duration::from_secs(3), |manager| {
        manager.phase("alpha") == Some(Phase::Healthy)
    });
    // 让脚本完成 trap 安装，否则 TERM 会赶在 trap 之前到达。
    thread::sleep(Duration::from_millis(250));
    manager.stop_all(now_ms());
    assert_eq!(manager.phase("alpha"), Some(Phase::Stopped));
    assert_eq!(lines(&counter), 1, "SIGTERM 应让脚本走 trap");

    for _ in 0..20 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(manager.phase("alpha"), Some(Phase::Stopped));
    assert_eq!(lines(&counter), 1, "主动停止不得触发重启");
}

#[test]
fn oneshot_probe_failure_reruns_within_max_attempts() {
    let dir = temp_dir("oneshot");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "up.sh",
        &format!("echo run >> '{}'\nexit 0", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "snow"
enabled = true
type = "oneshot"
command = ["{}"]
file = "/tmp/acb-test-compose.yml"
compose_bin = ["{}"]
[component.readiness]
kind = "tcp"
port = 1
interval_secs = 1
failures_threshold = 1
success_threshold = 1
startup_grace_secs = 0
[component.restart]
policy = "on-failure"
max_attempts = 5
backoff_secs = [0]
backoff_cap_secs = 0
"#,
            script.display(),
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(4), |_| lines(&counter) >= 2);
    assert!(
        matches!(
            manager.phase("snow"),
            Some(Phase::Starting | Phase::Restarting | Phase::Unhealthy | Phase::Healthy)
        ),
        "探针失败后 oneshot 必须重跑，实际 {:?}",
        manager.phase("snow")
    );
    manager.stop_all(now_ms());
    assert_eq!(manager.phase("snow"), Some(Phase::Stopped));
}

#[test]
fn oneshot_exit_zero_becomes_healthy() {
    let dir = temp_dir("oneshot-exit");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "once.sh",
        &format!("echo run >> '{}'\nexit 0", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "sync"
enabled = true
type = "oneshot"
command = ["{}"]
[component.readiness]
kind = "exit"
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(2), |manager| {
        manager.phase("sync") == Some(Phase::Healthy)
    });
    assert_eq!(lines(&counter), 1);
    let state = read_state(&dir.0, "sync");
    assert_eq!(state.phase, "healthy");
    assert_eq!(state.attempts, 0, "exit 0 成功必须清掉失败计数");
    assert_eq!(state.last_result.as_deref(), Some("succeeded"));
    assert_eq!(state.probe_url, None, "exit 只看退出码，没有探针端点");

    for _ in 0..20 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines(&counter), 1, "没有依赖事件不得重跑");
}

#[test]
fn oneshot_exit_non_zero_counts_attempts_until_failed() {
    let dir = temp_dir("oneshot-exit-fail");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "fail.sh",
        &format!("echo run >> '{}'\nexit 3", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "sync"
enabled = true
type = "oneshot"
command = ["{}"]
[component.readiness]
kind = "exit"
[component.restart]
policy = "on-failure"
max_attempts = 2
backoff_secs = [0]
backoff_cap_secs = 0
"#,
            script.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(3), |manager| {
        manager.phase("sync") == Some(Phase::Failed)
    });
    assert_eq!(lines(&counter), 2, "max_attempts=2 表示总共只跑两次");
    let state = read_state(&dir.0, "sync");
    assert_eq!(state.phase, "failed");
    assert!(state.failed_at_ms.is_some());
    assert!(state.attempts > 0);

    for _ in 0..10 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lines(&counter), 2, "failed 终态不再重跑");
}

#[test]
fn oneshot_reruns_once_after_dependency_becomes_ready_again() {
    let dir = temp_dir("oneshot-rerun");
    let sync_runs = dir.0.join("sync-runs");
    let dep_runs = dir.0.join("dep-runs");
    let trigger = dir.0.join("trigger");
    let sync = script(
        &dir.0,
        "sync.sh",
        &format!("echo run >> '{}'\nexit 0", sync_runs.display()),
    );
    let dep = script(
        &dir.0,
        "dep.sh",
        &format!(
            "echo run >> '{}'\nwhile [ ! -f '{}' ]; do sleep 0.2; done\nrm -f '{}'\nexit 1",
            dep_runs.display(),
            trigger.display(),
            trigger.display()
        ),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "sync"
enabled = true
type = "oneshot"
command = ["{}"]
depends.after = ["dep"]
[component.readiness]
kind = "exit"

[[component]]
name = "dep"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
[component.restart]
policy = "on-failure"
max_attempts = 10
backoff_secs = [0]
backoff_cap_secs = 0
"#,
            sync.display(),
            dep.display()
        ),
    );

    // 依赖先就绪，oneshot 才跑第一次。
    wait_for(&mut manager, Duration::from_secs(5), |manager| {
        manager.phase("dep") == Some(Phase::Healthy)
            && manager.phase("sync") == Some(Phase::Healthy)
    });
    assert_eq!(lines(&sync_runs), 1);

    // 依赖被杀 → 自动重启 → 再次就绪；oneshot 必须恰好重跑一次。
    fs::write(&trigger, b"").expect("trigger dep exit");
    wait_for(&mut manager, Duration::from_secs(8), |_| lines(&sync_runs) >= 2);
    assert_eq!(manager.phase("dep"), Some(Phase::Healthy));
    assert_eq!(lines(&dep_runs), 2);

    for _ in 0..40 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        lines(&sync_runs),
        2,
        "同一代就绪只触发一次重跑，不得刷重试"
    );
    manager.stop_all(now_ms());
}

#[test]
fn oneshot_stays_blocked_while_dependency_never_ready() {
    let dir = temp_dir("oneshot-blocked");
    let counter = dir.0.join("runs");
    let script = script(
        &dir.0,
        "once.sh",
        &format!("echo run >> '{}'\nexit 0", counter.display()),
    );
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "sync"
enabled = true
type = "oneshot"
command = ["{}"]
depends.after = ["dep"]
[component.readiness]
kind = "exit"

[[component]]
name = "dep"
enabled = false
command = ["/bin/true"]
[component.readiness]
kind = "process"
"#,
            script.display()
        ),
    );

    for _ in 0..40 {
        manager.tick(now_ms());
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(manager.phase("sync"), Some(Phase::Blocked));
    assert_eq!(lines(&counter), 0, "依赖未就绪不得启动");
    let state = read_state(&dir.0, "sync");
    assert_eq!(state.phase, "blocked");
    assert_eq!(state.attempts, 0, "等待期间不累计 attempts，不刷重试");
}

#[test]
fn stop_order_is_reverse_dependency_order() {
    let dir = temp_dir("order");
    let order = dir.0.join("order");
    let script_a = script(
        &dir.0,
        "alpha.sh",
        &format!(
            "trap 'echo alpha >> \"{}\"; exit 0' TERM\nwhile :; do sleep 0.2; done",
            order.display()
        ),
    );
    let script_b = script(
        &dir.0,
        "beta.sh",
        &format!(
            "trap 'echo beta >> \"{}\"; exit 0' TERM\nwhile :; do sleep 0.2; done",
            order.display()
        ),
    );
    // 故意把依赖方写在前面，验证拓扑排序而不是声明顺序。
    let mut manager = manager(
        &dir.0,
        &format!(
            r#"
[supervisor]
program = "/bin/true"

[[component]]
name = "beta"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
depends.requires = ["alpha"]
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0

[[component]]
name = "alpha"
enabled = true
command = ["{}"]
stop_timeout_secs = 1
[component.readiness]
kind = "process"
interval_secs = 1
success_threshold = 1
startup_grace_secs = 0
"#,
            script_b.display(),
            script_a.display()
        ),
    );

    wait_for(&mut manager, Duration::from_secs(3), |manager| {
        manager.pid("alpha").is_some() && manager.pid("beta").is_some()
    });
    // 等两个脚本都装好 TERM trap。
    thread::sleep(Duration::from_millis(250));
    manager.stop_all(now_ms());

    let text = fs::read_to_string(&order).expect("order file");
    let entries: Vec<&str> = text.lines().collect();
    assert_eq!(entries, vec!["beta", "alpha"], "停止必须逆依赖序");
}
