//! `acbctl unit install-user|uninstall-user` 的黑盒测试。
//!
//! 用临时 HOME、临时 system unit 目录和 PATH 里的假 `systemctl`，不碰真实用户管理器。

#![cfg(unix)]

mod common;

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
            "acb-unit-user-{label}-{}-{sequence}",
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

struct Fixture {
    temp: TempDir,
    home: PathBuf,
    log: PathBuf,
    system_unit_dir: PathBuf,
    values: Vec<(String, String)>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let temp = TempDir::new(label);
        let root = temp.path();
        let home = root.join("home");
        let fake_bin = root.join("fakebin");
        let system_unit_dir = root.join("system-units");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(&fake_bin).expect("create fake bin");
        fs::create_dir_all(&system_unit_dir).expect("create system units");

        let log = root.join("systemctl.log");
        let state = root.join("systemctl-state");
        write_fake_systemctl(&fake_bin.join("systemctl"), &state);

        let values = vec![
            (
                "ACB_UNIT_REPO_DIR".to_string(),
                root.join("repo").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_LOG_DIR".to_string(),
                root.join("logs").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_PROFILE_DIR".to_string(),
                root.join("profile").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_BIN_DIR".to_string(),
                root.join("bin").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_CONFIG_FILE".to_string(),
                root.join("config.toml").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_ENV_FILE".to_string(),
                root.join("acb.env").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_OPENCODE_DIR".to_string(),
                root.join("opencode").to_string_lossy().into_owned(),
            ),
            (
                "ACB_UNIT_SYSTEM_UNIT_DIR".to_string(),
                system_unit_dir.to_string_lossy().into_owned(),
            ),
        ];

        Self {
            temp,
            home,
            log,
            system_unit_dir,
            values,
        }
    }

    fn unit_path(&self) -> PathBuf {
        self.home.join(".config/systemd/user/acb-bridge.service")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(&[], args)
    }

    fn run_with(&self, extra_env: &[(&str, &str)], args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_acbctl"));
        for (key, _) in std::env::vars() {
            if key.starts_with("ACB_UNIT_")
                || matches!(
                    key.as_str(),
                    "ACB_PROFILE_DIR" | "ACB_STATE_DIR" | "XDG_CONFIG_HOME"
                )
            {
                command.env_remove(key);
            }
        }
        command.env("HOME", &self.home);
        command.env("SYSTEMCTL_LOG", &self.log);
        command.env("SYSTEMCTL_STATE", self.temp.path().join("systemctl-state"));
        let mut paths = vec![self.temp.path().join("fakebin")];
        if let Some(path) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&path));
        }
        command.env("PATH", std::env::join_paths(paths).expect("join fake PATH"));
        for (key, value) in &self.values {
            command.env(key, value);
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.args(args);
        command.output().expect("spawn acbctl")
    }

    fn systemctl_log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

fn write_fake_systemctl(path: &Path, state: &Path) {
    let body = format!(
        r#"printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
case "$*" in
  *FragmentPath*) printf '%s\n' "${{FRAGMENT_PATH:-}}" ;;
  *LoadState*) printf '%s\n' "${{LOAD_STATE:-loaded}}" ;;
  *is-enabled*)
    if [ -f "{state}/enabled" ]; then
      printf 'enabled\n'
    else
      printf 'disabled\n'
      exit 1
    fi
    ;;
  *enable*) mkdir -p "{state}" && : > "{state}/enabled" ;;
  *disable*) rm -f "{state}/enabled" ;;
esac
exit 0
"#,
        state = state.display()
    );
    common::write_script(path, &body);
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "acbctl failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

