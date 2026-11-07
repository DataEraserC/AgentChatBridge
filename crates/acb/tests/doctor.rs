//! `acbctl unit doctor` 黑盒测试。

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
            "acbctl-doctor-{label}-{}-{sequence}",
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

fn base_values(root: &Path, bin: &Path, repo: &Path) -> Vec<(&'static str, String)> {
    fs::create_dir_all(repo.join("deploy")).expect("repo deploy");
    fs::create_dir_all(repo.join("state/opencode")).expect("opencode dir");
    for script in ["start-dsh.sh", "start-drivers.sh", "opencode-serve.sh"] {
        common::write_script(&repo.join("deploy").join(script), "exit 0\n");
    }
    fs::write(repo.join("config.toml"), "[endpoint]\n").expect("config");
    fs::write(repo.join("deploy/acb.env"), "").expect("env file");

    vec![
        ("ACB_UNIT_REPO_DIR", repo.to_string_lossy().into_owned()),
        (
            "ACB_UNIT_LOG_DIR",
            root.join("logs").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_PROFILE_DIR",
            "/etc/profiles/per-user/test".to_string(),
        ),
        ("ACB_UNIT_BIN_DIR", bin.to_string_lossy().into_owned()),
        // acbl 是独立安装的静态入口，不来自 payload；测试里显式指到 fixture。
        (
            "ACB_UNIT_LAUNCHER_BIN",
            bin.join("acb-launcher").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_SHELL",
            common::bash_path().to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_CONFIG_FILE",
            repo.join("config.toml").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_ENV_FILE",
            repo.join("deploy/acb.env").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_OPENCODE_DIR",
            repo.join("state/opencode").to_string_lossy().into_owned(),
        ),
        ("ACB_UNIT_CONSOLE_PORT", "3100".to_string()),
        ("ACB_UNIT_OPENCODE_PORT", "4097".to_string()),
        ("ACB_UNIT_AUTOSTART", "0".to_string()),
    ]
}

fn run_acb(values: &[(&str, String)], extra_env: &[(&str, String)], args: &[&str]) -> Output {
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

fn install(root: &Path, values: &[(&str, String)]) -> Output {
    run_acb(
        values,
        &[],
        &["unit", "install", "--root", root.to_str().unwrap()],
    )
}

fn systemctl_shim(dir: &Path, fragment_base: &str) -> PathBuf {
    let path = dir.join("systemctl");
    common::write_script(
        &path,
        &format!(
            r#"set -euo pipefail
unit="${{!#}}"
echo "FragmentPath={fragment_base}/$unit"
"#
        ),
    );
    path
}

fn run_doctor(root: &Path, shim: &Path, home: &Path) -> Output {
    let path = format!(
        "{}:{}",
        shim.parent().expect("shim parent").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_acb(
        &[],
        &[
            ("PATH", path),
            ("HOME", home.to_string_lossy().into_owned()),
            (
                "ACB_PROFILE_DIR",
                home.join("profile-absent").to_string_lossy().into_owned(),
            ),
        ],
        &["unit", "doctor", "--root", root.to_str().unwrap()],
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn doctor_reports_clean_state() {
    let temp = TempDir::new("clean");
    let root = temp.path().join("units");
    let bin = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let shim = temp.path().join("shim");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&shim).expect("shim");
    common::write_script(&bin.join("acb-bridge"), "exit 0\n");
    common::write_script(&bin.join("acb-launcher"), "exit 0\n");
    let values = base_values(temp.path(), &bin, &repo);

    let installed = install(&root, &values);
    assert!(installed.status.success(), "{}", stderr(&installed));

    let home = temp.path().join("home");
    fs::create_dir_all(home.join(".config/systemd/user")).expect("user unit dir");
    let shim = systemctl_shim(&shim, &format!("{}/.config/systemd/user", home.display()));
    let output = run_doctor(&root, &shim, &home);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ok: 7 units verified"),
        "{}",
        stdout(&output)
    );
    assert!(
        stdout(&output).contains("using fallback"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn doctor_reports_sha256_drift() {
    let temp = TempDir::new("sha");
    let root = temp.path().join("units");
    let bin = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let shim_dir = temp.path().join("shim");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&shim_dir).expect("shim");
    common::write_script(&bin.join("acb-bridge"), "exit 0\n");
    let values = base_values(temp.path(), &bin, &repo);

    let installed = install(&root, &values);
    assert!(installed.status.success(), "{}", stderr(&installed));

    let bridge = root.join("acb-bridge.service");
    let mut text = fs::read_to_string(&bridge).expect("bridge unit");
    text.push_str("# tampered\n");
    fs::write(&bridge, text).expect("tamper");

    let home = temp.path().join("home");
    fs::create_dir_all(home.join(".config/systemd/user")).expect("user unit dir");
    let shim = systemctl_shim(
        &shim_dir,
        &format!("{}/.config/systemd/user", home.display()),
    );
    let output = run_doctor(&root, &shim, &home);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("sha256 mismatch"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn doctor_reports_fragment_path_outside_user_dir() {
    let temp = TempDir::new("fragment");
    let root = temp.path().join("units");
    let bin = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let shim_dir = temp.path().join("shim");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&shim_dir).expect("shim");
    common::write_script(&bin.join("acb-bridge"), "exit 0\n");
    let values = base_values(temp.path(), &bin, &repo);

    let installed = install(&root, &values);
    assert!(installed.status.success(), "{}", stderr(&installed));

    let home = temp.path().join("home");
    fs::create_dir_all(home.join(".config/systemd/user")).expect("user unit dir");
    let shim = systemctl_shim(&shim_dir, "/etc/systemd/system");
    let output = run_doctor(&root, &shim, &home);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("outside"), "{}", stdout(&output));
}

#[test]
fn doctor_reports_missing_execstart_program() {
    let temp = TempDir::new("execstart");
    let root = temp.path().join("units");
    let bin = temp.path().join("missing-bin");
    let repo = temp.path().join("repo");
    let shim_dir = temp.path().join("shim");
    fs::create_dir_all(&shim_dir).expect("shim");
    let values = base_values(temp.path(), &bin, &repo);

    let home = temp.path().join("home");
    fs::create_dir_all(home.join(".config/systemd/user")).expect("user unit dir");

    let installed = run_acb(
        &values,
        &[
            ("HOME", home.to_string_lossy().into_owned()),
            (
                "ACB_PROFILE_DIR",
                home.join("profile-absent").to_string_lossy().into_owned(),
            ),
        ],
        &["unit", "install", "--root", root.to_str().unwrap()],
    );
    assert!(installed.status.success(), "{}", stderr(&installed));

    let shim = systemctl_shim(
        &shim_dir,
        &format!("{}/.config/systemd/user", home.display()),
    );
    let output = run_doctor(&root, &shim, &home);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("ExecStart program missing"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn doctor_systemctl_failure_is_internal_error() {
    let temp = TempDir::new("systemctl-error");
    let root = temp.path().join("units");
    let bin = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let shim_dir = temp.path().join("shim");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&shim_dir).expect("shim");
    common::write_script(&bin.join("acb-bridge"), "exit 0\n");
    let values = base_values(temp.path(), &bin, &repo);

    let installed = install(&root, &values);
    assert!(installed.status.success(), "{}", stderr(&installed));

    common::write_script(
        &shim_dir.join("systemctl"),
        "echo 'systemctl failed' >&2\nexit 1\n",
    );
    let home = temp.path().join("home");
    let high = shim_dir.join("systemctl");
    let output = run_doctor(&root, &high, &home);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("systemctl --user show"),
        "{}",
        stderr(&output)
    );
}
