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
    fs::create_dir_all(dir.join("share/acb/units")).expect("templates");
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
    command.env_remove("ACB_PROFILE_STRATEGY");
    command.env_remove("ACB_STATE_DIR");
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
        &[
            "profile",
            "add",
            source.to_str().unwrap(),
            "--label",
            label,
            "--strategy",
            "copy",
        ],
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
    assert_eq!(meta["payload"]["strategy"], "copy");
    assert!(
        meta["payload"]["snapshot"]["files"].as_u64().unwrap() > 0,
        "{meta}"
    );
    assert!(meta["config"]["sha256"].as_str().unwrap().len() == 64);
    let snapshot_root = generation_dir(&profile, &first).join("root");
    assert!(
        !fs::symlink_metadata(&snapshot_root)
            .expect("snapshot root")
            .file_type()
            .is_symlink(),
        "copy strategy must not leave a symlink"
    );
    assert!(snapshot_root.join("bin/acb-bridge").is_file());

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

    let first = run_acb(
        &env,
        &[
            "profile",
            "seed",
            first_payload.to_str().unwrap(),
            "--strategy",
            "copy",
        ],
    );
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

    // 安装器只在首次安装时写一次 LKG；后续 runtime 提升/回退归 launcher + supervisor。
    let lkg_path = temp.path().join("state/supervisor/lkg.json");
    let lkg_text = fs::read_to_string(&lkg_path).expect("initial lkg.json");
    let lkg: serde_json::Value = serde_json::from_str(&lkg_text).expect("parse lkg.json");
    assert_eq!(lkg["command"]["generation"]["id"], seeded_id);
    let pinned = generation_dir(&profile, &seeded_id).join("root/bin/acb-bridge");
    assert_eq!(
        lkg["command"]["program"],
        pinned.to_string_lossy().into_owned()
    );
    assert_eq!(lkg["command"]["args"][0], "--config");

    let second = run_acb(
        &env,
        &[
            "profile",
            "seed",
            second_payload.to_str().unwrap(),
            "--strategy",
            "copy",
        ],
    );
    assert_eq!(second.status.code(), Some(0), "{}", stderr(&second));
    assert_eq!(
        fs::read_to_string(&lkg_path).expect("lkg.json after skipped seed"),
        lkg_text,
        "seed must not rewrite the runtime LKG pointer"
    );
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
    let seeded = run_acb(
        &env,
        &[
            "profile",
            "seed",
            repair_payload.to_str().unwrap(),
            "--strategy",
            "copy",
        ],
    );
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
    fs::remove_file(profile.join("current/bin/acb-bridge")).expect("drop bridge binary");

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
    fs::remove_file(profile.join("current/bin/acb-bridge")).expect("drop bridge binary");

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
        &[
            "profile",
            "seed",
            invalid_payload.to_str().unwrap(),
            "--strategy",
            "copy",
        ],
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
    let snapshot_root = generation_dir(&profile, &id).join("root");
    assert_eq!(generation.path, snapshot_root.to_string_lossy());
    assert!(snapshot_root.join("bin/acb-bridge").is_file());
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
        ("ACB_UNIT_INSTALL_SECTION", "0".to_string()),
    ]
}