#[test]
fn install_user_writes_live_equivalent_unit_and_enables_it() {
    let fixture = Fixture::new("install");
    let output = fixture.run(&["unit", "install-user"]);
    assert_success(&output);

    let text = fs::read_to_string(fixture.unit_path()).expect("read installed unit");
    for directive in [
        "Type=notify",
        "WatchdogSec=120",
        "Restart=on-failure",
        "RestartSec=1",
        "RestartForceExitStatus=75",
        "RestartPreventExitStatus=78",
        "KillMode=process",
        "[Install]\nWantedBy=default.target",
    ] {
        assert!(text.contains(directive), "missing {directive}:\n{text}");
    }
    let config = fixture
        .values
        .iter()
        .find(|(key, _)| key == "ACB_UNIT_CONFIG_FILE")
        .map(|(_, value)| value)
        .expect("config value");
    assert!(
        text.contains(&format!("Environment=ACB_UNIT_CONFIG_FILE={config}")),
        "ACB_UNIT_CONFIG_FILE must be in the unit:\n{text}"
    );

    // ExecStart 从 acbctl 自身安装前缀推导，不能写死 NixOS 路径。
    let exe = fs::canonicalize(env!("CARGO_BIN_EXE_acbctl")).expect("canonical acbctl");
    let expected = exe
        .parent()
        .expect("bin dir")
        .join("acb-launcher")
        .to_string_lossy()
        .into_owned();
    assert!(
        text.contains(&format!("ExecStart={expected}\n")),
        "ExecStart must be {expected}:\n{text}"
    );
    assert!(!text.contains("ExecStart=/run/current-system/sw/bin/acb-launcher"));

    let log = fixture.systemctl_log();
    assert!(log.contains("--user daemon-reload"), "{log}");
    assert!(log.contains("--user enable acb-bridge.service"), "{log}");
}

#[test]
fn second_install_is_unchanged_and_does_not_reload_again() {
    let fixture = Fixture::new("idempotent");
    assert_success(&fixture.run(&["unit", "install-user"]));
    let before = fs::read_to_string(fixture.unit_path()).expect("read unit");

    let output = fixture.run(&["unit", "install-user"]);
    assert_success(&output);
    assert!(stdout(&output).contains("unchanged"), "{}", stdout(&output));
    assert_eq!(
        before,
        fs::read_to_string(fixture.unit_path()).expect("read unit"),
        "repeated install must not change content"
    );
    assert_eq!(
        fixture.systemctl_log().matches("daemon-reload").count(),
        1,
        "unchanged install must not reload again:\n{}",
        fixture.systemctl_log()
    );
}

#[test]
fn changed_values_update_the_unit() {
    let fixture = Fixture::new("update");
    assert_success(&fixture.run(&["unit", "install-user"]));

    let output = fixture.run_with(
        &[("ACB_UNIT_ENDPOINT_LISTEN", "127.0.0.1:7711")],
        &["unit", "install-user"],
    );
    assert_success(&output);
    assert!(stdout(&output).contains("updated"), "{}", stdout(&output));
    let text = fs::read_to_string(fixture.unit_path()).expect("read unit");
    assert!(
        text.contains("Environment=ACB_ENDPOINT_LISTEN=127.0.0.1:7711"),
        "{text}"
    );
    assert_eq!(fixture.systemctl_log().matches("daemon-reload").count(), 2);
}

#[test]
fn nix_provided_unit_is_reported_as_shadowed() {
    let fixture = Fixture::new("shadow");
    let etc_unit = fixture.system_unit_dir.join("acb-bridge.service");
    fs::write(&etc_unit, "[Unit]\nDescription=nix\n").expect("write nix unit");

    let output = fixture.run(&["unit", "install-user"]);
    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("由 nix-config 提供"), "{text}");
    assert!(text.contains(&*etc_unit.to_string_lossy()), "{text}");
    assert!(text.contains("遮蔽"), "{text}");
    assert!(fixture.unit_path().is_file());
}

#[test]
fn unmanaged_unit_file_is_never_overwritten() {
    let fixture = Fixture::new("unmanaged");
    let path = fixture.unit_path();
    fs::create_dir_all(path.parent().expect("unit parent")).expect("create unit dir");
    fs::write(&path, "[Unit]\nDescription=hand-written\n").expect("write unit");

    let output = fixture.run(&["unit", "install-user"]);
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("refusing to overwrite"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(&path).expect("read unit"),
        "[Unit]\nDescription=hand-written\n"
    );
}

#[test]
fn uninstall_user_stops_disables_and_removes_the_unit() {
    let fixture = Fixture::new("uninstall");
    assert_success(&fixture.run(&["unit", "install-user"]));

    let output = fixture.run(&["unit", "uninstall-user"]);
    assert_success(&output);
    assert!(!fixture.unit_path().exists(), "unit file must be removed");
    assert!(
        !fixture
            .unit_path()
            .parent()
            .expect("unit dir")
            .join("acb-units.manifest.json")
            .exists(),
        "empty manifest must be removed"
    );

    let log = fixture.systemctl_log();
    for call in [
        "--user stop acb-bridge.service",
        "--user disable acb-bridge.service",
        "--user daemon-reload",
    ] {
        assert!(log.contains(call), "missing `{call}`:\n{log}");
    }
}
