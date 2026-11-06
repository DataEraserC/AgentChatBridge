//! `acbctl profile`：按 generation 存放 ACB 产物，`current` 指向当前 generation。
//!
//! 布局：
//! `profile/generations/<id>/{meta.json,root -> 产物目录}`，
//! `profile/current -> generations/<id>/root`。
//! 切换只改 `current`，不启停服务；generation 里的 `root` 指向 store path 或本地产物。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

use crate::fsutil::{now_ms, sha256_hex, write_atomic};

const SCHEMA: u32 = 1;
const DEFAULT_KEEP: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Source {
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConfigIdentity {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub schema: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at_ms: i64,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigIdentity>,
}

impl Meta {
    pub(crate) fn generation_ref(&self) -> acb_supervisor::GenerationRef {
        acb_supervisor::GenerationRef {
            id: self.id.clone(),
            path: self.source.path.clone(),
            config_sha256: self.config.as_ref().map(|config| config.sha256.clone()),
        }
    }
}

pub(crate) enum Status {
    Missing {
        root: PathBuf,
    },
    Active {
        root: PathBuf,
        id: String,
        artifact: PathBuf,
        bin: PathBuf,
    },
    Broken {
        root: PathBuf,
        detail: String,
    },
}

pub(crate) fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("ACB_PROFILE_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| PathBuf::from(".local/share"));
    base.join("acb").join("profile")
}

pub(crate) fn status() -> Status {
    status_at(&root())
}

pub(crate) fn status_at(root: &Path) -> Status {
    let current = root.join("current");
    match fs::symlink_metadata(&current) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Status::Missing {
            root: root.to_path_buf(),
        },
        Err(error) => Status::Broken {
            root: root.to_path_buf(),
            detail: format!("read {}: {error}", current.display()),
        },
        Ok(_) => match fs::read_link(&current) {
            Err(error) => Status::Broken {
                root: root.to_path_buf(),
                detail: format!("read link {}: {error}", current.display()),
            },
            Ok(target) => {
                let Some(id) = target
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
                else {
                    return Status::Broken {
                        root: root.to_path_buf(),
                        detail: format!("current has unexpected target: {}", target.display()),
                    };
                };
                let resolved = if target.is_absolute() {
                    target.clone()
                } else {
                    root.join(&target)
                };
                let bin = root.join("current").join("bin");
                if !resolved.is_dir() {
                    return Status::Broken {
                        root: root.to_path_buf(),
                        detail: format!("current target missing: {}", resolved.display()),
                    };
                }
                if !resolved.join("bin").is_dir() {
                    return Status::Broken {
                        root: root.to_path_buf(),
                        detail: format!("current target has no bin/: {}", resolved.display()),
                    };
                }
                Status::Active {
                    root: root.to_path_buf(),
                    id: id.to_string(),
                    artifact: resolved,
                    bin,
                }
            }
        },
    }
}

/// render/run 的二进制解析：profile 活跃时用 profile/current/bin，否则用传入回退值。
pub(crate) fn resolve_bin_dir(fallback: Option<&Path>) -> Result<PathBuf, String> {
    match status() {
        Status::Active { bin, .. } => Ok(bin),
        Status::Missing { .. } => fallback.map(Path::to_path_buf).ok_or_else(|| {
            "missing ACB_UNIT_BIN_DIR and no active ACB profile (profile/current/bin not found)"
                .to_string()
        }),
        Status::Broken { detail, .. } => Err(format!("ACB profile current is broken: {detail}")),
    }
}

pub(crate) fn active_meta() -> Result<Option<Meta>, String> {
    match status() {
        Status::Missing { .. } => Ok(None),
        Status::Broken { detail, .. } => Err(format!("ACB profile current is broken: {detail}")),
        Status::Active { root, id, .. } => load_meta(&root, &id)?
            .map(Some)
            .ok_or_else(|| format!("profile current {id} has no readable meta.json")),
    }
}

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    let Some(subcommand) = args.first() else {
        print_help();
        return Err("missing profile subcommand".to_string());
    };
    let rest = &args[1..];
    match subcommand.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "list" => list_command(rest),
        "current" => current_command(rest),
        "add" => add_command(rest),
        "switch" => switch_command(rest),
        "gc" => gc_command(rest),
        "seed" => seed_command(rest),
        other => Err(format!("unknown profile subcommand: {other}")),
    }
}