#[test]
fn strategy_must_be_explicit_and_unsupported_modes_fail() {
    let temp = TempDir::new("strategy-required");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let source = payload(temp.path(), "strategy");
    let env = profile_env(&profile, &config);

    let missing = run_acb(&env, &["profile", "add", source.to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(2), "{}", stderr(&missing));
    assert!(
        stderr(&missing).contains("profile strategy is required"),
        "{}",
        stderr(&missing)
    );

    let bogus = run_acb(
        &env,
        &[
            "profile",
            "add",
            source.to_str().unwrap(),
            "--strategy",
            "magic",
        ],
    );
    assert_eq!(bogus.status.code(), Some(2), "{}", stderr(&bogus));
    assert!(
        stderr(&bogus).contains("unsupported profile strategy: magic"),
        "{}",
        stderr(&bogus)
    );

    let store = run_acb(
        &env,
        &[
            "profile",
            "add",
            source.to_str().unwrap(),
            "--strategy",
            "store",
        ],
    );
    assert_eq!(store.status.code(), Some(2), "{}", stderr(&store));
    assert!(
        stderr(&store).contains("store strategy requires the payload under /nix/store"),
        "{}",
        stderr(&store)
    );

    let seed = run_acb(&env, &["profile", "seed", source.to_str().unwrap()]);
    assert_eq!(seed.status.code(), Some(2), "{}", stderr(&seed));
    assert!(
        stderr(&seed).contains("profile strategy is required"),
        "{}",
        stderr(&seed)
    );
}

#[test]
fn copy_snapshot_detaches_from_mutated_source() {
    let temp = TempDir::new("detach");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let source = payload(temp.path(), "detach");
    let id = add(&profile, &config, &source, "detach");
    let snapshot = generation_dir(&profile, &id).join("root");
    let original = fs::read_to_string(snapshot.join("harnesses/shared/tools.mjs")).expect("tools");
    let original_bridge = fs::read_to_string(snapshot.join("bin/acb-bridge")).expect("bridge");

    // 源目录在 add 之后被下一次构建改掉：快照必须保持原字节。
    fs::write(
        source.join("harnesses/shared/tools.mjs"),
        "export const TOOLS = [\"mutated\"]\n",
    )
    .expect("mutate harness");
    fs::write(source.join("bin/acb-bridge"), "#!/bin/sh\nexit 9\n").expect("mutate bridge");
    assert_eq!(
        fs::read_to_string(snapshot.join("harnesses/shared/tools.mjs")).expect("snapshot tools"),
        original
    );
    assert_eq!(
        fs::read_to_string(snapshot.join("bin/acb-bridge")).expect("snapshot bridge"),
        original_bridge
    );

    let env = profile_env(&profile, &config);
    let switched = run_acb(&env, &["profile", "switch", &id]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    let current = run_acb(&env, &["profile", "current"]);
    assert!(current.status.success(), "{}", stderr(&current));
    assert!(
        stdout(&current).contains("payload: copy snapshot"),
        "{}",
        stdout(&current)
    );

    // 快照自身完好（退出 0），同时能报告源目录已经漂移。
    let verify = run_acb(&env, &["profile", "verify"]);
    assert!(verify.status.success(), "{}", stderr(&verify));
    let text = stdout(&verify);
    assert!(text.contains("verify: snapshot ok"), "{text}");
    assert!(text.contains("source differs"), "{text}");
    assert!(text.contains("2 changed"), "{text}");
}

#[test]
fn verify_detects_snapshot_corruption() {
    let temp = TempDir::new("verify-corrupt");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let id = add(&profile, &config, &payload(temp.path(), "corrupt"), "corrupt");
    let snapshot = generation_dir(&profile, &id).join("root");
    fs::write(
        snapshot.join("harnesses/shared/tools.mjs"),
        "export const TOOLS = [\"tampered\"]\n",
    )
    .expect("tamper harness");
    fs::remove_file(snapshot.join("bin/acb-bridge")).expect("drop bridge");

    let env = profile_env(&profile, &config);
    let verify = run_acb(&env, &["profile", "verify", &id]);
    assert_eq!(verify.status.code(), Some(1), "{}", stderr(&verify));
    let text = stdout(&verify);
    assert!(
        text.contains("snapshot damaged: 1 changed, 1 missing, 0 extra"),
        "{text}"
    );
    assert!(
        text.contains("snapshot changed harnesses/shared/tools.mjs"),
        "{text}"
    );
    assert!(text.contains("snapshot missing bin/acb-bridge"), "{text}");
    assert!(text.contains("source unchanged"), "{text}");
}

fn make_legacy_generation(profile: &Path, id: &str, source: &Path) {
    let generation = generation_dir(profile, id);
    fs::create_dir_all(&generation).expect("legacy generation dir");
    let canonical = fs::canonicalize(source).expect("canonical legacy source");
    let meta = serde_json::json!({
        "schema": 1,
        "id": id,
        "created_at_ms": 1,
        "source": {"kind": "path", "path": canonical.to_string_lossy()},
    });
    fs::write(
        generation.join("meta.json"),
        serde_json::to_string_pretty(&meta).expect("serialize legacy meta"),
    )
    .expect("legacy meta");
    std::os::unix::fs::symlink(&canonical, generation.join("root")).expect("legacy root symlink");
}

#[test]
fn legacy_generation_still_switches_and_migrates_to_copy() {
    let temp = TempDir::new("legacy");
    let profile = temp.path().join("profile");
    let config = temp.path().join("config.toml");
    fs::write(&config, "[endpoint]\n").expect("config");
    let source = payload(temp.path(), "legacy");
    fs::create_dir_all(&profile).expect("profile");
    make_legacy_generation(&profile, "gen-legacy", &source);
    let env = profile_env(&profile, &config);

    // 旧代（无 payload 字段）必须照常可切、可运行。
    let switched = run_acb(&env, &["profile", "switch", "gen-legacy"]);
    assert!(switched.status.success(), "{}", stderr(&switched));
    assert_eq!(
        read_link(&profile),
        PathBuf::from("generations/gen-legacy/root")
    );
    assert!(profile.join("current/bin/acb-bridge").is_file());
    let current = run_acb(&env, &["profile", "current"]);
    assert!(current.status.success(), "{}", stderr(&current));
    assert!(
        stdout(&current).contains("payload: - (legacy symlink)"),
        "{}",
        stdout(&current)
    );
    let verify_legacy = run_acb(&env, &["profile", "verify"]);
    assert!(verify_legacy.status.success(), "{}", stderr(&verify_legacy));
    assert!(
        stdout(&verify_legacy).contains("legacy symlink generation"),
        "{}",
        stdout(&verify_legacy)
    );

    // 显式迁移：原地转 copy，id/current 不变。
    let migrate = run_acb(&env, &["profile", "migrate", "--to", "copy"]);
    assert!(migrate.status.success(), "{}", stderr(&migrate));
    assert!(
        stdout(&migrate).contains("migrated gen-legacy -> copy"),
        "{}",
        stdout(&migrate)
    );
    assert_eq!(
        read_link(&profile),
        PathBuf::from("generations/gen-legacy/root"),
        "migrate must not move current"
    );
    let migrated_root = generation_dir(&profile, "gen-legacy").join("root");
    assert!(
        !fs::symlink_metadata(&migrated_root)
            .expect("migrated root")
            .file_type()
            .is_symlink()
    );
    let meta: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(generation_dir(&profile, "gen-legacy").join("meta.json")).expect("meta"),
    )
    .expect("parse migrated meta");
    assert_eq!(meta["payload"]["strategy"], "copy");
    assert!(meta["payload"]["snapshot"]["files"].as_u64().unwrap() > 0);

    fs::write(
        source.join("harnesses/shared/tools.mjs"),
        "export const TOOLS = [\"after-migrate\"]\n",
    )
    .expect("mutate migrated source");
    let verify = run_acb(&env, &["profile", "verify", "gen-legacy"]);
    assert!(verify.status.success(), "{}", stderr(&verify));
    let text = stdout(&verify);
    assert!(text.contains("verify: snapshot ok"), "{text}");
    assert!(text.contains("source differs"), "{text}");

    let back = run_acb(&env, &["profile", "migrate", "--to", "store"]);
    assert_eq!(back.status.code(), Some(2), "{}", stderr(&back));
    assert!(
        stderr(&back).contains("only copy is supported"),
        "{}",
        stderr(&back)
    );
}
