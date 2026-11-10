//! `acbctl sessions`：从 kernel 配置推导每个会话的 agent preset 名与 MCP 端口。
//!
//! 推导规则是单一的：
//!   - preset 名 = `acb-<session id>`
//!   - MCP 端口 = 会话显式 `driver_port`；旧配置缺省时回落到 `3101 + 下标` 并告警
//!
//! `dsh-presets` 与 `drivers` 两个组件脚本都消费这份输出，避免两处各拼一套名字。
//! 输出按行制表符分隔：`id\tharness\tpreset\tport`，不带表头，便于脚本过滤。
//!
//! 另外提供两个只维护 dsh 会话存档的子命令：
//!   - `scan`：按“最终投影”枚举会话的 preset，并用 `--known` 标出已不在预设表里的值；
//!   - `migrate-preset`：按显式 `--map 旧=新` 迁移存档。迁移只改会话日志，不读取
//!     SQLite，也不碰 `profile/current` 指针。
//!
//! dsh 的 resume 读的是 `agentPreset` 投影，不是 header 里的创建值；已运行进程还会
//! 把投影缓存留在内存里。因此迁移同时做两件事：把 header 里的旧名改掉（清理存量
//! 字符串），再追加一条 `agent-preset/selected` 事件（让投影立即前进，不必重启 dsh）。

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 78;
/// 会话被正在运行的 dsh 占用；迁移本身没错，只是现在不能写，稍后重跑即可。
const EXIT_TEMPFAIL: u8 = 75;

const DEFAULT_CONFIG: &str = "config.toml";
const PORT_BASE: u16 = 3101;

const SESSION_FORMAT_VERSION: u64 = 4;
const AGENT_PRESET_KEY: &str = "agentPreset";
const AGENT_PRESET_EVENT: &str = "agent-preset/selected";

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("scan") => scan_command(&args[1..]),
        Some("migrate-preset") => migrate_command(&args[1..]),
        Some("set-port") => set_port_command(&args[1..]),
        _ => derive_command(args),
    }
}

/// `sessions set-port <id> <port>`：把显式端口写回库里的会话对象。
///
/// 只改 `config_objects` 的 session payload（TOML 原文），不碰文件；写前校验端口
/// 范围和唯一性，写后重新加载一次配置确认可解析，失败就回滚旧文本。
fn set_port_command(args: &[String]) -> Result<ExitCode, String> {
    let mut config = None;
    let mut config_dir = None;
    let mut id = None;
    let mut port = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                config = Some(PathBuf::from(next(args, &mut index, "--config")?));
            }
            "--config-dir" => {
                config_dir = Some(PathBuf::from(next(args, &mut index, "--config-dir")?));
            }
            "-h" | "--help" | "help" => {
                println!(
                    "Usage:\n  acbctl sessions set-port <session-id> <port> [--config PATH] [--config-dir DIR]"
                );
                return Ok(ExitCode::SUCCESS);
            }
            other if other.starts_with('-') => {
                eprintln!("acbctl sessions set-port: unknown option: {other}");
                return Ok(ExitCode::from(EXIT_USAGE));
            }
            other => {
                if id.is_none() {
                    id = Some(other.to_string());
                } else if port.is_none() {
                    port = Some(other.to_string());
                } else {
                    eprintln!("acbctl sessions set-port: unexpected argument: {other}");
                    return Ok(ExitCode::from(EXIT_USAGE));
                }
            }
        }
        index += 1;
    }
    let (Some(id), Some(port)) = (id, port) else {
        eprintln!("acbctl sessions set-port: need <session-id> and <port>");
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    let port: u16 = match port.parse() {
        Ok(port) if port >= 1024 => port,
        Ok(port) => {
            eprintln!("acbctl sessions set-port: port {port} is below 1024");
            return Ok(ExitCode::from(EXIT_USAGE));
        }
        Err(_) => {
            eprintln!("acbctl sessions set-port: invalid port {port:?}");
            return Ok(ExitCode::from(EXIT_USAGE));
        }
    };

    let options = Options {
        config,
        config_dir,
        help: false,
    };
    let desired = options.desired_config();
    let dir = options
        .config_dir
        .clone()
        .unwrap_or_else(|| crate::config_source::default_config_dir(&desired));
    let loaded = crate::config_source::load(&desired, Some(&dir))?;
    let Some(session) = loaded
        .config
        .sessions
        .iter()
        .find(|session| session.id == id)
    else {
        eprintln!("acbctl sessions set-port: no such session: {id}");
        return Ok(ExitCode::from(EXIT_CONFIG));
    };
    if !matches!(session.harness.as_str(), "dsh") {
        eprintln!(
            "acbctl sessions set-port: session {id} uses harness {}; driver_port only applies to dsh",
            session.harness
        );
        return Ok(ExitCode::from(EXIT_USAGE));
    }
    if let Some(other) = loaded
        .config
        .sessions
        .iter()
        .find(|other| other.id != id && other.driver_port == Some(port))
    {
        eprintln!(
            "acbctl sessions set-port: port {port} is already used by session {}",
            other.id
        );
        return Ok(ExitCode::from(EXIT_USAGE));
    }

    let store_path = loaded.config.storage.path.clone();
    let mut store = acb_kernel::SqliteStore::open(&store_path)
        .map_err(|error| format!("open store {store_path}: {error}"))?;
    use acb_kernel::storage::ConfigObjectsStore;
    let Some(objects) = store
        .config_objects_load()
        .map_err(|error| format!("read config objects: {error}"))?
    else {
        eprintln!(
            "acbctl sessions set-port: config objects are not initialized in {store_path}; start the bridge once first"
        );
        return Ok(ExitCode::from(EXIT_CONFIG));
    };
    let Some((position, object)) = objects
        .sessions
        .iter()
        .enumerate()
        .find(|(_, object)| object.id == id)
    else {
        eprintln!(
            "acbctl sessions set-port: session {id} exists in the merged config but not in config_objects"
        );
        return Ok(ExitCode::from(EXIT_CONFIG));
    };
    let old_toml = object.toml.clone();
    let new_toml = match with_driver_port(&old_toml, port) {
        Ok(text) => text,
        Err(message) => {
            eprintln!("acbctl sessions set-port: {message}");
            return Ok(ExitCode::from(EXIT_CONFIG));
        }
    };
    let at = now_ms() as i64;
    if let Err(error) = store.config_object_put(
        acb_kernel::ConfigObjectKind::Session,
        &id,
        &new_toml,
        position as i64,
        at,
    ) {
        eprintln!("acbctl sessions set-port: write config object: {error}");
        return Ok(ExitCode::from(EXIT_CONFIG));
    }
    // 写后重载一次，确认整份配置仍能解析；坏文本不能留在库里。
    if let Err(error) = crate::config_source::load(&desired, Some(&dir)) {
        let _ = store.config_object_put(
            acb_kernel::ConfigObjectKind::Session,
            &id,
            &old_toml,
            position as i64,
            at,
        );
        eprintln!("acbctl sessions set-port: reload failed, rolled back: {error}");
        return Ok(ExitCode::from(EXIT_CONFIG));
    }
    println!(
        "session {id}: driver_port = {port} (written to {store_path}; next turn/driver reconcile picks it up)"
    );
    Ok(ExitCode::SUCCESS)
}

