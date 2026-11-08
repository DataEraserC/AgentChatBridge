//! `acbctl sessions` 推导规则与 `dsh-presets` 脚本行为（假 DSH_HOME / 假 dsh，不碰现场）。

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

const CONFIG_TWO_DSH: &str = r#"
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
id = "dsh-dev"
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

const CONFIG_ONE_DSH: &str = r#"
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
id = "dsh-snowluma-test"
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

/// 现场 web 组合的真实形态：base bundle 默认插入 hmr 行（`!!js` 表达式，未显式
/// `disabled: true`），profile 层没有声明 hmr；hmr 之后还有大量其它插件行，
/// 其中既有 `disabled: true` 的行，也有浏览器侧的 `client-hmr`。
const DUMP_HMR_ENABLED: &str = r#"# == @deepseek-ai/dsh-base
- id: tool-plugin-manager
  name: '@deepseek-ai/dsh-plugin-manager/tools'
  disabled: true
- id: plugin-manager
  name: '@deepseek-ai/dsh-plugin-manager'
  disabled: !!js '!ctx.get(''profileContext'')'
- id: timer
  name: '@deepseek-ai/cordis-plugin-timer'
- id: hmr
  name: '@deepseek-ai/dsh-hmr'
  disabled: !!js '!ctx.get(''profileContext'')'
  config:
    root: []
- id: llm
  name: '@deepseek-ai/dsh-llm'
- id: session
  name: '@deepseek-ai/dsh-session'
  disabled: true
# == @deepseek-ai/dsh-web-app
- id: web
  name: '@deepseek-ai/dsh-web-app'
- id: client-hmr
  name: '@deepseek-ai/dsh-client-hmr'
"#;

/// headless 组合：headless bundle 把 base 的 hmr 行整行覆盖为 `disabled: true`，
/// 该行仍然留在组合里（不是「缺失」），后面也还有其它行。
const DUMP_HMR_DISABLED: &str = r#"# == @deepseek-ai/dsh-base
- id: tool-plugin-manager
  name: '@deepseek-ai/dsh-plugin-manager/tools'
  disabled: true
# == @deepseek-ai/dsh-base, patched by @deepseek-ai/dsh-headless
- id: hmr
  name: '@deepseek-ai/dsh-hmr'
  disabled: true
  config:
    root: []
- id: credentials
  disabled: true
- id: llm
  name: '@deepseek-ai/dsh-llm'
"#;

/// 组合里根本没有 hmr 行（不是被禁用），只能重启。
const DUMP_HMR_ABSENT: &str = r#"# == @deepseek-ai/dsh-base
- id: tool-plugin-manager
  name: '@deepseek-ai/dsh-plugin-manager/tools'
  disabled: true
- id: timer
  name: '@deepseek-ai/cordis-plugin-timer'
- id: llm
  name: '@deepseek-ai/dsh-llm'
"#;

/// 静态 acb-presets bundle 与 profile patch 各注册了一个同名 preset。
const DUMP_CONFLICT: &str = r#"# == @deepseek-ai/dsh-base
- id: hmr
  name: '@deepseek-ai/dsh-hmr'
  disabled: !!js '!ctx.get(''profileContext'')'
  config:
    root: []
# == acb-presets
- id: preset-acb-dsh-qas
  name: '@deepseek-ai/dsh-agent-preset'
  config:
    id: acb-dsh-qas
    plugins: []
# == graph-memory, patched by /etc/dsh/acb-extra.yml
- id: preset-acb-dsh-dev
  name: '@deepseek-ai/dsh-agent-preset'
  config:
    id: acb-dsh-dev
    plugins: []
"#;

fn write(path: &Path, text: &str) -> PathBuf {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
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

fn node_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("ACB_TEST_NODE_BIN") {
        return path.into();
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = dir.join("node");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("测试需要 PATH 里的 node（或设置 ACB_TEST_NODE_BIN）");
}

/// 假 dsh：忽略参数，把固定组合打印到 stdout。
fn fake_dsh(dir: &Path, dump: &str) -> PathBuf {
    let path = dir.join("dsh-real");
    let script = format!("#!/bin/sh\ncat <<'ACB_DUMP_EOF'\n{dump}\nACB_DUMP_EOF\n");
    write(&path, &script);
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake dsh");
    }
    path
}

