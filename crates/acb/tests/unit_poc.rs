//! ACB unit PoC 的黑盒测试：只通过 `acb` 二进制和临时目录验证行为。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

const UNIT_NAMES: [&str; 7] = [
    "dsh-acb.service",
    "acb-bridge.service",
    "acb-drivers.service",
    "acb-opencode-serve.service",
    "acb-launcher.service",
    "acb-launcher-user.service",
    "acb-stack.target",
];

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acb-unit-{label}-{}-{sequence}",
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

fn base_values(root: &Path) -> Vec<(String, String)> {
    let repo = root.join("repo");
    vec![
        (
            "ACB_UNIT_REPO_DIR".to_string(),
            repo.to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_LOG_DIR".to_string(),
            root.join("logs").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_PROFILE_DIR".to_string(),
            "/etc/profiles/per-user/test".to_string(),
        ),
        (
            "ACB_UNIT_BIN_DIR".to_string(),
            root.join("bin").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_CONFIG_FILE".to_string(),
            repo.join("config.toml").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_ENV_FILE".to_string(),
            repo.join("deploy/acb.env").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_OPENCODE_DIR".to_string(),
            root.join("opencode").to_string_lossy().into_owned(),
        ),
        ("ACB_UNIT_CONSOLE_PORT".to_string(), "3100".to_string()),
        ("ACB_UNIT_OPENCODE_PORT".to_string(), "4097".to_string()),
        ("ACB_UNIT_INSTALL_SECTION".to_string(), "0".to_string()),
    ]
}

fn set_value(values: &mut [(String, String)], key: &str, value: &str) {
    let entry = values
        .iter_mut()
        .find(|(candidate, _)| candidate == key)
        .unwrap_or_else(|| panic!("missing test value {key}"));
    entry.1 = value.to_string();
}

fn run_acb(values: &[(String, String)], extra_env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_acbctl"));
    // 清掉调用方可能残留的 ACB_UNIT_*，保留 PATH 等运行测试所需的环境。
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
    command.output().expect("spawn acb")
}

fn install(root: &Path, values: &[(String, String)]) -> Output {
    let root_arg = root.to_string_lossy().into_owned();
    run_acb(
        values,
        &[],
        &["unit", "install", "--root", root_arg.as_str()],
    )
}