/// 在 `[[sessions]]` 原文里设置 `driver_port`。
///
/// TOML 顶层键必须放在任何子表之前；这里固定插在表头后的第一行，先删掉已有的
/// `driver_port` 行，避免重复键。
fn with_driver_port(text: &str, port: u16) -> Result<String, String> {
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| "session payload is empty".to_string())?;
    if !header.trim_start().starts_with("[[sessions]]") {
        return Err(format!(
            "session payload does not start with [[sessions]]: {header:?}"
        ));
    }
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');
    out.push_str(&format!("driver_port = {port}\n"));
    for line in lines {
        if line.trim_start().starts_with("driver_port") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

fn derive_command(args: &[String]) -> Result<ExitCode, String> {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("acbctl sessions: {message}");
            return Ok(ExitCode::from(EXIT_USAGE));
        }
    };
    if options.help {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    let loaded = match load(&options) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("acbctl sessions: {message}");
            return Ok(ExitCode::from(EXIT_CONFIG));
        }
    };

    for (index, session) in loaded.config.sessions.iter().enumerate() {
        let port = match session.driver_port {
            Some(port) => port,
            None => {
                let Some(port) = PORT_BASE.checked_add(index as u16) else {
                    eprintln!(
                        "acbctl sessions: too many sessions; port {} overflows u16",
                        PORT_BASE as u32 + index as u32
                    );
                    return Ok(ExitCode::from(EXIT_CONFIG));
                };
                if session.harness == "dsh" {
                    eprintln!(
                        "acbctl sessions: session {} has no driver_port; using legacy index port {} (add driver_port to the session to make it stable)",
                        session.id, port
                    );
                }
                port
            }
        };
        println!(
            "{}\t{}\t{}\t{}",
            session.id,
            session.harness,
            preset_name(&session.id),
            port
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// 会话 → agent preset 名的唯一规则。
pub fn preset_name(session_id: &str) -> String {
    format!("acb-{session_id}")
}

fn load(options: &Options) -> Result<acb_kernel::LoadedConfig, String> {
    let config = options.desired_config();
    let config_dir = options
        .config_dir
        .clone()
        .unwrap_or_else(|| crate::config_source::default_config_dir(&config));
    // 会话清单以库为准（schema 15）：只读文件会拿到空清单，驱动一个都不起。
    crate::config_source::load(&config, Some(&config_dir))
}

struct Options {
    config: Option<PathBuf>,
    config_dir: Option<PathBuf>,
    help: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            config: None,
            config_dir: None,
            help: false,
        };
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--config" => {
                    options.config = Some(PathBuf::from(next(args, &mut index, "--config")?));
                }
                "--config-dir" => {
                    options.config_dir =
                        Some(PathBuf::from(next(args, &mut index, "--config-dir")?));
                }
                "-h" | "--help" | "help" => options.help = true,
                other if other.starts_with("--config=") => {
                    let value = other.trim_start_matches("--config=");
                    if value.is_empty() {
                        return Err("--config requires a value".to_string());
                    }
                    options.config = Some(PathBuf::from(value));
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option: {other}"));
                }
                other => return Err(format!("unexpected argument: {other}")),
            }
            index += 1;
        }
        Ok(options)
    }

    fn desired_config(&self) -> PathBuf {
        self.config
            .clone()
            .or_else(|| {
                std::env::var("ACB_CONFIG_FILE")
                    .ok()
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))
    }
}

fn next(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn print_help() {
    println!(
        "Usage:
  acbctl sessions [--config PATH] [--config-dir DIR]
  acbctl sessions set-port <session-id> <port> [--config PATH] [--config-dir DIR]
  acbctl sessions scan [--dsh-home DIR] [--known PRESET]... [--json]
  acbctl sessions migrate-preset --map OLD=NEW [--map OLD=NEW]... [--dsh-home DIR] [--apply]

Derives one row per [[sessions]] entry: id<TAB>harness<TAB>preset<TAB>port
  preset = acb-<id>, port = session.driver_port (legacy fallback: 3101 + index)

scan enumerates stored dsh sessions by their effective agentPreset projection.
  --known marks presets that no longer exist in the current preset table.
  --json emits the full per-session detail.

migrate-preset renames the preset in stored dsh sessions. The mapping is explicit
  (no guessing); without --apply it is a dry run. Each changed archive is backed up
  beside the original as <file>.bak.<unix-ms>. The migration rewrites the header's
  agentPreset and appends an agent-preset/selected event, so a running dsh picks up
  the new value without a restart. Sessions currently held by dsh are skipped.
  Exit codes: 0 ok, 75 skipped-in-use (rerun later), 78 failure, 2 usage."
    );
}

// ---------------------------------------------------------------------------
// scan
// ---------------------------------------------------------------------------

struct ScanOptions {
    dsh_home: Option<PathBuf>,
    known: BTreeSet<String>,
    json: bool,
    help: bool,
}

impl ScanOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            dsh_home: None,
            known: BTreeSet::new(),
            json: false,
            help: false,
        };
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "--dsh-home" => {
                    options.dsh_home = Some(PathBuf::from(next(args, &mut index, "--dsh-home")?));
                }
                "--known" => {
                    options.known.insert(next(args, &mut index, "--known")?);
                }
                "--json" => options.json = true,
                "-h" | "--help" | "help" => options.help = true,
                other if other.starts_with("--dsh-home=") => {
                    let value = other.trim_start_matches("--dsh-home=");
                    if value.is_empty() {
                        return Err("--dsh-home requires a value".to_string());
                    }
                    options.dsh_home = Some(PathBuf::from(value));
                }
                other if other.starts_with("--known=") => {
                    let value = other.trim_start_matches("--known=");
                    if value.is_empty() {
                        return Err("--known requires a value".to_string());
                    }
                    options.known.insert(value.to_string());
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option: {other}"));
                }
                other => return Err(format!("unexpected argument: {other}")),
            }
            index += 1;
        }
        Ok(options)
    }
}

