//! `acbctl profile` 黑盒测试：add/list/switch/gc/seed、原子切换、render/run 解析。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acbctl-profile-{label}-{}-{sequence}",
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

fn payload(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(format!("payload-{name}"));
    fs::create_dir_all(dir.join("bin")).expect("bin");
    fs::create_dir_all(dir.join("harnesses")).expect("harnesses");
    fs::create_dir_all(dir.join("deploy/units/systemd")).expect("templates");
    fs::create_dir_all(dir.join("deploy/run")).expect("run templates");
    fs::create_dir_all(dir.join("harnesses/dsh/src")).expect("dsh harness");
    fs::create_dir_all(dir.join("harnesses/opencode/plugin")).expect("opencode plugin");
    fs::create_dir_all(dir.join("harnesses/shared")).expect("shared harness");
    for file in [
        "harnesses/dsh/src/driver.mjs",
        "harnesses/dsh/dsh-mcp.patch.yml",
        "harnesses/opencode/plugin/index.js",
        "harnesses/opencode/plugin/acb-wire.mjs",
        "harnesses/shared/tools.mjs",
    ] {
        fs::write(
            dir.join(file),
            "// profile test fixture; relative imports rely on the full harnesses tree\n",
        )
        .expect("harness file");
    }
    fs::write(
        dir.join("harnesses/opencode/plugin/index.js"),
        "import { TOOLS } from \"../../shared/tools.mjs\"\n",
    )
    .expect("opencode plugin");
    fs::write(
        dir.join("harnesses/shared/tools.mjs"),
        "export const TOOLS = []\n",
    )
    .expect("shared tools");
    for script in ["start-dsh.sh", "start-drivers.sh", "opencode-serve.sh"] {
        common::write_script(&dir.join("deploy").join(script), "exit 0\n");
    }
    fs::write(
        dir.join("deploy/run/supervisor.toml.tmpl"),
        "[supervisor]\nprogram = \"@ACB_UNIT_BIN_DIR@/acb-bridge\"\n",
    )
    .expect("run template");
    for bin in ["acb-bridge", "acb-launcher", "acbctl"] {
        common::write_script(&dir.join("bin").join(bin), "exit 0\n");
    }
    dir
}

fn run_acb(env: &[(&str, String)], args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_acbctl"));
    for (key, _) in std::env::vars() {
        if key.starts_with("ACB_UNIT_") {
            command.env_remove(key);
        }
    }
    command.env_remove("ACB_PROFILE_DIR");
    for (key, value) in env {
        command.env(key, value);
    }
    command.args(args);
    command.output().expect("spawn acbctl")
}