fn list_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir"])?;
    if !positional.is_empty() {
        return Err("profile list takes no positional arguments".to_string());
    }
    let root = profile_root(&options);
    let generations = load_generations(&root)?;
    let current = current_id(&root)?;

    if generations.is_empty() {
        println!("profile: empty ({})", root.display());
        return Ok(ExitCode::SUCCESS);
    }

    for (id, meta) in generations {
        let marker = if current.as_deref() == Some(id.as_str()) {
            "*"
        } else {
            " "
        };
        let label = meta.label.as_deref().unwrap_or("-");
        let source = match &meta.source.version {
            Some(version) => format!("{}:{} ({version})", meta.source.kind, meta.source.path),
            None => format!("{}:{}", meta.source.kind, meta.source.path),
        };
        let config = match &meta.config {
            Some(config) => format!("{} sha256={}", config.path, short_hash(&config.sha256)),
            None => "-".to_string(),
        };
        println!(
            "{marker} {id} label={label} created_at_ms={} source={source} config={config}",
            meta.created_at_ms
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn current_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir"])?;
    if !positional.is_empty() {
        return Err("profile current takes no positional arguments".to_string());
    }
    let root = profile_root(&options);
    match status_at(&root) {
        Status::Missing { root } => {
            println!("profile: not initialized ({})", root.display());
            Ok(ExitCode::from(1))
        }
        Status::Broken { root, detail } => {
            println!("profile: current broken at {}: {detail}", root.display());
            Ok(ExitCode::from(1))
        }
        Status::Active {
            root,
            id,
            artifact,
            bin,
        } => {
            let meta = load_meta(&root, &id)?
                .ok_or_else(|| format!("profile current {id} has no meta.json"))?;
            println!("id: {}", meta.id);
            println!("label: {}", meta.label.as_deref().unwrap_or("-"));
            println!("created_at_ms: {}", meta.created_at_ms);
            println!("source: {} {}", meta.source.kind, meta.source.path);
            if let Some(version) = &meta.source.version {
                println!("version: {version}");
            }
            println!("artifact: {}", artifact.display());
            println!("bin: {}", bin.display());
            match &meta.config {
                Some(config) => {
                    println!("config: {}", config.path);
                    println!("config_sha256: {}", config.sha256);
                }
                None => {
                    println!("config: -");
                    println!("config_sha256: -");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn add_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) =
        parse_options(args, &["profile-dir", "label", "config", "version"])?;
    if positional.len() != 1 {
        return Err(
            "usage: acbctl profile add <path> [--label LABEL] [--config PATH] [--version VERSION]"
                .to_string(),
        );
    }
    let root = profile_root(&options);
    let label = options.get("label").cloned();
    let version = options.get("version").cloned();
    let config = config_identity(options.get("config").map(PathBuf::from).as_deref())?;
    let meta = add_generation(&root, Path::new(&positional[0]), label, config, version)?;
    println!("added {}", meta.id);
    Ok(ExitCode::SUCCESS)
}

fn switch_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir"])?;
    if positional.len() != 1 {
        return Err("usage: acbctl profile switch <gen-or-label>".to_string());
    }
    let root = profile_root(&options);
    let id = resolve_target(&root, &positional[0])?;
    let target = generation_dir(&root, &id).join("root");
    if !target.is_dir() {
        return Err(format!(
            "generation {id} payload missing: {}",
            target.display()
        ));
    }
    atomic_switch(&root, &id)?;
    println!("switched current -> {id}");
    Ok(ExitCode::SUCCESS)
}

fn gc_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir", "keep"])?;
    if !positional.is_empty() {
        return Err("profile gc takes no positional arguments".to_string());
    }
    let keep = match options.get("keep") {
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| format!("--keep must be a non-negative integer: {value}"))?,
        None => DEFAULT_KEEP,
    };
    let root = profile_root(&options);
    let generations = load_generations(&root)?;
    if generations.is_empty() {
        println!("profile: empty ({})", root.display());
        return Ok(ExitCode::SUCCESS);
    }

    let current = current_id(&root)?;
    let lkg = lkg_generation_id(&root)?;
    let mut protected: BTreeSet<String> = generations
        .iter()
        .take(keep)
        .map(|(id, _)| id.clone())
        .collect();
    if let Some(current) = &current {
        protected.insert(current.clone());
    }
    if let Some(lkg) = &lkg {
        protected.insert(lkg.clone());
    }

    let mut removed = 0;
    for (id, _) in &generations {
        if protected.contains(id) {
            let reason = if current.as_deref() == Some(id.as_str()) {
                "current"
            } else if lkg.as_deref() == Some(id.as_str()) {
                "lkg"
            } else {
                "keep"
            };
            println!("kept {id} ({reason})");
            continue;
        }
        let dir = generation_dir(&root, id);
        fs::remove_dir_all(&dir).map_err(|error| format!("remove {}: {error}", dir.display()))?;
        println!("removed {id}");
        removed += 1;
    }
    println!(
        "gc: removed {removed}, kept {}",
        generations.len().saturating_sub(removed)
    );
    Ok(ExitCode::SUCCESS)
}

fn seed_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir"])?;
    if positional.len() != 1 {
        return Err("usage: acbctl profile seed <store-path>".to_string());
    }
    let root = profile_root(&options);
    let existing = load_generations(&root)?;
    if !existing.is_empty() {
        println!(
            "seed skipped: profile already has {} generation(s)",
            existing.len()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let config = config_identity(None)?;
    let meta = add_generation(&root, Path::new(&positional[0]), None, config, None)?;
    atomic_switch(&root, &meta.id)?;
    println!("seeded {} and switched current", meta.id);
    Ok(ExitCode::SUCCESS)
}

fn add_generation(
    root: &Path,
    source: &Path,
    label: Option<String>,
    config: Option<ConfigIdentity>,
    version: Option<String>,
) -> Result<Meta, String> {
    if let Some(label) = &label {
        if label.trim().is_empty() {
            return Err("--label must not be empty".to_string());
        }
    }

    let source = fs::canonicalize(source)
        .map_err(|error| format!("canonicalize {}: {error}", source.display()))?;
    if !source.is_dir() {
        return Err(format!(
            "generation payload is not a directory: {}",
            source.display()
        ));
    }
    validate_payload(&source)?;

    let generations = load_generations(root)?;
    if let Some(label) = &label {
        if generations
            .iter()
            .any(|(_, meta)| meta.label.as_deref() == Some(label.as_str()))
        {
            return Err(format!("label already exists: {label}"));
        }
    }

    let id = allocate_id(root, &source, config.as_ref(), version.as_deref());
    let generations_dir = root.join("generations");
    fs::create_dir_all(&generations_dir)
        .map_err(|error| format!("create {}: {error}", generations_dir.display()))?;
    let gen_dir = generations_dir.join(&id);
    fs::create_dir(&gen_dir).map_err(|error| format!("create {}: {error}", gen_dir.display()))?;

    let kind = if source.to_string_lossy().starts_with("/nix/store/") {
        "store"
    } else {
        "path"
    };
    let meta = Meta {
        schema: SCHEMA,
        id: id.clone(),
        label,
        created_at_ms: now_ms(),
        source: Source {
            kind: kind.to_string(),
            path: source.to_string_lossy().into_owned(),
            version,
        },
        config,
    };

    let result = (|| -> Result<(), String> {
        symlink(&source, &gen_dir.join("root"))?;
        write_meta(&gen_dir.join("meta.json"), &meta)
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&gen_dir);
        return Err(error);
    }
    Ok(meta)
}

fn validate_payload(source: &Path) -> Result<(), String> {
    let dirs = ["bin", "harnesses", "deploy/units/systemd"];
    let files = [
        "deploy/start-dsh.sh",
        "deploy/start-drivers.sh",
        "deploy/opencode-serve.sh",
        "deploy/run/supervisor.toml.tmpl",
    ];
    let mut missing = Vec::new();
    for relative in dirs {
        if !source.join(relative).is_dir() {
            missing.push(relative.to_string());
        }
    }
    for relative in files {
        if !source.join(relative).is_file() {
            missing.push(relative.to_string());
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "generation payload {} missing required paths: {}",
            source.display(),
            missing.join(", ")
        ))
    }
}

fn config_identity(explicit: Option<&Path>) -> Result<Option<ConfigIdentity>, String> {
    let path = explicit.map(Path::to_path_buf).or_else(|| {
        std::env::var_os("ACB_UNIT_CONFIG_FILE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let Some(path) = path else {
        return Ok(None);
    };
    let bytes =
        fs::read(&path).map_err(|error| format!("read config {}: {error}", path.display()))?;
    let canonical = fs::canonicalize(&path).unwrap_or(path);
    Ok(Some(ConfigIdentity {
        path: canonical.to_string_lossy().into_owned(),
        sha256: sha256_hex(&bytes),
    }))
}

fn load_generations(root: &Path) -> Result<Vec<(String, Meta)>, String> {
    let generations_dir = root.join("generations");
    let entries = match fs::read_dir(&generations_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!("read {}: {error}", generations_dir.display()));
        }
    };
    let mut generations = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("read {}: {error}", generations_dir.display()))?;
        if !entry
            .file_type()
            .map_err(|error| format!("stat {}: {error}", entry.path().display()))?
            .is_dir()
        {
            continue;
        }
        let Some(id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if let Some(meta) = load_meta(root, &id)? {
            generations.push((id, meta));
        }
    }
    generations.sort_by(|(left_id, left), (right_id, right)| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| right_id.cmp(left_id))
    });
    Ok(generations)
}