fn scan_command(args: &[String]) -> Result<ExitCode, String> {
    let options = match ScanOptions::parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("acbctl sessions scan: {message}");
            return Ok(ExitCode::from(EXIT_USAGE));
        }
    };
    if options.help {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }
    let dsh_home = match resolve_dsh_home(options.dsh_home.clone()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("acbctl sessions scan: {message}");
            return Ok(ExitCode::from(EXIT_CONFIG));
        }
    };
    let inventory = match collect_inventory(&dsh_home) {
        Ok(inventory) => inventory,
        Err(message) => {
            eprintln!("acbctl sessions scan: {message}");
            return Ok(ExitCode::from(EXIT_CONFIG));
        }
    };

    if options.json {
        print_scan_json(&dsh_home, &inventory, &options.known)?;
    } else {
        print_scan_human(&dsh_home, &inventory, &options.known);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_scan_human(dsh_home: &Path, inventory: &Inventory, known: &BTreeSet<String>) {
    let mut rows: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for session in &inventory.sessions {
        let preset = session
            .effective_preset
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        rows.entry(preset).or_default().0 += 1;
    }
    for cache in &inventory.caches {
        let preset = cache.preset.clone().unwrap_or_else(|| "(none)".to_string());
        rows.entry(preset).or_default().1 += 1;
    }

    println!("DSH home: {}", dsh_home.display());
    println!("PRESET\tARCHIVES\tCACHES\tSTATUS");
    for (preset, (archives, caches)) in rows {
        let status = if known.is_empty() {
            ""
        } else if known.contains(&preset) {
            "known"
        } else {
            "unknown"
        };
        println!("{preset}\t{archives}\t{caches}\t{status}");
    }
    let orphans = inventory
        .caches
        .iter()
        .filter(|cache| !cache.has_archive)
        .count();
    let corrupt = inventory
        .sessions
        .iter()
        .filter(|session| session.error.is_some())
        .count();
    println!("orphan cache records (no archive): {orphans}");
    println!("corrupt archives: {corrupt}");
}

fn print_scan_json(
    dsh_home: &Path,
    inventory: &Inventory,
    known: &BTreeSet<String>,
) -> Result<(), String> {
    let mut rows: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for session in &inventory.sessions {
        let preset = session
            .effective_preset
            .clone()
            .unwrap_or_else(|| "(none)".to_string());
        rows.entry(preset).or_default().0 += 1;
    }
    for cache in &inventory.caches {
        let preset = cache.preset.clone().unwrap_or_else(|| "(none)".to_string());
        rows.entry(preset).or_default().1 += 1;
    }

    let presets: Vec<Value> = rows
        .iter()
        .map(|(preset, (archives, caches))| {
            json!({
                "preset": preset,
                "archives": archives,
                "caches": caches,
                "unknown": if known.is_empty() { Value::Null } else { Value::Bool(!known.contains(preset)) },
            })
        })
        .collect();
    let sessions: Vec<Value> = inventory
        .sessions
        .iter()
        .map(|session| {
            json!({
                "id": &session.id,
                "cwd": &session.cwd,
                "headerPreset": &session.header_preset,
                "effectivePreset": &session.effective_preset,
                "archive": session.archive.display().to_string(),
                "inUse": session.in_use,
                "error": &session.error,
            })
        })
        .collect();
    let caches: Vec<Value> = inventory
        .caches
        .iter()
        .map(|cache| {
            json!({
                "id": &cache.id,
                "preset": &cache.preset,
                "path": cache.path.display().to_string(),
                "hasArchive": cache.has_archive,
                "error": &cache.error,
            })
        })
        .collect();

    let payload = json!({
        "dshHome": dsh_home.display().to_string(),
        "known": known,
        "presets": presets,
        "sessions": sessions,
        "caches": caches,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(|error| format!("encode json: {error}"))?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// migrate-preset
// ---------------------------------------------------------------------------

struct MigrateOptions {
    dsh_home: Option<PathBuf>,
    maps: BTreeMap<String, String>,
    apply: bool,
    help: bool,
}

impl MigrateOptions {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            dsh_home: None,
            maps: BTreeMap::new(),
            apply: false,
            help: false,
        };
        let mut index = 0;
        while index < args.len() {
            let arg = args[index].as_str();
            match arg {
                "--dsh-home" => {
                    options.dsh_home = Some(PathBuf::from(next(args, &mut index, "--dsh-home")?));
                }
                "--map" => {
                    let value = next(args, &mut index, "--map")?;
                    options.insert_mapping(&value)?;
                }
                "--apply" => options.apply = true,
                "-h" | "--help" | "help" => options.help = true,
                other if other.starts_with("--dsh-home=") => {
                    let value = other.trim_start_matches("--dsh-home=");
                    if value.is_empty() {
                        return Err("--dsh-home requires a value".to_string());
                    }
                    options.dsh_home = Some(PathBuf::from(value));
                }
                other if other.starts_with("--map=") => {
                    let value = other.trim_start_matches("--map=");
                    options.insert_mapping(value)?;
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option: {other}"));
                }
                other => return Err(format!("unexpected argument: {other}")),
            }
            index += 1;
        }
        Ok(options)
    }

    fn insert_mapping(&mut self, value: &str) -> Result<(), String> {
        let (old, new) = value
            .split_once('=')
            .ok_or_else(|| format!("--map expects OLD=NEW, got {value:?}"))?;
        if old.is_empty() || new.is_empty() {
            return Err(format!("--map expects non-empty OLD=NEW, got {value:?}"));
        }
        if old == new {
            return Err(format!("--map OLD and NEW must differ, got {value:?}"));
        }
        if let Some(previous) = self.maps.insert(old.to_string(), new.to_string()) {
            if previous != new {
                return Err(format!(
                    "--map conflict for {old:?}: {previous:?} vs {new:?}"
                ));
            }
        }
        Ok(())
    }
}

fn migrate_command(args: &[String]) -> Result<ExitCode, String> {
    let options = match MigrateOptions::parse(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("acbctl sessions migrate-preset: {message}");
            return Ok(ExitCode::from(EXIT_USAGE));
        }
    };
    if options.help {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }
    if options.maps.is_empty() {
        eprintln!("acbctl sessions migrate-preset: at least one --map OLD=NEW is required");
        return Ok(ExitCode::from(EXIT_USAGE));
    }
    let dsh_home = match resolve_dsh_home(options.dsh_home.clone()) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("acbctl sessions migrate-preset: {message}");
            return Ok(ExitCode::from(EXIT_CONFIG));
        }
    };
    let inventory = match collect_inventory(&dsh_home) {
        Ok(inventory) => inventory,
        Err(message) => {
            eprintln!("acbctl sessions migrate-preset: {message}");
            return Ok(ExitCode::from(EXIT_CONFIG));
        }
    };

    let now_ms = now_ms();
    let mut migrated = Vec::new();
    let mut migrated_ids = BTreeSet::new();
    let mut attempted_ids = BTreeSet::new();
    let mut blocked_ids = BTreeSet::new();
    let mut skipped_in_use = Vec::new();
    let mut failures = Vec::new();
    let mut corrupt = Vec::new();

    for session in &inventory.sessions {
        if let Some(error) = &session.error {
            corrupt.push(format!("{}: {error}", session.id));
            blocked_ids.insert(session.id.clone());
        }
        let Some(effective) = session.effective_preset.as_deref() else {
            continue;
        };
        let Some(new_effective) = options.maps.get(effective) else {
            continue;
        };
        if let Some(error) = &session.error {
            failures.push(format!("{}: {error}", session.id));
            continue;
        }
        attempted_ids.insert(session.id.clone());

        let header_target = session
            .header_preset
            .as_deref()
            .and_then(|preset| options.maps.get(preset))
            .cloned();
        let seq = session.last_seq.map_or(0, |last| last + 1);

        let lock = match acquire_lock(session.archive.parent().unwrap_or(&dsh_home)) {
            Ok(lock) => lock,
            Err(LockFailure::Busy) => {
                skipped_in_use.push(session.id.clone());
                continue;
            }
            Err(LockFailure::Failure(message)) => {
                failures.push(format!("{}: {message}", session.id));
                continue;
            }
        };

        let bytes = match fs::read(&session.archive) {
            Ok(bytes) => bytes,
            Err(error) => {
                failures.push(format!(
                    "{}: read {}: {error}",
                    session.id,
                    session.archive.display()
                ));
                continue;
            }
        };
        let next = match migrate_bytes(
            &bytes,
            session,
            header_target.as_deref(),
            new_effective,
            seq,
            now_ms,
        ) {
            Ok(next) => next,
            Err(error) => {
                failures.push(format!("{}: {error}", session.id));
                continue;
            }
        };
        let verified = match inspect_archive_bytes(&next, session.compressed) {
            Ok(archive) => archive,
            Err(error) => {
                failures.push(format!("{}: verify rewritten archive: {error}", session.id));
                continue;
            }
        };
        if verified.effective_preset.as_deref() != Some(new_effective.as_str()) {
            failures.push(format!(
                "{}: rewritten archive still resolves preset {:?}",
                session.id, verified.effective_preset
            ));
            continue;
        }

        if !options.apply {
            migrated_ids.insert(session.id.clone());
            migrated.push(MigrationPlan {
                id: session.id.clone(),
                from: effective.to_string(),
                to: new_effective.clone(),
                header_from: if header_target.is_some() {
                    session.header_preset.clone()
                } else {
                    None
                },
                header_to: header_target.clone(),
                seq,
                backup: None,
            });
            drop(lock);
            continue;
        }

        let backup = backup_file(&session.archive, now_ms);
        let backup = match backup {
            Ok(path) => path,
            Err(error) => {
                failures.push(format!("{}: backup: {error}", session.id));
                continue;
            }
        };
        if let Err(error) = write_atomic(&session.archive, &next) {
            failures.push(format!(
                "{}: write {}: {error}",
                session.id,
                session.archive.display()
            ));
            continue;
        }
        migrated_ids.insert(session.id.clone());
        migrated.push(MigrationPlan {
            id: session.id.clone(),
            from: effective.to_string(),
            to: new_effective.clone(),
            header_from: if header_target.is_some() {
                session.header_preset.clone()
            } else {
                None
            },
            header_to: header_target.clone(),
            seq,
            backup: Some(backup),
        });
        drop(lock);
    }

    for id in &attempted_ids {
        if !migrated_ids.contains(id) {
            blocked_ids.insert(id.clone());
        }
    }

    for plan in &migrated {
        let header = match (&plan.header_from, &plan.header_to) {
            (Some(from), Some(to)) => format!(", header {from} -> {to}"),
            _ => String::new(),
        };
        let backup = plan
            .backup
            .as_ref()
            .map(|path| format!(", backup {}", path.display()))
            .unwrap_or_default();
        println!(
            "{}: {} -> {}{header}, appended {AGENT_PRESET_EVENT} seq {}{backup}",
            plan.id, plan.from, plan.to, plan.seq
        );
    }

    // 投影缓存是派生检查点。会话存档迁移成功后同步改掉缓存里的旧名，避免扫描时
    // 仍看到“旧名 20 条”这类残留；存档没迁移成功（占用/损坏）的会话保持原样。
    let mut migrated_caches = Vec::new();
    for cache in &inventory.caches {
        let Some(old) = cache.preset.as_deref() else {
            continue;
        };
        let Some(new) = options.maps.get(old) else {
            continue;
        };
        if cache.has_archive && blocked_ids.contains(&cache.id) {
            continue;
        }
        if let Some(error) = &cache.error {
            failures.push(format!("cache {}: {error}", cache.id));
            continue;
        }
        let bytes = match fs::read(&cache.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                failures.push(format!(
                    "cache {}: read {}: {error}",
                    cache.id,
                    cache.path.display()
                ));
                continue;
            }
        };
        let next = match migrate_cache_bytes(&bytes, old, new) {
            Ok(next) => next,
            Err(error) => {
                failures.push(format!("cache {}: {error}", cache.id));
                continue;
            }
        };
        if !options.apply {
            migrated_caches.push(CacheMigration {
                id: cache.id.clone(),
                from: old.to_string(),
                to: new.clone(),
                orphan: !cache.has_archive,
                backup: None,
            });
            continue;
        }
        let backup = match backup_file(&cache.path, now_ms) {
            Ok(path) => path,
            Err(error) => {
                failures.push(format!("cache {}: backup: {error}", cache.id));
                continue;
            }
        };
        if let Err(error) = write_atomic(&cache.path, &next) {
            failures.push(format!(
                "cache {}: write {}: {error}",
                cache.id,
                cache.path.display()
            ));
            continue;
        }
        match read_cache_preset(&cache.path) {
            Ok(Some(preset)) if preset == *new => {}
            Ok(other) => {
                failures.push(format!(
                    "cache {}: rewritten row is {other:?}, expected {new:?}",
                    cache.id
                ));
                continue;
            }
            Err(error) => {
                failures.push(format!("cache {}: verify: {error}", cache.id));
                continue;
            }
        }
        migrated_caches.push(CacheMigration {
            id: cache.id.clone(),
            from: old.to_string(),
            to: new.clone(),
            orphan: !cache.has_archive,
            backup: Some(backup),
        });
    }

    for plan in &migrated_caches {
        let orphan = if plan.orphan {
            ", orphan record (no archive)"
        } else {
            ""
        };
        let backup = plan
            .backup
            .as_ref()
            .map(|path| format!(", backup {}", path.display()))
            .unwrap_or_default();
        println!(
            "cache {}: {} -> {}{orphan}{backup}",
            plan.id, plan.from, plan.to
        );
    }

    // 只列“header 是要迁移的旧名、但当前投影已经不是映射目标”的会话：这类不能按
    // 旧名重写 header，也不能追加事件覆盖用户后来选择的 preset，必须人工定夺。
    for session in &inventory.sessions {
        let Some(header) = session.header_preset.as_deref() else {
            continue;
        };
        if !options.maps.contains_key(header) {
            continue;
        }
        let Some(effective) = session.effective_preset.as_deref() else {
            continue;
        };
        if options.maps.contains_key(effective) {
            continue;
        }
        println!(
            "left untouched: {} has legacy header {header} but effective preset is {effective}",
            session.id
        );
    }
    for line in &corrupt {
        println!("corrupt archive (skipped): {line}");
    }
    for id in &skipped_in_use {
        println!("skipped (session is in use): {id}");
    }
    for failure in &failures {
        eprintln!("acbctl sessions migrate-preset: {failure}");
    }

    if !options.apply {
        println!(
            "dry run: {} session(s) and {} cache row(s) would be migrated; rerun with --apply to write",
            migrated.len(),
            migrated_caches.len()
        );
    } else {
        println!(
            "migrated {} session(s) and {} cache row(s)",
            migrated.len(),
            migrated_caches.len()
        );
    }
    if !failures.is_empty() {
        return Ok(ExitCode::from(EXIT_CONFIG));
    }
    if !skipped_in_use.is_empty() {
        return Ok(ExitCode::from(EXIT_TEMPFAIL));
    }
    Ok(ExitCode::SUCCESS)
}

