//! `acbctl sessions` 推导规则与 `dsh-presets` 脚本行为（假 DSH_HOME，不碰现场）。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(label: &str) -> TempDir {
    let path = std::env::temp_dir().join(format!(
        "acb-sessions-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    TempDir(path)
}

const CONFIG_TWO_SESSIONS: &str = r#"
[[personas]]
id = "p"
name = "人格"
text = "你是桥。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[harnesses]]
id = "opencode"
name = "OpenCode"

[[sessions]]
id = "dsh-qas"
harness = "dsh"
persona = "p"
token = { secret = "t-dsh" }

[[sessions]]
id = "web"
harness = "opencode"
persona = "p"
token = { secret = "t-web" }

[secrets]
[[secrets.sources]]
name = "t-dsh"
kind = "env"
variable = "ACB_TEST_T_DSH"

[[secrets.sources]]
name = "t-web"
kind = "env"
variable = "ACB_TEST_T_WEB"
"#;

const CONFIG_NO_DSH: &str = r#"
[[personas]]
id = "p"
name = "人格"
text = "你是桥。"

[[harnesses]]
id = "opencode"
name = "OpenCode"

[[sessions]]
id = "web"
harness = "opencode"
persona = "p"
token = { secret = "t-web" }

[secrets]
[[secrets.sources]]
name = "t-web"
kind = "env"
variable = "ACB_TEST_T_WEB"
"#;

fn write(path: &Path, text: &str) -> PathBuf {
    fs::write(path, text).expect("write fixture");
    path.to_path_buf()
}

fn acbctl() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_acbctl"));
    command
        .env("ACB_TEST_T_DSH", "x")
        .env("ACB_TEST_T_WEB", "y");
    command
}

fn script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../deploy/presets/dsh-presets/dsh_presets_acb.sh")
}

fn run_script(config: &Path, dsh_home: &Path, home: Option<&Path>) -> Output {
    let mut command = Command::new("bash");
    command
        .arg(script_path())
        .env("ACBCTL_BIN", env!("CARGO_BIN_EXE_acbctl"))
        .env("ACB_CONFIG_FILE", config)
        .env("DSH_HOME", dsh_home)
        .env("ACB_TEST_T_DSH", "x")
        .env("ACB_TEST_T_WEB", "y");
    match home {
        Some(home) => {
            command.env("HOME", home);
        }
        None => {
            command.env("HOME", "/nonexistent-acb-home");
        }
    }
    command.output().expect("run dsh-presets script")
}

#[test]
fn sessions_derives_preset_and_port_deterministically() {
    let dir = temp_dir("derive");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_SESSIONS);

    let output = acbctl()
        .args(["sessions", "--config"])
        .arg(&config)
        .output()
        .expect("run acbctl sessions");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "dsh-qas\tdsh\tacb-dsh-qas\t3101\nweb\topencode\tacb-web\t3102\n"
    );
}

#[test]
fn dsh_presets_writes_managed_block_once_and_stays_idempotent() {
    let dir = temp_dir("write");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_SESSIONS);
    let home = dir.0.join("dsh-home");
    fs::create_dir_all(&home).expect("dsh home");
    let target = write(
        &home.join("cordis.patch.yml"),
        "- insert:\n    - id: something-from-nix\n      name: '@deepseek-ai/whatever'\n",
    );

    let output = run_script(&config, &home, None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first = fs::read_to_string(&target).expect("patch");
    assert!(first.contains("    - id: preset-acb-dsh-qas"));
    assert!(first.contains("        id: acb-dsh-qas"));
    assert!(first.contains("url: http://127.0.0.1:3101/mcp"));
    assert!(first.contains("something-from-nix"), "非托管内容必须保留");
    assert_eq!(first.matches("# BEGIN acb-generated dsh-presets").count(), 1);

    let again = run_script(&config, &home, None);
    assert!(again.status.success());
    let second = fs::read_to_string(&target).expect("patch");
    assert_eq!(first, second, "重复执行必须整写同一份内容");
}

#[test]
fn dsh_presets_removes_block_when_no_dsh_session() {
    let dir = temp_dir("remove");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_SESSIONS);
    let home = dir.0.join("dsh-home");
    fs::create_dir_all(&home).expect("dsh home");
    let target = write(
        &home.join("cordis.patch.yml"),
        "- insert:\n    - id: something-from-nix\n      name: '@deepseek-ai/whatever'\n",
    );
    assert!(run_script(&config, &home, None).status.success());
    assert!(fs::read_to_string(&target)
        .expect("patch")
        .contains("preset-acb-dsh-qas"));

    let no_dsh = write(&dir.0.join("config-nodsh.toml"), CONFIG_NO_DSH);
    let output = run_script(&no_dsh, &home, None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = fs::read_to_string(&target).expect("patch");
    assert!(!text.contains("acb-generated"), "托管块必须被移除");
    assert!(text.contains("something-from-nix"), "其余内容保留");
}

#[test]
fn dsh_presets_refuses_conflicting_preset_and_leaves_file_unchanged() {
    let dir = temp_dir("conflict");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_SESSIONS);
    let home = dir.0.join("dsh-home");
    fs::create_dir_all(&home).expect("dsh home");
    let target = write(
        &home.join("cordis.patch.yml"),
        "- insert:\n    - id: preset-acb-dsh-qas\n      name: '@deepseek-ai/dsh-agent-preset'\n",
    );
    let before = fs::read_to_string(&target).expect("patch");

    let output = run_script(&config, &home, None);
    assert!(!output.status.success(), "同名 preset 必须报错退出");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("preset 冲突"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&target).expect("patch"),
        before,
        "冲突时不得改写文件"
    );
}

#[test]
fn dsh_presets_fails_fast_without_dsh_home() {
    let dir = temp_dir("no-home");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_SESSIONS);
    let missing_home = dir.0.join("missing-home");

    let output = run_script(&config, &missing_home, Some(&missing_home));
    assert!(!output.status.success(), "没有 DSH_HOME 必须非零退出");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("DSH_HOME"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!missing_home.exists(), "安全失败不得创建目录");
}