/// 假 dsh：直接失败，用来证明某些路径不需要启动 dsh。
fn broken_dsh(dir: &Path) -> PathBuf {
    let path = dir.join("dsh-broken");
    let script = "#!/bin/sh\necho 'dsh should not be called' >&2\nexit 9\n";
    write(&path, script);
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake dsh");
    }
    path
}

fn profile_manifest(dsh_home: &Path, bundles: &[&str]) -> PathBuf {
    let dir = dsh_home.join("profiles/nix-web-acb");
    let list = bundles
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    write(
        &dir.join("package.json"),
        &format!(
            r#"{{
  "name": "dsh-profile-nix-web-acb",
  "private": true,
  "dependencies": {{}},
  "dsh": {{ "profile": {{ "bundles": [{list}] }} }}
}}
"#
        ),
    )
}

fn run_script(config: &Path, dsh_home: &Path, dsh_bin: &Path, home: &Path) -> Output {
    Command::new("bash")
        .arg(script_path())
        .env("ACBCTL_BIN", env!("CARGO_BIN_EXE_acbctl"))
        .env("ACB_NODE_BIN", node_bin())
        .env("ACB_CONFIG_FILE", config)
        .env("DSH_HOME", dsh_home)
        .env("ACB_DSH_PROFILE", "nix-web-acb")
        .env("ACB_DSH_BIN", dsh_bin)
        .env("HOME", home)
        .env("ACB_TEST_T_DSH", "x")
        .env("ACB_TEST_T_WEB", "y")
        .output()
        .expect("run dsh-presets script")
}

fn bundles(manifest: &Path) -> Vec<String> {
    let data: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(manifest).expect("manifest")).expect("json");
    data["dsh"]["profile"]["bundles"]
        .as_array()
        .expect("bundles")
        .iter()
        .map(|value| value.as_str().expect("string").to_string())
        .collect()
}

fn ours(manifest: &Path) -> Vec<String> {
    bundles(manifest)
        .into_iter()
        .filter(|name| name.starts_with("acb-presets-live-"))
        .collect()
}

#[test]
fn sessions_derives_preset_and_port_deterministically() {
    let dir = temp_dir("derive");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_DSH);

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
        "dsh-qas\tdsh\tacb-dsh-qas\t3101\ndsh-dev\tdsh\tacb-dsh-dev\t3102\nweb\topencode\tacb-web\t3103\n"
    );
}

#[test]
fn installs_content_addressed_bundle_and_is_idempotent() {
    let dir = temp_dir("install");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_DSH);
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let dsh_home = dir.0.join("dsh-home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base", "acb-presets"]);
    let patch = write(
        &dsh_home.join("cordis.patch.yml"),
        "- id: graph-memory\n  disabled: true\n",
    );
    let patch_before = fs::read_to_string(&patch).expect("patch");
    let dsh = fake_dsh(&dir.0, DUMP_HMR_ENABLED);

    let output = run_script(&config, &dsh_home, &dsh, &home);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let name = ours(&manifest);
    assert_eq!(name.len(), 1, "只登记一个动态 bundle");
    assert!(name[0].starts_with("acb-presets-live-"));
    assert_eq!(
        bundles(&manifest).last().map(String::as_str),
        Some(name[0].as_str()),
        "动态 bundle 追加在列表末尾（最高层）"
    );
    assert!(bundles(&manifest).iter().any(|b| b == "acb-presets"));

    let pkg = dsh_home
        .join("profiles/nix-web-acb/node_modules")
        .join(&name[0]);
    let pkg_json = fs::read_to_string(pkg.join("package.json")).expect("bundle package.json");
    assert!(pkg_json.contains(&format!("\"name\": \"{}\"", name[0])));
    assert!(pkg_json.contains("./presets/acb.patch.yml"));
    let patch_text = fs::read_to_string(pkg.join("presets/acb.patch.yml")).expect("bundle patch");
    assert!(patch_text.contains("    - id: preset-acb-dsh-qas"));
    assert!(patch_text.contains("        id: acb-dsh-qas"));
    assert!(patch_text.contains("url: http://127.0.0.1:3101/mcp"));
    assert!(patch_text.contains("    - id: preset-acb-dsh-dev"));
    assert!(patch_text.contains("url: http://127.0.0.1:3102/mcp"));
    assert_eq!(
        fs::read_to_string(&patch).expect("patch"),
        patch_before,
        "不碰 cordis.patch.yml"
    );

    // 幂等：内容一致时不再启动 dsh，也不改 manifest。
    let before = fs::read_to_string(&manifest).expect("manifest");
    let broken = broken_dsh(&dir.0);
    let again = run_script(&config, &dsh_home, &broken, &home);
    assert!(
        again.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&again.stderr)
    );
    assert_eq!(fs::read_to_string(&manifest).expect("manifest"), before);
}