struct MigrationPlan {
    id: String,
    from: String,
    to: String,
    header_from: Option<String>,
    header_to: Option<String>,
    seq: u64,
    backup: Option<PathBuf>,
}

struct CacheMigration {
    id: String,
    from: String,
    to: String,
    orphan: bool,
    backup: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// dsh session inventory
// ---------------------------------------------------------------------------

struct Inventory {
    sessions: Vec<StoredSession>,
    caches: Vec<CacheRecord>,
}

struct StoredSession {
    id: String,
    cwd: Option<String>,
    header_preset: Option<String>,
    effective_preset: Option<String>,
    last_seq: Option<u64>,
    first_frame_size: usize,
    first_frame_plain: Vec<u8>,
    ends_with_newline: bool,
    compressed: bool,
    in_use: bool,
    archive: PathBuf,
    error: Option<String>,
}

struct CacheRecord {
    id: String,
    preset: Option<String>,
    path: PathBuf,
    has_archive: bool,
    error: Option<String>,
}

fn resolve_dsh_home(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    let path = explicit
        .or_else(|| std::env::var_os("DSH_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".dsh"))
        })
        .ok_or_else(|| "cannot resolve DSH home; pass --dsh-home".to_string())?;
    if !path.is_dir() {
        return Err(format!("DSH home {} is not a directory", path.display()));
    }
    Ok(path)
}

fn collect_inventory(dsh_home: &Path) -> Result<Inventory, String> {
    let sessions_root = dsh_home.join("sessions");
    if !sessions_root.is_dir() {
        return Ok(Inventory {
            sessions: Vec::new(),
            caches: Vec::new(),
        });
    }

    let mut sessions = Vec::new();
    for cwd_entry in read_dir_sorted(&sessions_root)? {
        if !cwd_entry.path().is_dir() {
            continue;
        }
        for session_entry in read_dir_sorted(&cwd_entry.path())? {
            let session_dir = session_entry.path();
            if !session_dir.is_dir() {
                continue;
            }
            let Some((archive, compressed)) = find_archive(&session_dir) else {
                continue;
            };
            sessions.push(inspect_stored_session(&session_dir, &archive, compressed));
        }
    }
    sessions.sort_by(|left, right| left.id.cmp(&right.id));

    let cache_dir = dsh_home.join("storages/session_projcache/sessions");
    let mut caches = Vec::new();
    if cache_dir.is_dir() {
        for entry in read_dir_sorted(&cache_dir)? {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_string();
            let (preset, error) = match read_cache_preset(&path) {
                Ok(preset) => (preset, None),
                Err(message) => (None, Some(message)),
            };
            caches.push(CacheRecord {
                id,
                preset,
                path,
                has_archive: false,
                error,
            });
        }
    }
    for cache in &mut caches {
        cache.has_archive = sessions.iter().any(|session| session.id == cache.id);
    }
    caches.sort_by(|left, right| left.id.cmp(&right.id));

    Ok(Inventory { sessions, caches })
}

fn read_dir_sorted(path: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

/// 只接受当前 v4 存档：`session.v4.jsonl`、`session.v4.jsonl.zstd`、`session.v4.jsonl.zst`。
fn find_archive(session_dir: &Path) -> Option<(PathBuf, bool)> {
    for entry in read_dir_sorted(session_dir).ok()? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix("session.v") else {
            continue;
        };
        let Some((version, rest)) = rest.split_once(".jsonl") else {
            continue;
        };
        let Ok(version) = version.parse::<u64>() else {
            continue;
        };
        if version != SESSION_FORMAT_VERSION {
            continue;
        }
        match rest {
            ".zstd" | ".zst" => return Some((entry.path(), true)),
            "" => return Some((entry.path(), false)),
            _ => continue,
        }
    }
    None
}

fn inspect_stored_session(session_dir: &Path, archive: &Path, compressed: bool) -> StoredSession {
    let in_use = matches!(
        acquire_lock(session_dir),
        Err(LockFailure::Busy) | Err(LockFailure::Failure(_))
    );
    match fs::read(archive)
        .map_err(|error| format!("read {}: {error}", archive.display()))
        .and_then(|bytes| inspect_archive_bytes(&bytes, compressed))
    {
        Ok(parsed) => StoredSession {
            id: parsed.id,
            cwd: parsed.cwd,
            header_preset: parsed.header_preset,
            effective_preset: parsed.effective_preset,
            last_seq: parsed.last_seq,
            first_frame_size: parsed.first_frame_size,
            first_frame_plain: parsed.first_frame_plain,
            ends_with_newline: parsed.ends_with_newline,
            compressed,
            in_use,
            archive: archive.to_path_buf(),
            error: None,
        },
        Err(error) => StoredSession {
            id: session_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
            cwd: None,
            header_preset: None,
            effective_preset: None,
            last_seq: None,
            first_frame_size: 0,
            first_frame_plain: Vec::new(),
            ends_with_newline: false,
            compressed,
            in_use,
            archive: archive.to_path_buf(),
            error: Some(error),
        },
    }
}

struct ParsedArchive {
    id: String,
    cwd: Option<String>,
    header_preset: Option<String>,
    effective_preset: Option<String>,
    last_seq: Option<u64>,
    first_frame_size: usize,
    first_frame_plain: Vec<u8>,
    ends_with_newline: bool,
}

/// 只解析事件封套里真正用到的两个字段；其余字段（尤其是 `data`）跳过不建树，
/// 避免为了取 `seq` 给每条事件分配一遍 `serde_json::Value`。
#[derive(serde::Deserialize)]
struct EventRow<'a> {
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default, rename = "type")]
    kind: Option<&'a str>,
}