fn profile_env(profile: &Path, config: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("ACB_PROFILE_DIR", profile.to_string_lossy().into_owned()),
        (
            "ACB_UNIT_CONFIG_FILE",
            config.to_string_lossy().into_owned(),
        ),
    ]
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn add(profile: &Path, config: &Path, source: &Path, label: &str) -> String {
    let env = profile_env(profile, config);
    let output = run_acb(
        &env,
        &["profile", "add", source.to_str().unwrap(), "--label", label],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    text.trim()
        .strip_prefix("added ")
        .expect("add output")
        .to_string()
}

fn generation_dir(profile: &Path, id: &str) -> PathBuf {
    profile.join("generations").join(id)
}

fn read_link(profile: &Path) -> PathBuf {
    fs::read_link(profile.join("current")).expect("current symlink")
}

#[test]
fn add_list_switch_and_gc() {
    let temp = TempDir::new("core");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");

    let first = add(&profile, &config, &payload(temp.path(), "first"), "first");
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generation_dir(&profile, &first).join("meta.json")).expect("meta"),
    )
    .expect("parse meta");
    assert_eq!(meta["schema"], 1);
    assert_eq!(meta["label"], "first");
    assert_eq!(meta["source"]["kind"], "path");
    assert!(meta["config"]["sha256"].as_str().unwrap().len() == 64);

    let env = profile_env(&profile, &config);
    let listed = run_acb(&env, &["profile", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(
        stdout(&listed).contains("label=first"),
        "{}",
        stdout(&listed)
    );
    assert!(
        stdout(&listed).contains("source=path:"),
        "{}",
        stdout(&listed)
    );

    let current = run_acb(&env, &["profile", "current"]);
    assert_eq!(current.status.code(), Some(1), "{}", stderr(&current));

    let switched = run_acb(&env, &["profile", "switch", "first"]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    assert_eq!(
        read_link(&profile),
        PathBuf::from("generations").join(&first).join("root")
    );
    let current = run_acb(&env, &["profile", "current"]);
    assert!(current.status.success(), "{}", stderr(&current));
    assert!(stdout(&current).contains(&format!("id: {first}")));

    // 三个 generation，切到最老的 first，gc 保留最新 1 个 + current。
    std::thread::sleep(Duration::from_millis(20));
    let second = add(&profile, &config, &payload(temp.path(), "second"), "second");
    std::thread::sleep(Duration::from_millis(20));
    let third = add(&profile, &config, &payload(temp.path(), "third"), "third");

    let gc = run_acb(&env, &["profile", "gc", "--keep", "1"]);
    assert!(gc.status.success(), "{}", stderr(&gc));
    assert!(
        !generation_dir(&profile, &second).exists(),
        "middle generation should be removed"
    );
    assert!(
        generation_dir(&profile, &first).exists(),
        "current must be kept"
    );
    assert!(
        generation_dir(&profile, &third).exists(),
        "newest must be kept"
    );
    assert!(stdout(&gc).contains(&format!("removed {second}")));
}

#[test]
fn failed_switch_keeps_old_current() {
    let temp = TempDir::new("switch-fail");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let first = add(&profile, &config, &payload(temp.path(), "first"), "first");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", "first"]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    let old = read_link(&profile);

    let failed = run_acb(&env, &["profile", "switch", "missing-generation"]);
    assert!(!failed.status.success());
    assert_eq!(read_link(&profile), old);
    assert!(generation_dir(&profile, &first).exists());
}

#[test]
fn seed_is_idempotent_and_sets_first_current() {
    let temp = TempDir::new("seed");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let first_payload = payload(temp.path(), "seed-first");
    let second_payload = payload(temp.path(), "seed-second");
    let env = profile_env(&profile, &config);

    let first = run_acb(&env, &["profile", "seed", first_payload.to_str().unwrap()]);
    assert!(first.status.success(), "{}", stderr(&first));
    let seeded_id = stdout(&first)
        .trim()
        .strip_prefix("seeded ")
        .expect("seed output")
        .split_whitespace()
        .next()
        .expect("seed id")
        .to_string();
    assert_eq!(
        read_link(&profile),
        PathBuf::from("generations").join(&seeded_id).join("root")
    );

    let second = run_acb(&env, &["profile", "seed", second_payload.to_str().unwrap()]);
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert!(
        stdout(&second).contains("seed skipped: profile already has 1 generation(s)"),
        "{}",
        stdout(&second)
    );
    let generations = fs::read_dir(profile.join("generations"))
        .expect("generations")
        .count();
    assert_eq!(generations, 1, "seed must not add a second generation");
}

fn seed_and_expect_repair(
    profile: &Path,
    config: &Path,
    surviving_id: &str,
    repair_payload: &Path,
    reason: &str,
) -> String {
    let env = profile_env(profile, config);
    let seeded = run_acb(&env, &["profile", "seed", repair_payload.to_str().unwrap()]);
    assert!(seeded.status.success(), "{}", stderr(&seeded));
    let text = stdout(&seeded);
    let prefix = "seed repaired current -> ";
    let repaired = text
        .trim()
        .strip_prefix(prefix)
        .expect("repair output")
        .split_whitespace()
        .next()
        .expect("repaired id")
        .to_string();
    assert!(text.contains(reason), "missing repair reason: {text}");
    assert_eq!(
        read_link(profile),
        PathBuf::from("generations").join(&repaired).join("root")
    );
    assert!(
        profile.join("current/bin/acb-bridge").is_file(),
        "repaired current has no bin/acb-bridge"
    );
    assert!(
        generation_dir(profile, surviving_id).exists(),
        "repair must not delete the surviving generation"
    );
    repaired
}

#[test]
fn seed_repairs_missing_current() {
    let temp = TempDir::new("seed-repair-missing");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let existing = add(&profile, &config, &payload(temp.path(), "old"), "old");
    let repaired = seed_and_expect_repair(
        &profile,
        &config,
        &existing,
        &payload(temp.path(), "repair"),
        "current symlink missing",
    );
    assert_ne!(existing, repaired);
    assert_eq!(
        fs::read_dir(profile.join("generations"))
            .expect("generations")
            .count(),
        2
    );
}

#[test]
fn seed_repairs_current_pointing_to_missing_generation() {
    let temp = TempDir::new("seed-repair-dangling");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let surviving = add(
        &profile,
        &config,
        &payload(temp.path(), "surviving"),
        "surviving",
    );
    let existing = add(&profile, &config, &payload(temp.path(), "old"), "old");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &existing]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    fs::remove_dir_all(generation_dir(&profile, &existing)).expect("drop generation");

    let repaired = seed_and_expect_repair(
        &profile,
        &config,
        &surviving,
        &payload(temp.path(), "repair"),
        "current target missing",
    );
    assert!(generation_dir(&profile, &repaired).exists());
}

#[test]
fn seed_repairs_when_bridge_binary_is_missing() {
    let temp = TempDir::new("seed-repair-no-bridge");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let existing_payload = payload(temp.path(), "old");
    let existing = add(&profile, &config, &existing_payload, "old");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &existing]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    fs::remove_file(existing_payload.join("bin/acb-bridge")).expect("drop bridge binary");

    let repaired = seed_and_expect_repair(
        &profile,
        &config,
        &existing,
        &payload(temp.path(), "repair"),
        "current target missing bin/acb-bridge",
    );
    assert_ne!(existing, repaired);
}