fn load_meta(root: &Path, id: &str) -> Result<Option<Meta>, String> {
    let path = generation_dir(root, id).join("meta.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let meta: Meta = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if meta.schema != SCHEMA {
        return Err(format!(
            "{}: unsupported profile schema {}",
            path.display(),
            meta.schema
        ));
    }
    Ok(Some(meta))
}

fn write_meta(path: &Path, meta: &Meta) -> Result<(), String> {
    let text = serde_json::to_string_pretty(meta)
        .map_err(|error| format!("serialize meta.json: {error}"))?
        + "\n";
    write_atomic(path, text.as_bytes())
}

fn resolve_target(root: &Path, target: &str) -> Result<String, String> {
    if generation_dir(root, target).join("meta.json").is_file() {
        return Ok(target.to_string());
    }
    let generations = load_generations(root)?;
    let matches: Vec<String> = generations
        .into_iter()
        .filter(|(_, meta)| meta.label.as_deref() == Some(target))
        .map(|(id, _)| id)
        .collect();
    match matches.as_slice() {
        [] => Err(format!("generation or label not found: {target}")),
        [id] => Ok(id.clone()),
        _ => Err(format!(
            "label {target} is ambiguous: {}",
            matches.join(", ")
        )),
    }
}

fn current_id(root: &Path) -> Result<Option<String>, String> {
    let current = root.join("current");
    let target = match fs::read_link(&current) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read link {}: {error}", current.display())),
    };
    let id = target
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("current has unexpected target: {}", target.display()))?;
    Ok(Some(id))
}

