//! `acbctl run` / `status --mode=run` 黑盒测试。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acbctl-run-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_acb(values: &[(&str, &str)], extra_env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_acbctl"));
    for (key, _) in std::env::vars() {
        if key.starts_with("ACB_UNIT_") {
            command.env_remove(key);
        }
    }
    command.env_remove("ACB_PROFILE_DIR");
    command.env_remove("ACB_STATE_DIR");
    for (key, value) in values {
        command.env(key, value);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.args(args);
    command.output().expect("spawn acbctl")
}

fn launcher_config(root: &Path, state_dir: &Path) -> PathBuf {
    let path = root.join("supervisor.toml");
    fs::write(
        &path,
        format!(
            "[supervisor]\nprogram = \"/bin/true\"\nstate_dir = \"{}\"\n",
            state_dir.display()
        ),
    )
    .expect("write supervisor config");
    path
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    path.exists()
}

fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[test]
fn launcher_75_reloads_pointer_then_78_exits_with_reason() {
    let temp = TempDir::new("75-78");
    let bin = temp.path().join("bin");
    let state = temp.path().join("state");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&state).expect("state");
    let config = launcher_config(temp.path(), &state);
    let count = temp.path().join("spawn-count");
    let pointer = state.join("current.json");
    let env_marker = temp.path().join("child-env");

    let launcher = bin.join("acb-launcher");
    common::write_script(
        &launcher,
        &format!(
            r#"set -euo pipefail
echo "notify=${{NOTIFY_SOCKET:-none}} watchdog=${{WATCHDOG_USEC:-none}}" > {env_marker}
count={count}
n=$(cat "$count" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$count"
if [[ $n -eq 1 ]]; then
  cat > {pointer} <<'JSON'
{{"command":{{"program":"/fake/after-75","args":["x"]}},"updated_at_ms":1,"updated_by":"test"}}
JSON
  echo "fake launcher: rollback requested" >&2
  exit 75
fi
echo "fake launcher: fatal config" >&2
exit 78
"#,
            env_marker = shell_path(&env_marker),
            count = shell_path(&count),
            pointer = shell_path(&pointer),
        ),
    );

    let output = run_acb(
        &[("ACB_UNIT_BIN_DIR", bin.to_str().unwrap())],
        &[
            ("NOTIFY_SOCKET", "/tmp/fake-notify"),
            ("WATCHDOG_USEC", "999"),
        ],
        &[
            "run",
            "--mode=launcher",
            "--config",
            config.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(78), "{}", stderr(&output));
    assert_eq!(
        fs::read_to_string(&env_marker).expect("child env"),
        "notify=none watchdog=none\n"
    );
    let text = stderr(&output);
    assert!(
        text.contains("re-read current.json -> /fake/after-75 x"),
        "{text}"
    );
    assert!(text.contains("fake launcher: fatal config"), "{text}");
    assert_eq!(
        fs::read_to_string(&count).expect("count"),
        "2\n",
        "expected exactly two spawns"
    );
    assert!(!state.join("run.json").exists());
}

#[test]
fn rate_limit_stops_after_three_failures() {
    let temp = TempDir::new("rate-limit");
    let bin = temp.path().join("bin");
    let state = temp.path().join("state");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&state).expect("state");
    let config = launcher_config(temp.path(), &state);
    let count = temp.path().join("spawn-count");

    common::write_script(
        &bin.join("acb-launcher"),
        &format!(
            r#"set -euo pipefail
count={count}
n=$(cat "$count" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$count"
echo "fake launcher: failing" >&2
exit 75
"#,
            count = shell_path(&count),
        ),
    );

    let output = run_acb(
        &[("ACB_UNIT_BIN_DIR", bin.to_str().unwrap())],
        &[],
        &[
            "run",
            "--mode=launcher",
            "--config",
            config.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(78), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("已触发限流"),
        "{}",
        stderr(&output)
    );
    assert_eq!(fs::read_to_string(&count).expect("count"), "3\n");
}

#[test]
fn sigterm_forwards_to_child_and_reaps() {
    let temp = TempDir::new("sigterm");
    let bin = temp.path().join("bin");
    let state = temp.path().join("state");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&state).expect("state");
    let config = launcher_config(temp.path(), &state);
    let marker = temp.path().join("child-term");
    let child_pid_file = temp.path().join("child-pid");

    common::write_script(
        &bin.join("acb-launcher"),
        &format!(
            r#"set -euo pipefail
echo $$ > {pid_file}
trap 'echo term > {marker}; exit 0' TERM INT
while :; do sleep 0.05; done
"#,
            pid_file = shell_path(&child_pid_file),
            marker = shell_path(&marker),
        ),
    );

    let mut parent = Command::new(env!("CARGO_BIN_EXE_acbctl"));
    for (key, _) in std::env::vars() {
        if key.starts_with("ACB_UNIT_") {
            parent.env_remove(key);
        }
    }
    parent.env_remove("ACB_PROFILE_DIR");
    parent.env_remove("ACB_STATE_DIR");
    parent
        .env("ACB_UNIT_BIN_DIR", &bin)
        .args([
            "run",
            "--mode=launcher",
            "--config",
            config.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let parent_child = parent.spawn().expect("spawn acbctl run");
    let parent_pid = parent_child.id();

    assert!(
        wait_for(&state.join("run.json"), Duration::from_secs(5)),
        "run.json was not written"
    );
    assert!(
        wait_for(&child_pid_file, Duration::from_secs(5)),
        "fake launcher did not start"
    );
    let child_pid: u32 = fs::read_to_string(&child_pid_file)
        .expect("child pid")
        .trim()
        .parse()
        .expect("parse child pid");

    unsafe {
        libc::kill(parent_pid as libc::pid_t, libc::SIGTERM);
    }

    let output = parent_child.wait_with_output().expect("wait acbctl run");
    assert!(
        output.status.success(),
        "parent did not exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(marker.exists(), "child did not receive SIGTERM");
    assert!(!pid_alive(child_pid), "child was left as an orphan");
    assert!(!state.join("run.json").exists());
}

#[test]
fn status_run_ignores_failing_systemctl_shim() {
    let temp = TempDir::new("status");
    let state = temp.path().join("state");
    let shim = temp.path().join("shim");
    fs::create_dir_all(&state).expect("state");
    fs::create_dir_all(&shim).expect("shim");

    let run_json = serde_json::json!({
        "parent_pid": std::process::id(),
        "child_pid": std::process::id(),
        "child_exe": "/fake/acb-launcher",
        "args": ["--config", "/fake/supervisor.toml"],
        "started_at": 1,
        "config_sha": "abc",
        "mode": "launcher",
        "selected": {"program": "/fake/acb-bridge", "args": []},
        "state_dir": state.to_str().unwrap(),
    });
    fs::write(
        state.join("run.json"),
        serde_json::to_string_pretty(&run_json).unwrap(),
    )
    .expect("run.json");
    fs::write(
        state.join("current.json"),
        r#"{"command":{"program":"/fake/current","args":[]}}"#,
    )
    .expect("current.json");
    fs::write(
        state.join("lkg.json"),
        r#"{"command":{"program":"/fake/lkg","args":[]}}"#,
    )
    .expect("lkg.json");

    common::write_script(
        &shim.join("systemctl"),
        "echo 'systemctl must not be called' >&2\nexit 42\n",
    );
    let path = format!(
        "{}:{}",
        shim.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = run_acb(
        &[],
        &[("PATH", &path)],
        &[
            "status",
            "--mode=run",
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("run: active"), "{text}");
    assert!(text.contains("current: /fake/current"), "{text}");
    assert!(text.contains("lkg: /fake/lkg"), "{text}");
    assert!(text.contains("health: skipped"), "{text}");
}

#[test]
fn status_run_reports_stale_run_json() {
    let temp = TempDir::new("status-stale");
    let state = temp.path().join("state");
    fs::create_dir_all(&state).expect("state");
    let run_json = serde_json::json!({
        "parent_pid": 999_999u32,
        "child_pid": 999_999u32,
        "child_exe": "/fake/acb-launcher",
        "args": [],
        "started_at": 1,
        "config_sha": "abc",
        "mode": "launcher",
        "selected": {"program": "/fake/acb-bridge", "args": []},
        "state_dir": state.to_str().unwrap(),
    });
    fs::write(
        state.join("run.json"),
        serde_json::to_string_pretty(&run_json).unwrap(),
    )
    .expect("run.json");

    let output = run_acb(
        &[],
        &[],
        &[
            "status",
            "--mode=run",
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("run: stale"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn launcher_mode_renders_supervisor_toml_from_acb_unit_values() {
    let temp = TempDir::new("render-supervisor");
    let bin = temp.path().join("bin");
    let state = temp.path().join("state");
    fs::create_dir_all(&bin).expect("bin");
    let repo = temp.path().join("repo");
    let config_file = repo.join("config.toml");
    fs::create_dir_all(&repo).expect("repo");
    fs::write(&config_file, "[endpoint]\n").expect("config");

    common::write_script(&bin.join("acb-bridge"), "exit 0\n");
    common::write_script(
        &bin.join("acb-launcher"),
        "echo 'fake launcher: clean' >&2\nexit 0\n",
    );

    let log_dir = repo.join("logs");
    let env_file = repo.join("deploy/acb.env");
    let opencode_dir = repo.join("state/opencode");
    let values = [
        ("ACB_UNIT_REPO_DIR", repo.to_str().unwrap()),
        ("ACB_UNIT_LOG_DIR", log_dir.to_str().unwrap()),
        ("ACB_UNIT_PROFILE_DIR", "/etc/profiles/per-user/test"),
        ("ACB_UNIT_BIN_DIR", bin.to_str().unwrap()),
        ("ACB_UNIT_CONFIG_FILE", config_file.to_str().unwrap()),
        ("ACB_UNIT_ENV_FILE", env_file.to_str().unwrap()),
        ("ACB_UNIT_OPENCODE_DIR", opencode_dir.to_str().unwrap()),
        ("ACB_UNIT_CONSOLE_PORT", "3100"),
        ("ACB_UNIT_OPENCODE_PORT", "4097"),
        ("ACB_UNIT_INSTALL_SECTION", "0"),
    ];

    let output = run_acb(
        &values,
        &[],
        &[
            "run",
            "--mode=launcher",
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let config = fs::read_to_string(state.join("supervisor.toml")).expect("supervisor.toml");
    assert!(
        config.contains(&format!("program = \"{}/acb-bridge\"", bin.display())),
        "{config}"
    );
    assert!(config.contains("health_url = \"http://127.0.0.1:3100/healthz\""));
    assert!(config.contains(&format!("state_dir = \"{}\"", state.display())));
}

#[test]
fn direct_mode_spawns_bridge() {
    let temp = TempDir::new("direct");
    let bin = temp.path().join("bin");
    let state = temp.path().join("state");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&state).expect("state");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let marker = temp.path().join("bridge-started");

    common::write_script(
        &bin.join("acb-bridge"),
        &format!("echo started > {}\nexit 0\n", shell_path(&marker)),
    );

    let output = run_acb(
        &[("ACB_UNIT_BIN_DIR", bin.to_str().unwrap())],
        &[],
        &[
            "run",
            "--mode=direct",
            "--config",
            config.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(marker.exists(), "bridge was not spawned");
    assert!(!state.join("run.json").exists());
}

fn shell_path(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "'\\''")
}