#[test]
fn seed_fails_actionably_when_current_cannot_be_repaired() {
    let temp = TempDir::new("seed-repair-fail");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let existing_payload = payload(temp.path(), "old");
    let existing = add(&profile, &config, &existing_payload, "old");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &existing]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    fs::remove_file(existing_payload.join("bin/acb-bridge")).expect("drop bridge binary");

    let missing_payload = run_acb(&env, &["profile", "seed"]);
    assert_eq!(
        missing_payload.status.code(),
        Some(2),
        "{}",
        stderr(&missing_payload)
    );
    let text = stderr(&missing_payload);
    assert!(text.contains("profile current is not usable"), "{text}");
    assert!(text.contains("acbctl profile add"), "{text}");
    assert!(text.contains("acbctl profile switch"), "{text}");
    assert!(
        !stdout(&missing_payload).contains("seed skipped"),
        "seed must not silently skip a broken current"
    );

    let invalid_payload = temp.path().join("not-a-payload");
    let bad = run_acb(
        &env,
        &["profile", "seed", invalid_payload.to_str().unwrap()],
    );
    assert_eq!(bad.status.code(), Some(2), "{}", stderr(&bad));
    let text = stderr(&bad);
    assert!(text.contains("seed cannot repair it"), "{text}");
    assert!(text.contains("acbctl profile add"), "{text}");
    assert!(text.contains("acbctl profile switch"), "{text}");
    assert_eq!(
        read_link(&profile),
        PathBuf::from("generations").join(&existing).join("root"),
        "failed repair must not change current"
    );
    assert_eq!(
        fs::read_dir(profile.join("generations"))
            .expect("generations")
            .count(),
        1,
        "failed repair must not create a generation"
    );
}

