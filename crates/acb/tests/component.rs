//! `acbctl component list/status` 黑盒测试：只读、不伪造进程态。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acbctl-component-{label}-{}-{sequence}",
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

fn write_config(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("supervisor.toml");
    fs::write(&path, text).expect("write config");
    path
}

fn run_acb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_acbctl"))
        .args(args)
        .env_remove("ACB_STATE_DIR")
        .env_remove("ACB_UNIT_CONFIG_FILE")
        .env_remove("ACB_PROFILE_DIR")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run acbctl")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const CONFIG: &str = "
[supervisor]
program = \"/bin/acb-bridge\"

[[component]]
name = \"dsh\"
command = [\"/bin/dsh\"]
enabled = true
[component.readiness]
kind = \"tcp\"
port = 3080

[[component]]
name = \"opencode\"
command = [\"/bin/opencode\"]
[component.readiness]
kind = \"tcp\"
port = 4097
";

#[test]
fn component_list_shows_declarations_without_process_state() {
    let dir = TempDir::new("list");
    let config = write_config(dir.path(), CONFIG);
    let output = run_acb(&[
        "component",
        "list",
        "--config",
        config.to_str().expect("path"),
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("components: 2 declared (1 enabled)"), "{text}");
    assert!(text.contains("dsh"), "{text}");
    assert!(text.contains("opencode"), "{text}");
    assert!(text.contains("tcp 127.0.0.1:3080"), "{text}");
    assert!(!text.contains("running"), "list 不伪造进程态: {text}");
}

#[test]
fn component_status_reports_not_started_without_state_file() {
    let dir = TempDir::new("status");
    let config = write_config(dir.path(), CONFIG);
    let output = run_acb(&[
        "component",
        "status",
        "dsh",
        "--config",
        config.to_str().expect("path"),
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("component: dsh"), "{text}");
    assert!(text.contains("enabled: true"), "{text}");
    assert!(text.contains("readiness: tcp 127.0.0.1:3080"), "{text}");
    assert!(text.contains("restart: on-failure"), "{text}");
    assert!(text.contains("process: not started (no state file)"), "{text}");
}

#[test]
fn component_status_reads_persisted_state() {
    let dir = TempDir::new("state");
    let config = format!(
        "
[supervisor]
program = \"/bin/acb-bridge\"
state_dir = \"{}/state/supervisor\"

[[component]]
name = \"dsh\"
command = [\"/bin/dsh\"]
enabled = true
[component.readiness]
kind = \"tcp\"
port = 3080
",
        dir.path().display()
    );
    let config = write_config(dir.path(), &config);
    let components = dir.path().join("state").join("components");
    fs::create_dir_all(&components).unwrap();
    fs::write(
        components.join("dsh.json"),
        r#"{
  "schema": 1,
  "name": "dsh",
  "phase": "stopped",
  "restart_policy": "on-failure",
  "enabled": true,
  "attempts": 2,
  "consecutive_failures": 1,
  "restarts": 0,
  "instance_id": "inst-live",
  "config_sha256": "deadbeef",
  "last_run_at_ms": 123,
  "last_result": "ok",
  "updated_at_ms": 456
}
"#,
    )
    .unwrap();

    let output = run_acb(&[
        "component",
        "status",
        "dsh",
        "--config",
        config.to_str().expect("path"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("process: stopped"), "{text}");
    assert!(text.contains("instance_id=inst-live"), "{text}");
    assert!(text.contains("config_sha256=deadbeef"), "{text}");
    assert!(text.contains("consecutive_failures=1"), "{text}");
    assert!(text.contains("last_run_at_ms=123"), "{text}");
    assert!(text.contains("last_result=ok"), "{text}");
}

#[test]
fn component_status_all_entries_shows_state_summary() {
    let dir = TempDir::new("status-all");
    let config = write_config(dir.path(), CONFIG);
    let output = run_acb(&[
        "component",
        "status",
        "--config",
        config.to_str().expect("path"),
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("dsh: declared enabled=true"), "{text}");
    assert!(
        text.contains("opencode: declared enabled=false"),
        "{text}"
    );
    assert!(text.contains("phase=not-started (no state file)"), "{text}");
}

const STATE_CONFIG: &str = "
[supervisor]
program = \"/bin/acb-bridge\"
state_dir = \"{dir}/state/supervisor\"

[[component]]
name = \"dsh\"
command = [\"/bin/dsh\"]
enabled = true
[component.readiness]
kind = \"tcp\"
port = 3080
";

fn write_failed_state(dir: &Path) {
    let components = dir.join("state").join("components");
    fs::create_dir_all(&components).unwrap();
    fs::write(
        components.join("dsh.json"),
        r#"{
  "schema": 1,
  "name": "dsh",
  "phase": "failed",
  "restart_policy": "on-failure",
  "enabled": true,
  "attempts": 4,
  "consecutive_failures": 4,
  "restarts": 3,
  "config_sha256": "sha",
  "last_error": "exit status 1",
  "ready_at_ms": 100,
  "failed_at_ms": 200,
  "last_result": "failed",
  "updated_at_ms": 300
}
"#,
    )
    .unwrap();
}

#[test]
fn component_reset_clears_failed_state_only() {
    let dir = TempDir::new("reset");
    let config = write_config(
        dir.path(),
        &STATE_CONFIG.replace("{dir}", &dir.path().display().to_string()),
    );
    write_failed_state(dir.path());

    let output = run_acb(&[
        "component",
        "reset",
        "dsh",
        "--config",
        config.to_str().expect("path"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("reset: dsh"), "{text}");
    assert!(text.contains("phase=failed"), "要说明重置前状态: {text}");
    assert!(text.contains("attempts 4 -> 0"), "{text}");
    assert!(
        text.contains("consecutive_failures 4 -> 0"),
        "{text}"
    );
    assert!(text.contains("restarts 3 -> 0"), "{text}");
    assert!(text.contains("after: phase=stopped"), "{text}");

    let saved = fs::read_to_string(dir.path().join("state/components/dsh.json")).unwrap();
    assert!(saved.contains("\"phase\": \"stopped\""), "{saved}");
    assert!(saved.contains("\"attempts\": 0"), "{saved}");
    assert!(saved.contains("\"consecutive_failures\": 0"), "{saved}");
    assert!(saved.contains("\"restarts\": 0"), "{saved}");
    assert!(saved.contains("\"last_result\": \"reset\""), "{saved}");
    assert!(!saved.contains("failed_at_ms"), "{saved}");
    assert!(!saved.contains("last_error"), "{saved}");
    // 只改状态文件：配置原样。
    let config_text = fs::read_to_string(&config).unwrap();
    assert!(config_text.contains("name = \"dsh\""), "{config_text}");
}

#[test]
fn component_reset_refuses_running_component() {
    let dir = TempDir::new("reset-running");
    let config = write_config(
        dir.path(),
        &STATE_CONFIG.replace("{dir}", &dir.path().display().to_string()),
    );
    let components = dir.path().join("state").join("components");
    fs::create_dir_all(&components).unwrap();
    let before = r#"{
  "schema": 1,
  "name": "dsh",
  "phase": "healthy",
  "restart_policy": "on-failure",
  "enabled": true,
  "attempts": 0,
  "consecutive_failures": 0,
  "restarts": 0,
  "pid": 4242,
  "config_sha256": "sha",
  "updated_at_ms": 1
}
"#;
    fs::write(components.join("dsh.json"), before).unwrap();

    let output = run_acb(&[
        "component",
        "reset",
        "dsh",
        "--config",
        config.to_str().expect("path"),
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    let error = stderr(&output);
    assert!(error.contains("component is running"), "{error}");
    assert!(error.contains("phase=healthy"), "{error}");
    assert!(error.contains("pid=4242"), "{error}");
    let saved = fs::read_to_string(components.join("dsh.json")).unwrap();
    assert_eq!(saved, before, "运行态必须原样保留，不得改成 stopped");
}

#[test]
fn component_reset_unknown_name_exits_2() {
    let dir = TempDir::new("reset-unknown");
    let config = write_config(
        dir.path(),
        &STATE_CONFIG.replace("{dir}", &dir.path().display().to_string()),
    );
    let output = run_acb(&[
        "component",
        "reset",
        "missing",
        "--config",
        config.to_str().expect("path"),
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("unknown component: missing"));
}

#[test]
fn component_reset_without_state_file_is_a_noop() {
    let dir = TempDir::new("reset-noop");
    let config = write_config(
        dir.path(),
        &STATE_CONFIG.replace("{dir}", &dir.path().display().to_string()),
    );
    let output = run_acb(&[
        "component",
        "reset",
        "dsh",
        "--config",
        config.to_str().expect("path"),
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("no state file"), "{}", stdout(&output));
    assert!(!dir.path().join("state/components/dsh.json").exists());
}

#[test]
fn default_config_path_is_the_owned_supervisor_toml() {
    let dir = TempDir::new("default-path");
    let data = dir.path().join("data");
    let state = data.join("acb").join("state").join("supervisor");
    fs::create_dir_all(&state).unwrap();
    fs::write(state.join("supervisor.toml"), CONFIG).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_acbctl"))
        .args(["component", "list"])
        .env_remove("ACB_STATE_DIR")
        .env_remove("ACB_UNIT_CONFIG_FILE")
        .env_remove("ACB_PROFILE_DIR")
        .env("XDG_DATA_HOME", &data)
        .output()
        .expect("run acbctl");

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("components: 2 declared (1 enabled)"), "{text}");
}

#[test]
fn unknown_component_exits_2() {
    let dir = TempDir::new("unknown");
    let config = write_config(dir.path(), CONFIG);
    let output = run_acb(&[
        "component",
        "status",
        "missing",
        "--config",
        config.to_str().expect("path"),
    ]);

    assert_eq!(output.status.code(), Some(2), "{}", stdout(&output));
    assert!(stderr(&output).contains("unknown component: missing"));
}

#[test]
fn invalid_component_config_exits_3() {
    let dir = TempDir::new("invalid");
    let config = write_config(
        dir.path(),
        "
[supervisor]
program = \"/bin/acb-bridge\"

[[component]]
name = \"dsh\"
soruce = \"nix\"
command = [\"/bin/dsh\"]
",
    );
    let output = run_acb(&[
        "component",
        "list",
        "--config",
        config.to_str().expect("path"),
    ]);

    assert_eq!(output.status.code(), Some(3), "{}", stdout(&output));
    assert!(stderr(&output).contains("component_unknown_field"), "{output:?}");
}

#[test]
fn empty_component_table_lists_zero() {
    let dir = TempDir::new("empty");
    let config = write_config(
        dir.path(),
        "[supervisor]\nprogram = \"/bin/acb-bridge\"\n",
    );
    let output = run_acb(&[
        "component",
        "list",
        "--config",
        config.to_str().expect("path"),
    ]);

    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert!(stdout(&output).contains("components: 0 declared"));
}
