//! 常驻组件的**停机语义**：预设脚本收到 SIGTERM 后必须自己退出。
//!
//! 回归 2026-11-25（线上实测）：`drivers` / `acp` 两个对账脚本的 `cleanup()` 清掉
//! trap 后直接返回，bash 于是回到主循环继续 `reconcile`——刚被停掉的驱动被重新
//! 拉起，unit 永远不退出，systemd 只能等满 `TimeoutStopSec`（默认 90 秒）再
//! SIGKILL。整机关机因此多花约 80 秒：
//!
//! ```text
//! acb-component-drivers.service: State 'stop-sigterm' timed out. Killing.
//! ```
//!
//! 这里的断言只看「脚本自己会不会退」，不碰 systemd：两条脚本在 TERM 后必须
//! 在几秒内退出，且它拉起的、**故意忽略 TERM** 的驱动必须被 KILL 掉。旧实现两条
//! 都不满足（trap 返回后继续循环，`wait` 还会永远阻塞）。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
const DRIVERS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/presets/drivers/drivers_acb.sh"
);
const ACP: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../deploy/presets/acp/acp_driver_acb.sh"
);

/// 脚本必须在 TERM 后这么久内退出。旧实现是「永不退出」，新实现上界约
/// 一个轮询间隔（1s）+ cleanup 的 3s 有界等待。
const STOP_DEADLINE: Duration = Duration::from_secs(8);

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(label: &str) -> TempDir {
    let base = std::env::temp_dir().join(format!(
        "acb-preset-stop-{label}-{}-{}",
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
    // `#!/usr/bin/env bash`：NixOS 没有 /bin/bash。
    fs::write(&path, format!("#!/usr/bin/env bash\n{body}\n")).expect("write script");
    let mut permissions = fs::metadata(&path).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod");
    path
}

/// 假的 `acbctl`：只回答脚本真正用到的两条子命令。
fn stub_acbctl(dir: &Path) -> PathBuf {
    script(
        dir,
        "acbctl",
        r#"case "${1:-}" in
  sessions) printf '%s\t%s\t%s\t%s\n' "${STUB_SID}" "${STUB_HARNESS}" preset 0 ;;
  secrets) printf 'token\n' ;;
esac
exit 0"#,
    )
}

/// 故意忽略 TERM 的驱动：只有 `kill -9` 能收掉它。
///
/// 这正是最坏情况的驱动行为——脚本要么有界等待后 KILL，要么永远卡在 `wait`。
fn stubborn_driver(dir: &Path, pidfile: &Path) -> PathBuf {
    script(
        dir,
        "driver.sh",
        &format!(
            r#"trap '' TERM INT
echo $$ > '{}'
while :; do sleep 1; done"#,
            pidfile.display()
        ),
    )
}