fn lkg_generation_id(root: &Path) -> Result<Option<String>, String> {
    let state_dir = root
        .parent()
        .map(|parent| parent.join("state").join("supervisor"))
        .unwrap_or_else(|| PathBuf::from("state/supervisor"));
    let path = state_dir.join("lkg.json");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    Ok(value
        .pointer("/command/generation/id")
        .and_then(|id| id.as_str())
        .map(str::to_string))
}

#[cfg(unix)]
fn atomic_switch(root: &Path, id: &str) -> Result<(), String> {
    let current = root.join("current");
    let old_target = fs::read_link(&current).ok();
    let target = PathBuf::from("generations").join(id).join("root");
    let temp = root.join(format!(".current.tmp-{}-{}", std::process::id(), nonce()));
    let _ = fs::remove_file(&temp);
    std::os::unix::fs::symlink(&target, &temp)
        .map_err(|error| format!("create temporary current symlink: {error}"))?;

    if let Err(error) = fs::rename(&temp, &current) {
        let _ = fs::remove_file(&temp);
        // rename 失败时旧 current 通常会保留；极端情况下补回原指向。
        if fs::symlink_metadata(&current).is_err() {
            if let Some(old_target) = old_target {
                let restore = root.join(format!(
                    ".current.restore-{}-{}",
                    std::process::id(),
                    nonce()
                ));
                if std::os::unix::fs::symlink(&old_target, &restore).is_ok() {
                    let _ = fs::rename(&restore, &current);
                }
            }
        }
        return Err(format!("switch current -> {id}: {error}"));
    }
    Ok(())
}

