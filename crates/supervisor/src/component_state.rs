//! 组件状态持久化（契约 §5.7）。
//!
//! 状态文件位置是 `<state>/components/<name>.json`：`state_dir` 通常以
//! `supervisor` 结尾（`<state>/supervisor`），此时组件状态与日志放在它的父目录，
//! 即 `<state>/components` 与 `<state>/logs`。计数在配置 sha256 不变时跨启动保留；
//! 配置一变就清零，避免把旧失败累计算到新配置上。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::component::{ComponentTable, LogTarget, ReadinessKind, ResolvedComponent};

pub const COMPONENTS_DIR: &str = "components";
pub const LOGS_DIR: &str = "logs";
pub const STATE_SCHEMA: u32 = 1;

/// 组件状态与日志所在的根目录。
///
/// `state_dir` 正常是 `<state>/supervisor`，此时根目录取 `<state>`，
/// 于是状态落在 `<state>/components`、日志落在 `<state>/logs`。
/// 自定义 `state_dir` 不以 `supervisor` 结尾时，直接把它当根目录。
pub fn state_root(state_dir: &Path) -> PathBuf {
    let is_supervisor = state_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "supervisor");
    if is_supervisor {
        state_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state_dir.to_path_buf())
    } else {
        state_dir.to_path_buf()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentState {
    pub schema: u32,
    pub name: String,
    /// 见 [`crate::component_runtime::Phase`]；P2 第二半起会写真实阶段。
    pub phase: String,
    pub restart_policy: String,
    pub enabled: bool,
    pub attempts: u32,
    pub consecutive_failures: u32,
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    /// 声明这份组件的 supervisor.toml 的 sha256。
    pub config_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_url: Option<String>,
    pub updated_at_ms: i64,
}

impl ComponentState {
    pub fn path(state_dir: &Path, name: &str) -> PathBuf {
        state_root(state_dir)
            .join(COMPONENTS_DIR)
            .join(format!("{name}.json"))
    }

    pub fn load(state_dir: &Path, name: &str) -> Result<Option<Self>, String> {
        let path = Self::path(state_dir, name);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| format!("parse {}: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }
}

/// 把每个已声明组件的状态写到 `state_dir/components/<name>.json`。
///
/// 表为空时什么都不做（不创建空目录）。返回写入的条目数。
pub fn sync(
    state_dir: &Path,
    table: &ComponentTable,
    instance_id: Option<&str>,
    config_sha256: &str,
    now: i64,
) -> Result<usize, String> {
    if table.is_empty() {
        return Ok(0);
    }

    let dir = state_root(state_dir).join(COMPONENTS_DIR);
    fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;

    let mut names = Vec::new();
    for entry in &table.components {
        if !names.iter().any(|name| name == &entry.name) {
            names.push(entry.name.clone());
        }
    }

    for name in &names {
        let entry = effective_entry(table, name);
        let previous = ComponentState::load(state_dir, name)?;
        let state = build_state(entry, previous.as_ref(), instance_id, config_sha256, now);
        write_state(state_dir, &state)?;
    }
    Ok(names.len())
}

/// 同名多条目时以 enabled 的那条为准；否则取第一条。
fn effective_entry<'a>(table: &'a ComponentTable, name: &str) -> &'a ResolvedComponent {
    table
        .components
        .iter()
        .find(|entry| entry.name == name && entry.enabled)
        .or_else(|| table.components.iter().find(|entry| entry.name == name))
        .expect("name was collected from the table")
}

fn build_state(
    entry: &ResolvedComponent,
    previous: Option<&ComponentState>,
    instance_id: Option<&str>,
    config_sha256: &str,
    now: i64,
) -> ComponentState {
    let previous = previous.filter(|state| state.config_sha256 == config_sha256);

    ComponentState {
        schema: STATE_SCHEMA,
        name: entry.name.clone(),
        // P2 不 spawn，任何组件都不会进入 running。
        phase: "stopped".to_string(),
        restart_policy: entry.restart.policy.to_string(),
        enabled: entry.enabled,
        attempts: previous.map_or(0, |state| state.attempts),
        consecutive_failures: previous.map_or(0, |state| state.consecutive_failures),
        restarts: previous.map_or(0, |state| state.restarts),
        pid: None,
        instance_id: instance_id.map(str::to_string),
        config_sha256: config_sha256.to_string(),
        last_error: previous.and_then(|state| state.last_error.clone()),
        ready_at_ms: previous.and_then(|state| state.ready_at_ms),
        failed_at_ms: previous.and_then(|state| state.failed_at_ms),
        log_path: log_path(entry),
        last_run_at_ms: previous.and_then(|state| state.last_run_at_ms),
        last_result: previous.and_then(|state| state.last_result.clone()),
        probe_url: probe_url(entry),
        updated_at_ms: now,
    }
}

fn log_path(entry: &ResolvedComponent) -> Option<String> {
    match entry.log.target {
        LogTarget::File => entry.log.path.clone(),
        LogTarget::Journald => None,
    }
}

pub(crate) fn probe_url(entry: &ResolvedComponent) -> Option<String> {
    match entry.readiness.kind {
        ReadinessKind::Http => Some(format!(
            "http://{}{}",
            entry.readiness.endpoint(),
            entry.readiness.path
        )),
        ReadinessKind::Tcp => Some(format!("tcp://{}", entry.readiness.endpoint())),
        // `exit` 只看退出码，没有可探测的端点。
        ReadinessKind::Process | ReadinessKind::Exit => None,
    }
}

/// 写入单个组件状态文件（`acbctl component reset` 用）。
///
/// 只落盘，不 spawn、不停进程、不改配置；目录不存在时补建。
pub fn save(state_dir: &Path, state: &ComponentState) -> Result<(), String> {
    let dir = state_root(state_dir).join(COMPONENTS_DIR);
    fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    write_state(state_dir, state)
}

pub(crate) fn write_state(state_dir: &Path, state: &ComponentState) -> Result<(), String> {
    let path = ComponentState::path(state_dir, &state.name);
    let text = serde_json::to_string_pretty(state)
        .map_err(|error| format!("serialize {}: {error}", path.display()))?
        + "\n";
    write_atomic(&path, text.as_bytes())
}

fn write_atomic(path: &Path, content: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid file name: {}", path.display()))?;
    let temp = parent.join(format!(".{file_name}.acb-tmp-{}", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(|error| format!("write {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{PresetRegistry, parse_config_file};

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "acb-component-state-{label}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn table(text: &str) -> ComponentTable {
        let (_file, table) = parse_config_file(text, &PresetRegistry::empty()).expect("parse");
        table
    }

    #[test]
    fn disabled_components_are_written_as_stopped() {
        let dir = temp_dir("stopped");
        let table = table(
            r#"
[supervisor]
program = "/bin/acb-bridge"

[[component]]
name = "dsh"
command = ["/bin/dsh"]
[component.readiness]
kind = "tcp"
port = 3080
"#,
        );
        let count = sync(&dir, &table, Some("inst-1"), "sha-a", 42).unwrap();
        assert_eq!(count, 1);

        let state = ComponentState::load(&dir, "dsh").unwrap().expect("state");
        assert_eq!(state.phase, "stopped");
        assert!(!state.enabled, "默认 enabled 必须是 false");
        assert_eq!(state.pid, None);
        assert_eq!(state.attempts, 0);
        assert_eq!(state.config_sha256, "sha-a");
        assert_eq!(state.instance_id.as_deref(), Some("inst-1"));
        assert_eq!(state.probe_url.as_deref(), Some("tcp://127.0.0.1:3080"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn enabled_components_are_still_not_spawned_in_p2() {
        let dir = temp_dir("no-spawn");
        let table = table(
            r#"
[supervisor]
program = "/bin/acb-bridge"

[[component]]
name = "dsh"
command = ["/bin/dsh"]
enabled = true
[component.readiness]
kind = "tcp"
port = 3080
"#,
        );
        sync(&dir, &table, None, "sha", 7).unwrap();
        let state = ComponentState::load(&dir, "dsh").unwrap().expect("state");
        assert!(state.enabled);
        assert_eq!(state.phase, "stopped");
        assert_eq!(state.pid, None, "P2 不 spawn，任何组件都不得有 pid");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn counters_survive_only_when_config_sha_is_unchanged() {
        let dir = temp_dir("counters");
        let table = table(
            r#"
[supervisor]
program = "/bin/acb-bridge"

[[component]]
name = "dsh"
command = ["/bin/dsh"]
[component.readiness]
kind = "tcp"
port = 3080
"#,
        );
        sync(&dir, &table, None, "sha-a", 1).unwrap();
        let mut state = ComponentState::load(&dir, "dsh").unwrap().expect("state");
        state.attempts = 4;
        state.consecutive_failures = 2;
        state.last_result = Some("failed".to_string());
        write_state(&dir, &state).unwrap();

        sync(&dir, &table, None, "sha-a", 2).unwrap();
        let kept = ComponentState::load(&dir, "dsh").unwrap().expect("state");
        assert_eq!(kept.attempts, 4);
        assert_eq!(kept.consecutive_failures, 2);

        sync(&dir, &table, None, "sha-b", 3).unwrap();
        let reset = ComponentState::load(&dir, "dsh").unwrap().expect("state");
        assert_eq!(reset.attempts, 0);
        assert_eq!(reset.consecutive_failures, 0);
        assert_eq!(reset.last_result, None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_table_does_not_create_components_dir() {
        let dir = temp_dir("empty");
        let count = sync(&dir, &ComponentTable::default(), None, "sha", 1).unwrap();
        assert_eq!(count, 0);
        assert!(!dir.join(COMPONENTS_DIR).exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn supervisor_subdir_states_land_beside_it() {
        let dir = temp_dir("layout");
        let state_dir = dir.join("state").join("supervisor");
        fs::create_dir_all(&state_dir).expect("create state dir");
        let table = table(
            r#"
[supervisor]
program = "/bin/acb-bridge"

[[component]]
name = "dsh"
command = ["/bin/dsh"]
[component.readiness]
kind = "tcp"
port = 3080
"#,
        );
        sync(&state_dir, &table, None, "sha", 1).unwrap();
        assert!(dir.join("state/components/dsh.json").exists());
        assert!(!state_dir.join("components/dsh.json").exists());
        fs::remove_dir_all(&dir).ok();
    }
}