#[test]
fn render_falls_back_without_profile() {
    let temp = TempDir::new("render-fallback");
    let profile = temp.path().join("profile-absent");
    let fallback_bin = temp.path().join("fallback-bin");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&fallback_bin).expect("fallback bin");
    fs::create_dir_all(&repo).expect("repo");

    let mut env = unit_values(&repo, &fallback_bin, &temp.path().join("config.toml"));
    env.push(("ACB_PROFILE_DIR", profile.to_string_lossy().into_owned()));
    let output = run_acb(&env, &["unit", "render", "--print"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains(&format!("{}/acb-bridge", fallback_bin.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("{}/deploy/start-dsh.sh", repo.display())),
        "{text}"
    );
    assert!(
        !text.contains(&format!("{}/current/bin", profile.display())),
        "{text}"
    );
}

#[test]
fn render_prefers_profile_current_bin() {
    let temp = TempDir::new("render-profile");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let profile_payload = payload(temp.path(), "profile-payload");
    let fallback_bin = temp.path().join("fallback-bin");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&fallback_bin).expect("fallback bin");
    fs::create_dir_all(&repo).expect("repo");
    let id = add(&profile, &config, &profile_payload, "main");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &id]);
    assert!(switched.status.success(), "{}", stderr(&switched));

    let env = unit_values(&repo, &fallback_bin, &config);
    let mut env = env;
    env.push(("ACB_PROFILE_DIR", profile.to_string_lossy().into_owned()));
    let output = run_acb(&env, &["unit", "render", "--print"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    let current = format!("{}/current", profile.display());
    assert!(
        text.contains(&format!("{current}/bin/acb-bridge")),
        "{text}"
    );
    assert!(
        text.contains(&format!("{current}/deploy/start-dsh.sh")),
        "{text}"
    );
    assert!(
        text.contains(&format!("{current}/deploy/start-drivers.sh")),
        "{text}"
    );
    assert!(
        text.contains(&format!("{current}/deploy/opencode-serve.sh")),
        "{text}"
    );
    assert!(
        text.contains(&format!("WorkingDirectory={current}")),
        "{text}"
    );
    assert!(
        !text.contains(&format!("{}/deploy/start-dsh.sh", repo.display())),
        "{text}"
    );
    assert!(
        !text.contains(&format!("{}/acb-bridge", fallback_bin.display())),
        "{text}"
    );

    // 插件按原仓库结构放在 profile/current/harnesses 下，相对 import 目标必须存在。
    let plugin = profile.join("current/harnesses/opencode/plugin/index.js");
    let shared = profile.join("current/harnesses/shared/tools.mjs");
    assert!(plugin.is_file(), "plugin missing: {}", plugin.display());
    assert!(
        shared.is_file(),
        "shared tools missing: {}",
        shared.display()
    );
    let plugin_text = fs::read_to_string(&plugin).expect("plugin content");
    assert!(
        plugin_text.contains("../../shared/tools.mjs"),
        "{plugin_text}"
    );
    let imported = plugin
        .parent()
        .expect("plugin parent")
        .join("../../shared/tools.mjs");
    assert!(
        imported.is_file(),
        "plugin relative import target missing: {}",
        imported.display()
    );
}

#[test]
fn run_prefers_profile_current_bin() {
    let temp = TempDir::new("run-profile");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    let supervisor = temp.path().join("supervisor.toml");
    let state = temp.path().join("state");
    fs::write(&config, "[endpoint]\n").expect("config");
    fs::write(
        &supervisor,
        format!(
            "[supervisor]\nprogram = \"/fake/bridge\"\nstate_dir = \"{}\"\n",
            state.display()
        ),
    )
    .expect("supervisor config");
    let profile_payload = payload(temp.path(), "run-profile-payload");
    let marker = temp.path().join("launcher-started");
    let repo_marker = temp.path().join("launcher-repo");
    common::write_script(
        &profile_payload.join("bin/acb-launcher"),
        &format!(
            "set -euo pipefail\necho started > {}\necho \"${{ACB_UNIT_REPO_DIR:-none}}\" > {}\nexit 0\n",
            marker.to_string_lossy(),
            repo_marker.to_string_lossy()
        ),
    );
    let id = add(&profile, &config, &profile_payload, "main");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &id]);
    assert!(switched.status.success(), "{}", stderr(&switched));

    // 故意把回退 BIN_DIR 指向不存在的路径；profile 活跃时必须用 current/bin。
    let env = vec![
        ("ACB_PROFILE_DIR", profile.to_string_lossy().into_owned()),
        (
            "ACB_UNIT_BIN_DIR",
            temp.path().join("wrong-bin").to_string_lossy().into_owned(),
        ),
    ];
    let output = run_acb(
        &env,
        &[
            "run",
            "--mode=launcher",
            "--config",
            supervisor.to_str().unwrap(),
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(marker.exists(), "profile launcher was not spawned");
    assert_eq!(
        fs::read_to_string(&repo_marker).expect("repo marker"),
        format!("{}/current\n", profile.display())
    );
}

#[test]
fn gc_keeps_current_and_lkg_generation() {
    let temp = TempDir::new("gc-lkg");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let first = add(&profile, &config, &payload(temp.path(), "first"), "first");
    std::thread::sleep(Duration::from_millis(20));
    let second = add(&profile, &config, &payload(temp.path(), "second"), "second");
    std::thread::sleep(Duration::from_millis(20));
    let third = add(&profile, &config, &payload(temp.path(), "third"), "third");
    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &first]);
    assert!(switched.status.success(), "{}", stderr(&switched));

    let state = temp.path().join("state/supervisor");
    fs::create_dir_all(&state).expect("state");
    fs::write(
        state.join("lkg.json"),
        format!(
            r#"{{"command":{{"program":"/fake","args":[],"generation":{{"id":"{second}","path":"/fake/root","config_sha256":"abc"}}}},"promoted_at_ms":1}}"#
        ),
    )
    .expect("lkg.json");

    let gc = run_acb(&env, &["profile", "gc", "--keep", "0"]);
    assert!(gc.status.success(), "{}", stderr(&gc));
    assert!(
        generation_dir(&profile, &first).exists(),
        "current must be kept"
    );
    assert!(
        generation_dir(&profile, &second).exists(),
        "lkg must be kept"
    );
    assert!(
        !generation_dir(&profile, &third).exists(),
        "other must be removed"
    );
}