#[cfg(not(unix))]
fn atomic_switch(_root: &Path, _id: &str) -> Result<(), String> {
    Err("profile switch requires Unix symlinks".to_string())
}

#[cfg(unix)]
fn symlink(source: &Path, target: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(source, target).map_err(|error| {
        format!(
            "create symlink {} -> {}: {error}",
            target.display(),
            source.display()
        )
    })
}

#[cfg(not(unix))]
fn symlink(_source: &Path, _target: &Path) -> Result<(), String> {
    Err("profile generation requires Unix symlinks".to_string())
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn allocate_id(
    root: &Path,
    source: &Path,
    config: Option<&ConfigIdentity>,
    version: Option<&str>,
) -> String {
    let now = now_ms();
    let seed = format!(
        "{}|{}|{}|{}",
        now,
        source.display(),
        config.map(|config| config.sha256.as_str()).unwrap_or("-"),
        version.unwrap_or("-")
    );
    let hash = sha256_hex(seed.as_bytes());
    let base = format!("gen-{now}-{}", &hash[..8]);
    let mut id = base.clone();
    let mut suffix = 1;
    while generation_dir(root, &id).exists() {
        id = format!("{base}-{suffix}");
        suffix += 1;
    }
    id
}

fn generation_dir(root: &Path, id: &str) -> PathBuf {
    root.join("generations").join(id)
}

fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

fn profile_root(options: &BTreeMap<String, String>) -> PathBuf {
    options
        .get("profile-dir")
        .map(PathBuf::from)
        .unwrap_or_else(root)
}

fn parse_options(
    args: &[String],
    allowed: &[&str],
) -> Result<(Vec<String>, BTreeMap<String, String>), String> {
    let mut positional = Vec::new();
    let mut options = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(name) = arg.strip_prefix("--") {
            let (name, value) = match name.split_once('=') {
                Some((name, value)) => (name.to_string(), value.to_string()),
                None => {
                    index += 1;
                    let value = args
                        .get(index)
                        .filter(|value| !value.is_empty())
                        .cloned()
                        .ok_or_else(|| format!("--{name} requires a value"))?;
                    (name.to_string(), value)
                }
            };
            if !allowed.contains(&name.as_str()) {
                return Err(format!("unknown option for profile: --{name}"));
            }
            if value.is_empty() {
                return Err(format!("--{name} requires a value"));
            }
            options.insert(name, value);
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    Ok((positional, options))
}

fn print_help() {
    println!(
        "Usage:
  acbctl profile list    [--profile-dir DIR]
  acbctl profile current [--profile-dir DIR]
  acbctl profile add <path> [--label LABEL] [--config PATH] [--version VERSION] [--profile-dir DIR]
  acbctl profile switch <gen-or-label> [--profile-dir DIR]
  acbctl profile gc      [--keep N] [--profile-dir DIR]
  acbctl profile seed <path> [--profile-dir DIR]"
    );
}