#[test]
fn content_change_switches_bundle_name_and_removes_old_one() {
    let dir = temp_dir("switch");
    let dsh_home = dir.0.join("dsh-home");
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base"]);
    let dsh = fake_dsh(&dir.0, DUMP_HMR_ENABLED);

    let first = write(&dir.0.join("config-one.toml"), CONFIG_TWO_DSH);
    assert!(run_script(&first, &dsh_home, &dsh, &home).status.success());
    let old = ours(&manifest);
    assert_eq!(old.len(), 1);

    let second = write(&dir.0.join("config-two.toml"), CONFIG_ONE_DSH);
    let output = run_script(&second, &dsh_home, &dsh, &home);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let new = ours(&manifest);
    assert_eq!(new.len(), 1);
    assert_ne!(old[0], new[0], "内容变化必须产生新包名");
    assert!(
        !dsh_home
            .join("profiles/nix-web-acb/node_modules")
            .join(&old[0])
            .exists(),
        "旧包目录必须被清理"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(&format!("- {}\n", old[0])),
        "stdout 应给出 bundles 前后差异：{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn conflict_aborts_without_touching_files() {
    let dir = temp_dir("conflict");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_DSH);
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let dsh_home = dir.0.join("dsh-home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base", "acb-presets"]);
    let before = fs::read_to_string(&manifest).expect("manifest");
    let dsh = fake_dsh(&dir.0, DUMP_CONFLICT);

    let output = run_script(&config, &dsh_home, &dsh, &home);
    assert!(!output.status.success(), "同名 preset 必须非零退出");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("preset id 冲突"), "stderr: {stderr}");
    assert!(stderr.contains("acb-dsh-qas"), "stderr: {stderr}");
    assert!(stderr.contains("acb-dsh-dev"), "stderr: {stderr}");
    assert!(
        stderr.contains("静态 acb-presets bundle"),
        "要指出静态来源：{stderr}"
    );
    assert!(
        stderr.contains("dsh patch /etc/dsh/acb-extra.yml"),
        "要指出 patch 来源：{stderr}"
    );
    assert!(stderr.contains("nix-config"), "要给出处置提示：{stderr}");
    assert_eq!(
        fs::read_to_string(&manifest).expect("manifest"),
        before,
        "冲突时不得改 manifest"
    );
    assert!(
        !dsh_home.join("profiles/nix-web-acb/node_modules").exists(),
        "冲突时不得创建 node_modules"
    );
}

#[test]
fn no_dsh_session_removes_registered_bundle() {
    let dir = temp_dir("remove");
    let dsh_home = dir.0.join("dsh-home");
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base", "acb-presets"]);
    let dsh = fake_dsh(&dir.0, DUMP_HMR_ENABLED);

    let with_dsh = write(&dir.0.join("config-dsh.toml"), CONFIG_ONE_DSH);
    assert!(run_script(&with_dsh, &dsh_home, &dsh, &home)
        .status
        .success());
    let old = ours(&manifest);
    assert_eq!(old.len(), 1);

    let without_dsh = write(&dir.0.join("config-nodsh.toml"), CONFIG_NO_DSH);
    let output = run_script(&without_dsh, &dsh_home, &dsh, &home);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(ours(&manifest).is_empty(), "动态 bundle 必须从列表移除");
    assert!(
        !dsh_home
            .join("profiles/nix-web-acb/node_modules")
            .join(&old[0])
            .exists(),
        "旧包目录必须被清理"
    );
}