#[test]
fn shell_path_defaults_to_system_bash_and_is_configurable() {
    let temp = TempDir::new("shell");
    let values = base_values(temp.path());
    let default = run_acb(&values, &[], &["unit", "render", "--print"]);
    assert!(default.status.success(), "{}", stderr(&default));
    let text = stdout(&default);
    assert!(
        text.contains("ExecStart=/run/current-system/sw/bin/bash "),
        "{text}"
    );

    let shell = common::bash_path();
    let shell_text = shell.to_string_lossy();
    let custom = run_acb(
        &values,
        &[("ACB_UNIT_SHELL", shell_text.as_ref())],
        &["unit", "render", "--print"],
    );
    assert!(custom.status.success(), "{}", stderr(&custom));
    let text = stdout(&custom);
    assert!(text.contains(&format!("ExecStart={shell_text} ")), "{text}");
    assert!(
        !text.contains("ExecStart=/run/current-system/sw/bin/bash "),
        "custom shell should replace the default: {text}"
    );
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
        "acb failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

fn set_old_mtime(path: &Path) {
    let old = SystemTime::now()
        .checked_sub(Duration::from_secs(3600))
        .expect("one hour ago");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for mtime");
    file.set_times(fs::FileTimes::new().set_modified(old))
        .expect("set mtime");
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("modified time")
}

fn read_manifest(root: &Path) -> serde_json::Value {
    let text = fs::read_to_string(root.join("acb-units.manifest.json")).expect("read manifest");
    serde_json::from_str(&text).expect("parse manifest")
}

fn write_manifest(root: &Path, manifest: &serde_json::Value) {
    let text = serde_json::to_string_pretty(manifest).expect("serialize manifest");
    fs::write(root.join("acb-units.manifest.json"), text + "\n").expect("write manifest");
}

fn add_manifest_entry(root: &Path, name: &str) {
    let mut manifest = read_manifest(root);
    manifest["files"]
        .as_array_mut()
        .expect("files array")
        .push(serde_json::json!(name));
    write_manifest(root, &manifest);
}

#[test]
fn second_install_is_unchanged_and_preserves_mtime() {
    let temp = TempDir::new("idempotent");
    let root = temp.path().join("units");
    let values = base_values(temp.path());

    let first = install(&root, &values);
    assert_success(&first);
    for name in UNIT_NAMES {
        assert!(
            stdout(&first).contains(&format!("created {name}")),
            "first install missing created status for {name}"
        );
    }

    let bridge = root.join("acb-bridge.service");
    set_old_mtime(&bridge);
    let before = mtime(&bridge);

    let second = install(&root, &values);
    assert_success(&second);
    for name in UNIT_NAMES {
        assert!(
            stdout(&second).contains(&format!("unchanged {name}")),
            "second install missing unchanged status for {name}\n{}",
            stdout(&second)
        );
    }
    assert_eq!(
        before,
        mtime(&bridge),
        "unchanged install must not rewrite acb-bridge.service"
    );
}

#[test]
fn changed_value_updates_only_affected_file() {
    let temp = TempDir::new("partial");
    let root = temp.path().join("units");
    let mut values = base_values(temp.path());

    let first = install(&root, &values);
    assert_success(&first);

    for name in UNIT_NAMES {
        if name != "acb-opencode-serve.service" {
            set_old_mtime(&root.join(name));
        }
    }
    set_value(&mut values, "ACB_UNIT_OPENCODE_PORT", "4098");

    let second = install(&root, &values);
    assert_success(&second);
    let output = stdout(&second);
    assert!(
        output.contains("updated acb-opencode-serve.service"),
        "{output}"
    );
    assert!(!output.contains("updated dsh-acb.service"), "{output}");
    for name in UNIT_NAMES {
        if name != "acb-opencode-serve.service" {
            assert!(
                output.contains(&format!("unchanged {name}")),
                "unexpected status for {name}\n{output}"
            );
        }
    }
    for name in UNIT_NAMES {
        if name != "acb-opencode-serve.service" {
            assert!(
                mtime(&root.join(name)) < SystemTime::now() - Duration::from_secs(1800),
                "{name} was rewritten despite unchanged content"
            );
        }
    }
}

#[test]
fn manifest_orphans_are_removed() {
    let temp = TempDir::new("orphan");
    let root = temp.path().join("units");
    let values = base_values(temp.path());

    let first = install(&root, &values);
    assert_success(&first);

    add_manifest_entry(&root, "legacy-driver.service");
    fs::write(
        root.join("legacy-driver.service"),
        "# acb-managed: legacy\n[Unit]\nDescription=old\n",
    )
    .expect("write orphan");

    let second = install(&root, &values);
    assert_success(&second);
    assert!(stdout(&second).contains("removed legacy-driver.service"));
    assert!(!root.join("legacy-driver.service").exists());

    let manifest = read_manifest(&root);
    assert!(
        !manifest["files"]
            .as_array()
            .expect("files array")
            .iter()
            .any(|value| value.as_str() == Some("legacy-driver.service")),
        "orphan must be dropped from the new manifest"
    );
}

#[test]
fn files_without_managed_header_are_never_removed() {
    let temp = TempDir::new("unmanaged");
    let root = temp.path().join("units");
    let values = base_values(temp.path());

    let first = install(&root, &values);
    assert_success(&first);

    add_manifest_entry(&root, "local-override.service");
    let override_path = root.join("local-override.service");
    fs::write(&override_path, "[Unit]\nDescription=local\n").expect("write override");

    // 走到孤儿清理路径：manifest 登记过，但没有 acb-managed 头。
    let second = install(&root, &values);
    assert_success(&second);
    assert!(stdout(&second).contains("kept local-override.service"));
    assert!(
        override_path.exists(),
        "orphan cleanup must not delete a file without # acb-managed:"
    );

    // 再走到卸载路径，结论必须相同。
    add_manifest_entry(&root, "local-override.service");
    let root_arg = root.to_string_lossy().into_owned();
    let removed = run_acb(
        &values,
        &[],
        &["unit", "uninstall", "--root", root_arg.as_str()],
    );
    assert_success(&removed);
    assert!(stdout(&removed).contains("kept local-override.service"));
    assert!(
        override_path.exists(),
        "uninstall must not delete a file without # acb-managed:"
    );
    assert!(!root.join("acb-units.manifest.json").exists());
}

#[test]
fn unknown_acb_unit_key_is_rejected() {
    let temp = TempDir::new("unknown");
    let values = base_values(temp.path());

    let output = run_acb(
        &values,
        &[("ACB_UNIT_NOT_A_REAL_KEY", "value")],
        &["unit", "render", "--print"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("ACB_UNIT_NOT_A_REAL_KEY"),
        "unknown key must be named in the error: {}",
        stderr(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "render must not emit partial output on validation failure"
    );
}

#[test]
fn secrets_are_not_rendered() {
    let temp = TempDir::new("secrets");
    let values = base_values(temp.path());

    let output = run_acb(
        &values,
        &[
            ("ACB_CONSOLE_TOKEN", "console-secret-value"),
            ("ACB_MODEL_KEY", "model-secret-value"),
        ],
        &["unit", "render", "--print"],
    );
    assert_success(&output);
    let rendered = stdout(&output);
    assert!(!rendered.contains("console-secret-value"));
    assert!(!rendered.contains("model-secret-value"));
    for name in UNIT_NAMES {
        assert!(
            rendered.contains(&format!("# === {name} ===")),
            "missing separator for {name}"
        );
    }
}

#[test]
fn diff_reports_same_and_different() {
    let temp = TempDir::new("diff");
    let root = temp.path().join("units");
    let values = base_values(temp.path());

    let first = install(&root, &values);
    assert_success(&first);

    let root_arg = root.to_string_lossy().into_owned();
    let same = run_acb(
        &values,
        &[],
        &["unit", "render", "--diff", "--root", root_arg.as_str()],
    );
    assert_success(&same);
    assert!(stdout(&same).contains("no differences"));

    let mut changed = values.clone();
    set_value(&mut changed, "ACB_UNIT_OPENCODE_PORT", "4098");
    let different = run_acb(
        &changed,
        &[],
        &["unit", "render", "--diff", "--root", root_arg.as_str()],
    );
    assert_eq!(different.status.code(), Some(1));
    assert!(stdout(&different).contains("~ acb-opencode-serve.service"));
}

#[test]
fn install_scope_selects_stack_install_section() {
    let temp = TempDir::new("scope");
    let values = base_values(temp.path());
    let root = temp.path().join("units");
    fs::create_dir_all(&root).expect("create unit root");
    let root_arg = root.to_string_lossy().into_owned();

    let system = run_acb(
        &values,
        &[],
        &[
            "unit",
            "install",
            "--root",
            root_arg.as_str(),
            "--scope",
            "system",
            "--with-install-section",
            "1",
        ],
    );
    assert_success(&system);
    let text = fs::read_to_string(root.join("acb-stack.target")).expect("read target");
    assert!(
        text.contains("WantedBy=multi-user.target"),
        "system scope must use multi-user target: {text}"
    );
    assert!(!text.contains("WantedBy=default.target"), "{text}");

    let user = run_acb(
        &values,
        &[],
        &[
            "unit",
            "install",
            "--root",
            root_arg.as_str(),
            "--scope",
            "user",
            "--with-install-section",
            "1",
        ],
    );
    assert_success(&user);
    let text = fs::read_to_string(root.join("acb-stack.target")).expect("read target");
    assert!(text.contains("WantedBy=default.target"), "{text}");

    let invalid = run_acb(
        &values,
        &[],
        &[
            "unit", "install", "--root", root_arg.as_str(), "--scope", "container",
        ],
    );
    assert!(!invalid.status.success());
    assert!(
        stderr(&invalid).contains("--scope must be user or system"),
        "{}",
        stderr(&invalid)
    );
}

#[test]
fn install_section_defaults_off_and_renders_when_set() {
    let temp = TempDir::new("install-section");
    let values = base_values(temp.path());

    let disabled = run_acb(&values, &[], &["unit", "render", "--print"]);
    assert_success(&disabled);
    let text = stdout(&disabled);
    assert!(
        !text.contains("[Install]"),
        "default ACB_UNIT_INSTALL_SECTION=0 must not render [Install]"
    );
    assert!(!text.contains("WantedBy=default.target"));

    let enabled = run_acb(
        &values,
        &[("ACB_UNIT_INSTALL_SECTION", "1")],
        &["unit", "render", "--print"],
    );
    assert_success(&enabled);
    let text = stdout(&enabled);
    assert_eq!(
        text.matches("[Install]").count(),
        3,
        "stack target and both launcher units carry [Install]"
    );
    assert!(text.contains("[Install]\nWantedBy=default.target"));
    assert!(text.contains("[Install]\nWantedBy=multi-user.target"));

    let invalid = run_acb(
        &values,
        &[("ACB_UNIT_INSTALL_SECTION", "2")],
        &["unit", "render", "--print"],
    );
    assert!(!invalid.status.success());
    assert!(
        stderr(&invalid).contains("ACB_UNIT_INSTALL_SECTION must be 0 or 1"),
        "{}",
        stderr(&invalid)
    );
}

#[test]
fn bridge_template_has_no_health_probe_or_reload() {
    let temp = TempDir::new("bridge-clean");
    let values = base_values(temp.path());

    let output = run_acb(&values, &[], &["unit", "render", "--print"]);
    assert_success(&output);
    let text = stdout(&output);
    assert!(!text.contains("ExecStartPost"));
    assert!(!text.contains("ExecReload"));
    assert!(!text.contains("/healthz"));
}

#[test]
fn output_dir_contains_only_rendered_units() {
    let temp = TempDir::new("output-dir");
    let values = base_values(temp.path());
    let out_dir = temp.path().join("out");

    let out_arg = out_dir.to_string_lossy().into_owned();

    let output = run_acb(
        &values,
        &[],
        &["unit", "render", "--output-dir", out_arg.as_str()],
    );
    assert_success(&output);
    for name in UNIT_NAMES {
        assert!(out_dir.join(name).is_file(), "missing {name}");
    }
    assert!(!out_dir.join("acb-units.manifest.json").exists());
}