fn wait_for_pid(pidfile: &Path, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(pidfile) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return Some(pid);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    None
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn term(child: &Child) {
    let _ = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status();
}

/// 断言失败时别把自己的假驱动留在机器上：脚本跑在自己的进程组里，整组 KILL。
fn kill_group(child: &Child) {
    let _ = Command::new("kill")
        // `--` 之后的 `-<pgid>` 才是「整个进程组」，否则会被当成选项。
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .status();
}

/// 起脚本 → 等驱动真的起来 → 发 TERM → 断言脚本与驱动都退出。
fn assert_stops(label: &str, script_path: &str, env: Vec<(String, String)>, pidfile: &Path) {
    let dir = temp_dir(label);
    assert!(
        Path::new(script_path).is_file(),
        "找不到预设脚本：{script_path}（相对路径以 {REPO} 为根）"
    );

    let log_path = dir.0.join("script.log");
    let log = fs::File::create(&log_path).expect("create log");
    let log_err = log.try_clone().expect("clone log");
    let mut child = Command::new("bash")
        .arg(script_path)
        .envs(env)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        // 独立进程组：失败路径可以整组回收，不把假驱动留在开发机上。
        .process_group(0)
        .spawn()
        .expect("spawn preset script");

    let driver_pid = match wait_for_pid(pidfile, Duration::from_secs(10)) {
        Some(pid) => pid,
        None => {
            kill_group(&child);
            let _ = child.wait();
            let output = fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "{label}: 脚本在 10 秒内没有拉起驱动（pid 文件 {}）\n--- 脚本输出 ---\n{output}",
                pidfile.display()
            );
        }
    };

    let started = Instant::now();
    term(&child);
    loop {
        match child.try_wait().expect("try_wait") {
            Some(_) => break,
            None if started.elapsed() > STOP_DEADLINE => {
                kill_group(&child);
                let _ = child.wait();
                panic!(
                    "{label}: 收到 TERM 后 {} 秒仍未退出——cleanup 没有 exit，主循环把\
                     驱动重新拉起来了（2026-11-25 关机慢 90 秒的根因）",
                    STOP_DEADLINE.as_secs()
                );
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    }

    assert!(
        !alive(driver_pid),
        "{label}: 脚本退出了，但忽略 TERM 的驱动 {driver_pid} 还活着（cleanup 没有兜底 KILL）"
    );
}

fn base_env(dir: &Path, acbctl: &Path) -> Vec<(String, String)> {
    let config = dir.join("config.toml");
    fs::write(&config, "").expect("write config");
    vec![
        ("ACB_DATA".into(), dir.display().to_string()),
        ("ACB_CONFIG_FILE".into(), config.display().to_string()),
        ("ACBCTL_BIN".into(), acbctl.display().to_string()),
        ("ACB_NODE_BIN".into(), "/bin/sh".into()),
        ("STUB_SID".into(), "sess-1".into()),
        // 不跑真实的 preset 安装器：它在测试里只会制造噪音。
        (
            "ACB_PRESETS_INSTALLER".into(),
            dir.join("no-such-installer").display().to_string(),
        ),
    ]
}

#[test]
fn drivers_preset_exits_on_sigterm() {
    let dir = temp_dir("drivers-setup");
    let acbctl = stub_acbctl(&dir.0);
    let driver = stubborn_driver(&dir.0, &dir.0.join("driver.pid"));
    let mut env = base_env(&dir.0, &acbctl);
    env.push(("STUB_HARNESS".into(), "dsh".into()));
    env.push(("ACB_DRIVER_SCRIPT".into(), driver.display().to_string()));
    env.push(("ACB_DRIVERS_INTERVAL_SECS".into(), "1".into()));
    env.push((
        "ACB_DRIVERS_STATE_DIR".into(),
        dir.0.join("state").display().to_string(),
    ));

    let pidfile = dir.0.join("driver.pid");
    assert_stops("drivers", DRIVERS, env, &pidfile);
}

#[test]
fn acp_preset_exits_on_sigterm() {
    let dir = temp_dir("acp-setup");
    let acbctl = stub_acbctl(&dir.0);
    let driver = stubborn_driver(&dir.0, &dir.0.join("driver.pid"));
    let mut env = base_env(&dir.0, &acbctl);
    env.push(("STUB_HARNESS".into(), "acp".into()));
    // acp 用 ACB_ACP_DRIVER_SCRIPT 覆盖；只给 ACB_DRIVER_SCRIPT 会被推导成
    // `<dir>/../../acp/src/driver.mjs`，脚本直接以「找不到驱动」退出。
    env.push(("ACB_ACP_DRIVER_SCRIPT".into(), driver.display().to_string()));
    env.push(("ACB_ACP_HARNESSES".into(), "acp".into()));
    env.push(("ACB_ACP_INTERVAL_SECS".into(), "1".into()));
    env.push((
        "ACB_ACP_STATE_DIR".into(),
        dir.0.join("state").display().to_string(),
    ));

    let pidfile = dir.0.join("driver.pid");
    assert_stops("acp", ACP, env, &pidfile);
}
