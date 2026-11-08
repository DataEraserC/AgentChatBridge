//! `ProcessRunner` 停机必须收掉整个 bridge 进程组：bridge 的插件子进程
//! （issue-tools / notifier 等）和 bridge 同组，只看 leader 会留下孤儿。

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use acb_supervisor::{CommandSpec, ProcessRunner, Runner};

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "acb-runner-stop-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn wait_for_pid(path: &PathBuf) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "child pid file not written");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|text| {
            text.rfind(')')
                .and_then(|end| text.get(end + 1..))
                .and_then(|rest| rest.split_whitespace().next())
                .map(|state| state != "Z")
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

#[test]
fn stop_terminates_whole_bridge_group() {
    let dir = temp_dir("graceful");
    let pid_file = dir.join("child.pid");
    let mut runner = ProcessRunner::default();
    runner
        .start(&CommandSpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("sleep 300 & echo $! > {}; wait", pid_file.display()),
            ],
            generation: None,
        })
        .expect("start bridge");
    let grandchild = wait_for_pid(&pid_file);

    runner.stop().expect("stop bridge");

    assert!(!alive(grandchild), "插件子进程 {grandchild} 必须随组退出");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stop_within_force_kills_stubborn_group() {
    let dir = temp_dir("stubborn");
    let pid_file = dir.join("child.pid");
    let mut runner = ProcessRunner::default();
    runner
        .start(&CommandSpec {
            program: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!(
                    "trap '' TERM; sleep 300 & echo $! > {}; wait",
                    pid_file.display()
                ),
            ],
            generation: None,
        })
        .expect("start bridge");
    // 父 shell 与孙进程都忽略 TERM，只能靠 deadline 后的 SIGKILL 收敛。
    let grandchild = wait_for_pid(&pid_file);

    let started = Instant::now();
    runner
        .stop_within(Instant::now() + Duration::from_millis(300))
        .expect("stop bridge");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "停机必须受 deadline 约束"
    );
    assert!(!alive(grandchild), "忽略 TERM 的子进程必须被 SIGKILL");

    // 清理测试目录；进程组已在 stop 中收掉。
    let _ = fs::remove_dir_all(&dir);
}