#[test]
fn no_dsh_session_with_nothing_installed_needs_no_dsh() {
    let dir = temp_dir("nothing");
    let config = write(&dir.0.join("config.toml"), CONFIG_NO_DSH);
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let dsh_home = dir.0.join("dsh-home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base"]);
    let before = fs::read_to_string(&manifest).expect("manifest");
    let broken = broken_dsh(&dir.0);

    let output = run_script(&config, &dsh_home, &broken, &home);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("无需改动"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(fs::read_to_string(&manifest).expect("manifest"), before);
}

#[test]
fn without_hmr_reports_restart_instead_of_fake_success() {
    let dir = temp_dir("hmr-off");
    let config = write(&dir.0.join("config.toml"), CONFIG_ONE_DSH);
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let dsh_home = dir.0.join("dsh-home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base"]);
    let dsh = fake_dsh(&dir.0, DUMP_HMR_DISABLED);

    let output = run_script(&config, &dsh_home, &dsh, &home);
    assert!(!output.status.success(), "没有 HMR 不得假成功");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("未启用 HMR"), "stderr: {stderr}");
    assert!(stderr.contains("重启 dsh"), "stderr: {stderr}");
    assert_eq!(ours(&manifest).len(), 1, "安装与登记仍要完成");

    // 重启之后同样的状态再跑一次：已就绪，直接成功。
    let again = run_script(&config, &dsh_home, &dsh, &home);
    assert!(
        again.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&again.stderr)
    );
}

#[test]
fn base_default_hmr_is_not_misreported_as_absent() {
    // 回归：base bundle 默认启用 hmr、profile 未显式声明，且 hmr 行之后还有大量
    // 其它行。旧实现把「当前行仍是 hmr」和「出现过 hmr」混用一个状态位，
    // 后续 `- id:` 行一到就判成 absent。
    let dir = temp_dir("hmr-base-default");
    let config = write(&dir.0.join("config.toml"), CONFIG_ONE_DSH);
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let dsh_home = dir.0.join("dsh-home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base"]);
    let dsh = fake_dsh(&dir.0, DUMP_HMR_ENABLED);

    let output = run_script(&config, &dsh_home, &dsh, &home);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("HMR 已启用"),
        "应判定为启用，stdout: {stdout}"
    );
    assert!(
        !stderr.contains("未启用 HMR"),
        "不得再报 absent，stderr: {stderr}"
    );
    assert_eq!(ours(&manifest).len(), 1, "bundle 仍要安装");
}

#[test]
fn missing_hmr_row_requires_restart() {
    let dir = temp_dir("hmr-absent");
    let config = write(&dir.0.join("config.toml"), CONFIG_ONE_DSH);
    let home = dir.0.join("home");
    fs::create_dir_all(&home).expect("home");
    let dsh_home = dir.0.join("dsh-home");
    let manifest = profile_manifest(&dsh_home, &["@deepseek-ai/dsh-base"]);
    let dsh = fake_dsh(&dir.0, DUMP_HMR_ABSENT);

    let output = run_script(&config, &dsh_home, &dsh, &home);
    assert!(!output.status.success(), "组合里没有 hmr 必须非零退出");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("未启用 HMR"), "stderr: {stderr}");
    assert!(stderr.contains("absent"), "stderr: {stderr}");
    assert!(stderr.contains("重启 dsh"), "stderr: {stderr}");
    assert_eq!(ours(&manifest).len(), 1, "安装与登记仍要完成");
}

#[test]
fn fails_fast_without_dsh_home() {
    let dir = temp_dir("no-home");
    let config = write(&dir.0.join("config.toml"), CONFIG_TWO_DSH);
    let missing_home = dir.0.join("missing-home");
    let dsh = broken_dsh(&dir.0);

    let output = run_script(&config, &missing_home, &dsh, &missing_home);
    assert!(!output.status.success(), "没有 DSH_HOME 必须非零退出");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("DSH_HOME"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!missing_home.exists(), "安全失败不得创建目录");
}