fn inspect_archive_bytes(bytes: &[u8], compressed: bool) -> Result<ParsedArchive, String> {
    if bytes.is_empty() {
        return Err("empty session archive".to_string());
    }
    // 存档是“帧拼接”格式。首帧单独解压（迁移要保留首帧边界），其余帧一次性流过
    // multi-frame streaming decoder：逐帧 `decode_all` 会为每个帧新建一次解码器，
    // 上千条事件的存档会把一次扫描拖到分钟级。
    let (first_frame_size, first_frame_plain, rest_plain) = if compressed {
        let size = zstd::zstd_safe::find_frame_compressed_size(bytes)
            .map_err(|code| format!("invalid zstd frame at byte 0: {code:?}"))?;
        if size == 0 || size > bytes.len() {
            return Err("first zstd frame size is out of range".to_string());
        }
        let first = zstd::stream::decode_all(&bytes[..size])
            .map_err(|error| format!("decode first zstd frame: {error}"))?;
        let rest = if size < bytes.len() {
            zstd::stream::decode_all(&bytes[size..])
                .map_err(|error| format!("decode zstd frames after byte {size}: {error}"))?
        } else {
            Vec::new()
        };
        (size, first, rest)
    } else {
        let size = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1);
        (size, bytes[..size].to_vec(), bytes[size..].to_vec())
    };
    let mut lines: Vec<&[u8]> = Vec::new();
    for frame in [&first_frame_plain, &rest_plain] {
        for line in frame.split(|byte| *byte == b'\n') {
            if !line.is_empty() {
                lines.push(line);
            }
        }
    }
    let Some(header_line) = lines.first().copied() else {
        return Err("session archive has no header".to_string());
    };
    let header: Value = serde_json::from_slice(header_line)
        .map_err(|error| format!("parse session header: {error}"))?;
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return Err("session archive first record is not a session header".to_string());
    }
    if header.get("version").and_then(Value::as_u64) != Some(SESSION_FORMAT_VERSION) {
        return Err(format!(
            "session header version {:?} is not v{SESSION_FORMAT_VERSION}",
            header.get("version")
        ));
    }
    let id = header
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "session header has no string id".to_string())?
        .to_string();
    let cwd = header
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_string);
    let header_preset = header
        .get(AGENT_PRESET_KEY)
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut effective_preset = header_preset.clone();
    let mut last_seq = None;
    for &line in lines.iter().skip(1) {
        let row: EventRow = serde_json::from_slice(line)
            .map_err(|error| format!("parse session event: {error}"))?;
        if let Some(seq) = row.seq {
            last_seq = Some(last_seq.map_or(seq, |current: u64| current.max(seq)));
        }
        if row.kind == Some(AGENT_PRESET_EVENT) {
            let event: Value = serde_json::from_slice(line)
                .map_err(|error| format!("parse {AGENT_PRESET_EVENT}: {error}"))?;
            let preset = event
                .pointer("/data/agentPreset")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("{AGENT_PRESET_EVENT} event has no data.agentPreset"))?;
            effective_preset = Some(preset.to_string());
        }
    }

    let ends_with_newline = rest_plain
        .last()
        .or_else(|| first_frame_plain.last())
        .is_some_and(|last| *last == b'\n');
    Ok(ParsedArchive {
        id,
        cwd,
        header_preset,
        effective_preset,
        last_seq,
        first_frame_size,
        first_frame_plain,
        ends_with_newline,
    })
}