#[test]
fn run_renders_supervisor_generation_section_from_profile() {
    let temp = TempDir::new("run-generation");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    let state = temp.path().join("state");
    fs::write(&config, "[endpoint]\n").expect("config");
    let profile_payload = payload(temp.path(), "generation");
    let id = add(&profile, &config, &profile_payload, "main");
    let profile_env_values = profile_env(&profile, &config);
    let switched = run_acb(&profile_env_values, &["profile", "switch", &id]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generation_dir(&profile, &id).join("meta.json")).expect("meta"),
    )
    .expect("parse meta");
    let config_sha = meta["config"]["sha256"].as_str().expect("config sha");

    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo");
    let mut env = unit_values(&repo, &temp.path().join("fallback-bin"), &config);
    env.push(("ACB_PROFILE_DIR", profile.to_string_lossy().into_owned()));
    let output = run_acb(
        &env,
        &[
            "run",
            "--mode=launcher",
            "--state-dir",
            state.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "{}", stderr(&output));

    let supervisor = acb_supervisor::load_config(&state.join("supervisor.toml"))
        .expect("load rendered supervisor.toml");
    let generation = supervisor.generation.expect("generation section");
    assert_eq!(generation.id, id);
    assert_eq!(
        generation.path,
        fs::canonicalize(&profile_payload)
            .expect("canonical payload")
            .to_string_lossy()
    );
    assert_eq!(generation.config_sha256.as_deref(), Some(config_sha));
}

fn unit_values(repo: &Path, bin: &Path, config: &Path) -> Vec<(&'static str, String)> {
    vec![
        ("ACB_UNIT_REPO_DIR", repo.to_string_lossy().into_owned()),
        (
            "ACB_UNIT_LOG_DIR",
            repo.join("logs").to_string_lossy().into_owned(),
        ),
        (
            "ACB_UNIT_PROFILE_DIR",
            "/etc/profiles/per-user/test".to_string(),
        ),
        ("ACB_UNIT_BIN_DIR", bin.to_string_lossy().into_owned()),
        (
            "ACB_UNIT_CONFIG_FILE",
            config.to_string_lossy().into_owned(),
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