fn read_cache_preset(path: &Path) -> Result<Option<String>, String> {
    let bytes = fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    Ok(value
        .pointer("/record/rows/agentPreset/val")
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// 只改 `record.rows.agentPreset.val`，保留 `ver` / `seq` 与其余投影行。
fn migrate_cache_bytes(bytes: &[u8], old: &str, new: &str) -> Result<Vec<u8>, String> {
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("parse projection cache: {error}"))?;
    let current = value
        .pointer("/record/rows/agentPreset/val")
        .and_then(Value::as_str)
        .ok_or_else(|| "projection cache has no record.rows.agentPreset.val".to_string())?
        .to_string();
    if current != old {
        return Err(format!(
            "projection cache agentPreset is {current:?}, expected {old:?}"
        ));
    }
    let row = value
        .pointer_mut("/record/rows/agentPreset")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "projection cache agentPreset row is not an object".to_string())?;
    row.insert("val".to_string(), Value::String(new.to_string()));
    let mut out = serde_json::to_vec_pretty(&value)
        .map_err(|error| format!("encode projection cache: {error}"))?;
    if bytes.ends_with(b"\n") {
        out.push(b'\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// rewriting
// ---------------------------------------------------------------------------

fn migrate_bytes(
    bytes: &[u8],
    session: &StoredSession,
    header_to: Option<&str>,
    effective_to: &str,
    seq: u64,
    now_ms: u64,
) -> Result<Vec<u8>, String> {
    if !session.ends_with_newline {
        return Err("archive does not end with a newline; refusing to append".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() + 128);
    match header_to {
        Some(new_header) => {
            let plain = &session.first_frame_plain;
            let line_end = plain
                .iter()
                .position(|byte| *byte == b'\n')
                .ok_or_else(|| "first frame has no newline-terminated header".to_string())?;
            let mut header: Value = serde_json::from_slice(&plain[..line_end])
                .map_err(|error| format!("parse header for rewrite: {error}"))?;
            header
                .as_object_mut()
                .ok_or_else(|| "session header is not an object".to_string())?
                .insert(
                    AGENT_PRESET_KEY.to_string(),
                    Value::String(new_header.to_string()),
                );
            let mut new_line =
                serde_json::to_vec(&header).map_err(|error| format!("encode header: {error}"))?;
            new_line.push(b'\n');
            new_line.extend_from_slice(&plain[line_end + 1..]);
            if session.compressed {
                out.extend_from_slice(&encode_frame(&new_line)?);
            } else {
                out.extend_from_slice(&new_line);
            }
        }
        None => out.extend_from_slice(&bytes[..session.first_frame_size]),
    }
    out.extend_from_slice(&bytes[session.first_frame_size..]);

    let event = json!({
        "type": AGENT_PRESET_EVENT,
        "seq": seq,
        "time": now_ms,
        "data": { AGENT_PRESET_KEY: effective_to },
    });
    let mut line = serde_json::to_vec(&event).map_err(|error| format!("encode event: {error}"))?;
    line.push(b'\n');
    if session.compressed {
        out.extend_from_slice(&encode_frame(&line)?);
    } else {
        out.extend_from_slice(&line);
    }
    Ok(out)
}

fn encode_frame(payload: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder =
        zstd::Encoder::new(Vec::new(), 3).map_err(|error| format!("zstd encoder: {error}"))?;
    encoder
        .include_checksum(true)
        .map_err(|error| format!("zstd checksum flag: {error}"))?;
    encoder
        .write_all(payload)
        .map_err(|error| format!("zstd write: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("zstd finish: {error}"))
}

fn backup_file(path: &Path, stamp: u64) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid file name {}", path.display()))?;
    let backup = path.with_file_name(format!("{name}.bak.{stamp}"));
    fs::copy(path, &backup)
        .map_err(|error| format!("copy {} to {}: {error}", path.display(), backup.display()))?;
    Ok(backup)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("stat {}: {error}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid file name {}", path.display()))?;
    let temp = parent.join(format!(".{name}.acb-migrate-{}", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut file =
            File::create(&temp).map_err(|error| format!("create {}: {error}", temp.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {}: {error}", temp.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", temp.display()))?;
        fs::set_permissions(&temp, metadata.permissions())
            .map_err(|error| format!("chmod {}: {error}", temp.display()))?;
        fs::rename(&temp, path)
            .map_err(|error| format!("rename {} to {}: {error}", temp.display(), path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// session lock
// ---------------------------------------------------------------------------

#[allow(dead_code)]
enum LockFailure {
    Busy,
    Failure(String),
}

#[cfg(unix)]
struct LockGuard(File);

#[cfg(unix)]
impl Drop for LockGuard {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(not(unix))]
struct LockGuard;

#[cfg(unix)]
fn acquire_lock(session_dir: &Path) -> Result<Option<LockGuard>, LockFailure> {
    use std::os::unix::io::AsRawFd;

    let path = session_dir.join("session.lock");
    let file = match OpenOptions::new().read(true).open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(LockFailure::Failure(format!(
                "open {}: {error}",
                path.display()
            )));
        }
    };
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(Some(LockGuard(file)));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Err(LockFailure::Busy)
    } else {
        Err(LockFailure::Failure(format!(
            "flock {}: {error}",
            path.display()
        )))
    }
}

#[cfg(not(unix))]
fn acquire_lock(_session_dir: &Path) -> Result<Option<LockGuard>, LockFailure> {
    Ok(Some(LockGuard))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_archive(header: &str, events: &[&str]) -> Vec<u8> {
        let mut bytes = encode_frame(format!("{header}\n").as_bytes()).unwrap();
        if !events.is_empty() {
            let plain = events
                .iter()
                .map(|event| format!("{event}\n"))
                .collect::<String>();
            bytes.extend(encode_frame(plain.as_bytes()).unwrap());
        }
        bytes
    }

    #[test]
    fn preset_name_is_prefixed_session_id() {
        assert_eq!(preset_name("main"), "acb-main");
        assert_eq!(preset_name("dsh-qas"), "acb-dsh-qas");
    }

    #[test]
    fn set_port_inserts_and_replaces_driver_port() {
        let text = "[[sessions]]\nid = \"main\"\nharness = \"dsh\"\n\n[[sessions.subscriptions]]\nconnection = \"qq\"\nroom = \"group/1\"\n";
        let once = with_driver_port(text, 3101).unwrap();
        assert!(once.contains("driver_port = 3101"), "{once}");
        assert!(
            once.find("driver_port").unwrap() < once.find("[[sessions.subscriptions]]").unwrap(),
            "必须是顶层键，不能落到订阅表里：{once}"
        );
        let twice = with_driver_port(&once, 3102).unwrap();
        assert_eq!(twice.matches("driver_port").count(), 1, "{twice}");
        assert!(twice.contains("driver_port = 3102"), "{twice}");
    }

    #[test]
    fn parses_mapping_and_rejects_invalid_values() {
        let mut options = MigrateOptions {
            dsh_home: None,
            maps: BTreeMap::new(),
            apply: false,
            help: false,
        };
        options.insert_mapping("acb-dsh=acb-dsh-qas").unwrap();
        assert_eq!(
            options.maps.get("acb-dsh"),
            Some(&"acb-dsh-qas".to_string())
        );
        assert!(options.insert_mapping("acb-dsh").is_err());
        assert!(options.insert_mapping("=acb-dsh-qas").is_err());
        assert!(options.insert_mapping("acb-dsh=").is_err());
        assert!(options.insert_mapping("acb-dsh=acb-dsh").is_err());
    }

    #[test]
    fn reads_effective_preset_from_header_and_events() {
        let header = r#"{"type":"session","version":4,"id":"s1","createdAt":1,"cwd":"/tmp","isSeeded":false,"agentPreset":"acb-dsh"}"#;
        let bytes = encode_archive(
            header,
            &[
                r#"{"type":"permission/preset","seq":0,"time":1,"data":{"preset":"workspace-write"}}"#,
                r#"{"type":"agent-preset/selected","seq":1,"time":2,"data":{"agentPreset":"acb-dsh-qas"}}"#,
            ],
        );
        let parsed = inspect_archive_bytes(&bytes, true).unwrap();
        assert_eq!(parsed.header_preset.as_deref(), Some("acb-dsh"));
        assert_eq!(parsed.effective_preset.as_deref(), Some("acb-dsh-qas"));
        assert_eq!(parsed.last_seq, Some(1));
    }

    #[test]
    fn migrate_rewrites_header_and_appends_event() {
        let header = r#"{"type":"session","version":4,"id":"s1","createdAt":1,"cwd":"/tmp","isSeeded":false,"agentPreset":"acb-dsh"}"#;
        let bytes = encode_archive(
            header,
            &[
                r#"{"type":"permission/preset","seq":0,"time":1,"data":{"preset":"workspace-write"}}"#,
            ],
        );
        let parsed = inspect_archive_bytes(&bytes, true).unwrap();
        let session = StoredSession {
            id: parsed.id.clone(),
            cwd: parsed.cwd.clone(),
            header_preset: parsed.header_preset.clone(),
            effective_preset: parsed.effective_preset.clone(),
            last_seq: parsed.last_seq,
            first_frame_size: parsed.first_frame_size,
            first_frame_plain: parsed.first_frame_plain.clone(),
            ends_with_newline: parsed.ends_with_newline,
            compressed: true,
            in_use: false,
            archive: PathBuf::from("/tmp/s1"),
            error: None,
        };
        let next =
            migrate_bytes(&bytes, &session, Some("acb-dsh-qas"), "acb-dsh-qas", 1, 99).unwrap();
        let reparsed = inspect_archive_bytes(&next, true).unwrap();
        assert_eq!(reparsed.header_preset.as_deref(), Some("acb-dsh-qas"));
        assert_eq!(reparsed.effective_preset.as_deref(), Some("acb-dsh-qas"));
        assert_eq!(reparsed.last_seq, Some(1));
    }

    #[test]
    fn migrate_without_header_rewrite_keeps_creation_fact() {
        let header = r#"{"type":"session","version":4,"id":"s1","createdAt":1,"cwd":"/tmp","isSeeded":false,"agentPreset":"standard"}"#;
        let bytes = encode_archive(
            header,
            &[
                r#"{"type":"permission/preset","seq":0,"time":1,"data":{"preset":"workspace-write"}}"#,
            ],
        );
        let parsed = inspect_archive_bytes(&bytes, true).unwrap();
        let session = StoredSession {
            id: parsed.id.clone(),
            cwd: parsed.cwd.clone(),
            header_preset: parsed.header_preset.clone(),
            effective_preset: parsed.effective_preset.clone(),
            last_seq: parsed.last_seq,
            first_frame_size: parsed.first_frame_size,
            first_frame_plain: parsed.first_frame_plain.clone(),
            ends_with_newline: parsed.ends_with_newline,
            compressed: true,
            in_use: false,
            archive: PathBuf::from("/tmp/s1"),
            error: None,
        };
        let next = migrate_bytes(&bytes, &session, None, "acb-dsh-qas", 1, 99).unwrap();
        let reparsed = inspect_archive_bytes(&next, true).unwrap();
        assert_eq!(reparsed.header_preset.as_deref(), Some("standard"));
        assert_eq!(reparsed.effective_preset.as_deref(), Some("acb-dsh-qas"));
    }

    #[test]
    fn plain_archive_migration_keeps_other_lines() {
        let bytes = b"{\"type\":\"session\",\"version\":4,\"id\":\"s1\",\"createdAt\":1,\"cwd\":\"/tmp\",\"isSeeded\":false,\"agentPreset\":\"acb-dsh\"}\n{\"type\":\"permission/preset\",\"seq\":0,\"time\":1,\"data\":{\"preset\":\"workspace-write\"}}\n";
        let parsed = inspect_archive_bytes(bytes, false).unwrap();
        let session = StoredSession {
            id: parsed.id.clone(),
            cwd: parsed.cwd.clone(),
            header_preset: parsed.header_preset.clone(),
            effective_preset: parsed.effective_preset.clone(),
            last_seq: parsed.last_seq,
            first_frame_size: parsed.first_frame_size,
            first_frame_plain: parsed.first_frame_plain.clone(),
            ends_with_newline: parsed.ends_with_newline,
            compressed: false,
            in_use: false,
            archive: PathBuf::from("/tmp/s1"),
            error: None,
        };
        let next =
            migrate_bytes(bytes, &session, Some("acb-dsh-qas"), "acb-dsh-qas", 1, 99).unwrap();
        let reparsed = inspect_archive_bytes(&next, false).unwrap();
        assert_eq!(reparsed.header_preset.as_deref(), Some("acb-dsh-qas"));
        assert_eq!(reparsed.effective_preset.as_deref(), Some("acb-dsh-qas"));
    }

    #[test]
    fn cache_migration_only_rewrites_agent_preset_value() {
        let bytes = br#"{"version":7,"record":{"identity":{"formatVersion":4},"rows":{"agentPreset":{"ver":1,"seq":9,"val":"acb-dsh"},"title":{"ver":1,"seq":9,"val":"keep"}}}}"#;
        let next = migrate_cache_bytes(bytes, "acb-dsh", "acb-dsh-qas").unwrap();
        let value: Value = serde_json::from_slice(&next).unwrap();
        assert_eq!(
            value
                .pointer("/record/rows/agentPreset/val")
                .and_then(Value::as_str),
            Some("acb-dsh-qas")
        );
        assert_eq!(
            value
                .pointer("/record/rows/agentPreset/seq")
                .and_then(Value::as_u64),
            Some(9)
        );
        assert_eq!(
            value
                .pointer("/record/rows/title/val")
                .and_then(Value::as_str),
            Some("keep")
        );
        assert!(migrate_cache_bytes(bytes, "other", "acb-dsh-qas").is_err());
    }
}
