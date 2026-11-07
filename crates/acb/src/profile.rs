//! `acbctl profile`：按 generation 存放 ACB 产物，`current` 指向当前 generation。
//!
//! 布局：
//! `profile/generations/<id>/{meta.json,manifest.json,root/}`，
//! `profile/current -> generations/<id>/root`。
//! 新 generation 的 `root/` 是 payload 的本地快照（复制，不软链到源目录），
//! 回退不再依赖 nix store 不可变或源目录是否被改动。旧 generation 的 `root`
//! 仍可能是指向 store/source 的软链，读取路径保持兼容。
//! 切换只改 `current`，不启停服务。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::contract;
use crate::fsutil::{now_ms, sha256_hex, write_atomic};
use crate::pins::{self, PinOutcome};

const SCHEMA: u32 = 1;
const DEFAULT_KEEP: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub kind: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigIdentity {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema: u32,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at_ms: i64,
    pub source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ConfigIdentity>,
    /// 新格式 generation 的 payload 策略；缺失表示旧版软链 generation。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
}

/// payload 如何落到 generation 目录：store 靠 store 不可变，copy 是本地快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payload {
    pub strategy: Strategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strategy {
    Store,
    Copy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotInfo {
    pub files: usize,
    pub bytes: u64,
}

const MANIFEST_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    files: Vec<ManifestEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ManifestEntry {
    path: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}

impl Meta {
    /// 该 generation 实际使用的 payload 路径。新格式统一是 `root`
    /// （store 策略下它仍是指向 store 的软链），旧格式保持原来的源路径。
    /// launcher/supervisor 只消费这个路径，不需要知道策略。
    pub fn payload_path(&self, root: &Path) -> PathBuf {
        match &self.payload {
            Some(_) => generation_dir(root, &self.id).join("root"),
            None => PathBuf::from(&self.source.path),
        }
    }

    pub fn generation_ref(&self, root: &Path) -> acb_supervisor::GenerationRef {
        acb_supervisor::GenerationRef {
            id: self.id.clone(),
            path: self.payload_path(root).to_string_lossy().into_owned(),
            config_sha256: self.config.as_ref().map(|config| config.sha256.clone()),
        }
    }
}

pub enum Status {
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

pub fn root() -> PathBuf {
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

pub fn status() -> Status {
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
pub fn resolve_bin_dir(fallback: Option<&Path>) -> Result<PathBuf, String> {
    match status() {
        Status::Active { bin, .. } => Ok(bin),
        Status::Missing { .. } => fallback.map(Path::to_path_buf).ok_or_else(|| {
            "missing ACB_UNIT_BIN_DIR and no active ACB profile (profile/current/bin not found)"
                .to_string()
        }),
        Status::Broken { detail, .. } => Err(format!("ACB profile current is broken: {detail}")),
    }
}

/// scripts/harnesses 的根：profile 活跃时用 profile/current，否则用传入回退值。
pub fn resolve_repo_dir(fallback: Option<&Path>) -> Result<PathBuf, String> {
    match status() {
        Status::Active { root, .. } => Ok(root.join("current")),
        Status::Missing { .. } => fallback.map(Path::to_path_buf).ok_or_else(|| {
            "missing ACB_UNIT_REPO_DIR and no active ACB profile (profile/current not found)"
                .to_string()
        }),
        Status::Broken { detail, .. } => Err(format!("ACB profile current is broken: {detail}")),
    }
}

pub fn resolve_repo_dir_optional(fallback: Option<&Path>) -> Result<Option<PathBuf>, String> {
    match status() {
        Status::Active { root, .. } => Ok(Some(root.join("current"))),
        Status::Missing { .. } => Ok(fallback.map(Path::to_path_buf)),
        Status::Broken { detail, .. } => Err(format!("ACB profile current is broken: {detail}")),
    }
}

pub fn active_meta() -> Result<Option<Meta>, String> {
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
        "verify" => verify_command(rest),
        "migrate" => migrate_command(rest),
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
            match &meta.payload {
                Some(payload) => match (&payload.strategy, &payload.snapshot) {
                    (Strategy::Store, _) => println!("payload: store symlink"),
                    (Strategy::Copy, Some(snapshot)) => println!(
                        "payload: copy snapshot ({} files, {} bytes)",
                        snapshot.files, snapshot.bytes
                    ),
                    (Strategy::Copy, None) => println!("payload: copy snapshot (manifest missing)"),
                },
                None => println!("payload: - (legacy symlink)"),
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
    let (positional, options) = parse_options(
        args,
        &["profile-dir", "label", "config", "version", "strategy"],
    )?;
    if positional.len() != 1 {
        return Err(
            "usage: acbctl profile add <path> --strategy store|copy [--label LABEL] [--config PATH] [--version VERSION]"
                .to_string(),
        );
    }
    let root = profile_root(&options);
    let label = options.get("label").cloned();
    let version = options.get("version").cloned();
    let strategy = required_strategy(&options, "acbctl profile add <path>")?;
    let config = config_identity(options.get("config").map(PathBuf::from).as_deref())?;
    let meta = add_generation(
        &root,
        Path::new(&positional[0]),
        label,
        config,
        version,
        strategy,
    )?;
    println!("added {}", meta.id);
    Ok(ExitCode::SUCCESS)
}

/// 工作模式必须显式配置：CLI `--strategy` 或 `ACB_PROFILE_STRATEGY`。
/// 不做源路径嗅探，也不在不支持的模式下静默降级。
fn required_strategy(options: &BTreeMap<String, String>, usage: &str) -> Result<Strategy, String> {
    let value = options
        .get("strategy")
        .cloned()
        .or_else(|| {
            std::env::var_os("ACB_PROFILE_STRATEGY")
                .filter(|value| !value.to_string_lossy().trim().is_empty())
                .map(|value| value.to_string_lossy().into_owned())
        })
        .ok_or_else(|| {
            format!(
                "profile strategy is required: `{usage}` must pass --strategy store|copy \
                 (or set ACB_PROFILE_STRATEGY); there is no auto-detection"
            )
        })?;
    parse_strategy(value.trim())
}

fn parse_strategy(value: &str) -> Result<Strategy, String> {
    match value {
        "store" => Ok(Strategy::Store),
        "copy" => Ok(Strategy::Copy),
        other => Err(format!(
            "unsupported profile strategy: {other} (supported: store, copy)"
        )),
    }
}

fn switch_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir", "state-dir"])?;
    if positional.len() != 1 {
        return Err(
            "usage: acbctl profile switch <gen-or-label> [--state-dir DIR] [--profile-dir DIR]"
                .to_string(),
        );
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
    verify_generation_contract(&root, &id)?;
    let state_dir = supervisor_state_dir(&options, &root);
    // 先建 pin 再提交指针：避免 switch 成功后 store 路径被 GC 收走。
    pin_generation(&root, &id);
    // current.json 是执行真值，profile/current 只是投影；两者由同一个写入器提交。
    commit_profile_switch(&root, &id, &state_dir, "acbctl-profile-switch")?;
    println!("switched current -> {id} (current.json committed at {})", state_dir.display());
    Ok(ExitCode::SUCCESS)
}

/// `--state-dir` > `ACB_STATE_DIR` > profile 根目录派生的默认位置。
fn supervisor_state_dir(options: &BTreeMap<String, String>, root: &Path) -> PathBuf {
    options
        .get("state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir_for(root))
}

/// 升级链 stage：把 payload 落成 generation，返回它的引用。
///
/// 只建代与 pin，不切换 `current.json`；切换由 [`activate_generation`] 完成。
pub fn add_payload(
    profile_root: &Path,
    payload: &Path,
    strategy: &str,
) -> Result<acb_supervisor::GenerationRef, String> {
    let strategy = parse_strategy(strategy.trim())?;
    let meta = add_generation(profile_root, payload, None, None, None, strategy)?;
    Ok(meta.generation_ref(profile_root))
}

/// 升级链提交：校验 generation，一次写好 `current.json` 与 `profile/current` 投影，
/// 返回 launcher 重启后应运行的命令。
///
/// 新代没有自己的 config 时沿用当前指针的 `args`，保持升级前后启动参数一致。
pub fn activate_generation(
    profile_root: &Path,
    state_dir: &Path,
    generation_id: &str,
) -> Result<acb_supervisor::CommandSpec, String> {
    let command = resolve_generation_command(profile_root, state_dir, generation_id)?;
    pin_generation(profile_root, generation_id);
    commit_command(profile_root, generation_id, state_dir, "acb-launcher", &command)?;
    Ok(command)
}

/// 升级链预检：解析目标代的启动命令并确认可执行文件存在，但不写指针。
///
/// launcher 在停旧之前调用它，让「目标代不可用」这类失败发生在服务还活着的时候。
pub fn validate_generation(
    profile_root: &Path,
    state_dir: &Path,
    generation_id: &str,
) -> Result<(), String> {
    resolve_generation_command(profile_root, state_dir, generation_id).map(|_| ())
}

/// 解析目标代实际要跑的命令；只读，不建 pin、不提交指针。
fn resolve_generation_command(
    profile_root: &Path,
    state_dir: &Path,
    generation_id: &str,
) -> Result<acb_supervisor::CommandSpec, String> {
    let meta = load_meta(profile_root, generation_id)?
        .ok_or_else(|| format!("generation not found: {generation_id}"))?;
    // 契约不兼容要在停旧之前失败；resolve 是 launcher 的升级预检入口。
    verify_generation_contract(profile_root, generation_id)?;
    let payload = generation_dir(profile_root, generation_id).join("root");
    let command = match acb_supervisor::load_current(state_dir)? {
        Some(current) => {
            let program_name = Path::new(&current.program)
                .file_name()
                .ok_or_else(|| format!("cannot derive program name from {}", current.program))?;
            let program = payload.join("bin").join(program_name);
            if !program.is_file() {
                return Err(format!(
                    "target generation has no binary: {}",
                    program.display()
                ));
            }
            acb_supervisor::CommandSpec {
                program: program.to_string_lossy().into_owned(),
                args: current.args.clone(),
                generation: Some(acb_supervisor::GenerationRef {
                    id: generation_id.to_string(),
                    path: payload.to_string_lossy().into_owned(),
                    config_sha256: current
                        .generation
                        .as_ref()
                        .and_then(|generation| generation.config_sha256.clone()),
                }),
            }
        }
        None => {
            let command = command_for_generation(profile_root, &meta);
            if !Path::new(&command.program).is_file() {
                return Err(format!(
                    "target generation has no binary: {}",
                    command.program
                ));
            }
            command
        }
    };
    Ok(command)
}

/// 把 generation 写成 `current.json` 的 command，并同时更新 `profile/current`。
fn commit_profile_switch(
    root: &Path,
    id: &str,
    state_dir: &Path,
    updated_by: &str,
) -> Result<(), String> {
    let meta = load_meta(root, id)?.ok_or_else(|| format!("generation not found: {id}"))?;
    let command = command_for_generation(root, &meta);
    commit_command(root, id, state_dir, updated_by, &command)
}

fn commit_command(
    root: &Path,
    id: &str,
    state_dir: &Path,
    updated_by: &str,
    command: &acb_supervisor::CommandSpec,
) -> Result<(), String> {
    let projection = acb_supervisor::Projection {
        link: root.join("current"),
        target: PathBuf::from("generations").join(id).join("root"),
    };
    let writer = acb_supervisor::ActivationWriter::acquire(state_dir)?;
    writer
        .commit(command, Some(projection), updated_by, now_ms())
        .map_err(|error| format!("commit current.json: {error}"))
}

fn command_for_generation(root: &Path, meta: &Meta) -> acb_supervisor::CommandSpec {
    let generation_root = generation_dir(root, &meta.id).join("root");
    let mut args = Vec::new();
    if let Some(config) = &meta.config {
        args.push("--config".to_string());
        args.push(config.path.clone());
    }
    acb_supervisor::CommandSpec {
        program: generation_root
            .join("bin/acb-bridge")
            .to_string_lossy()
            .into_owned(),
        args,
        generation: Some(meta.generation_ref(root)),
    }
}

/// 给 generation 的 store payload 建 pin。失败只告警，不让 add/switch 失败；
/// 非 nix 环境降级为 `<profile>/../state/pins`，并说明挡不住 nix GC。
fn pin_generation(root: &Path, id: &str) {
    let generation_root = generation_dir(root, id).join("root");
    let payload = match fs::canonicalize(&generation_root) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("pin: warning: resolve {}: {error}", generation_root.display());
            return;
        }
    };
    match pins::pin_store_path(root, id, &payload) {
        Ok(PinOutcome::Gcroot(path)) => println!("pin: {id} -> {}", path.display()),
        Ok(PinOutcome::Local(path)) => eprintln!(
            "pin: warning: no writable nix gcroot; wrote local pin {} (nix GC can still collect {}; \
             keep this generation with `profile gc --keep` or add a gcroot manually)",
            path.display(),
            payload.display()
        ),
        Ok(PinOutcome::Skipped) => {}
        Err(error) => eprintln!("pin: warning: {error}"),
    }
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
        for pin in pins::unpin(&root, id) {
            println!("unpinned {}", pin.display());
        }
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
    let (positional, options) = parse_options(args, &["profile-dir", "strategy", "state-dir"])?;
    if positional.len() > 1 {
        return Err(
            "usage: acbctl profile seed [<path>] --strategy store|copy [--state-dir DIR] [--profile-dir DIR]"
                .to_string(),
        );
    }
    let root = profile_root(&options);
    let existing = load_generations(&root)?;
    if !existing.is_empty() {
        if let CurrentState::NeedsRepair(detail) = current_state(&root)? {
            let Some(payload) = positional.first() else {
                return Err(repair_error(&detail, "no payload argument provided"));
            };
            let strategy = required_strategy(&options, "acbctl profile seed <path>")?;
            let state_dir = supervisor_state_dir(&options, &root);
            return repair_seed(&root, Path::new(payload), &detail, strategy, &state_dir);
        }
        println!(
            "seed skipped: profile already has {} generation(s)",
            existing.len()
        );
        return Ok(ExitCode::SUCCESS);
    }

    let Some(payload) = positional.first() else {
        return Err(
            "usage: acbctl profile seed <path> --strategy store|copy [--state-dir DIR] [--profile-dir DIR]"
                .to_string(),
        );
    };
    let strategy = required_strategy(&options, "acbctl profile seed <path>")?;
    let config = config_identity(None)?;
    let state_dir = supervisor_state_dir(&options, &root);
    let meta = add_generation(&root, Path::new(payload), None, config, None, strategy)?;
    commit_profile_switch(&root, &meta.id, &state_dir, "acbctl-profile-seed")?;
    println!("seeded {} and committed current.json", meta.id);
    write_initial_lkg(&root, &meta.id, &state_dir)?;
    Ok(ExitCode::SUCCESS)
}

/// 安装器的 LKG 约定：第一代只在首次安装时写入一次；已有 lkg.json 时绝不覆盖，
/// 之后的提升/回退都归 launcher + supervisor。
fn write_initial_lkg(root: &Path, id: &str, state_dir: &Path) -> Result<(), String> {
    let path = state_dir.join("lkg.json");
    if path.exists() {
        println!(
            "lkg: {} already exists; left untouched",
            path.display()
        );
        return Ok(());
    }
    let meta = load_meta(root, id)?.ok_or_else(|| format!("generation not found: {id}"))?;
    let command = command_for_generation(root, &meta);
    let record = serde_json::json!({"command": command, "promoted_at_ms": now_ms()});
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| format!("serialize lkg.json: {error}"))?
        + "\n";
    fs::create_dir_all(state_dir)
        .map_err(|error| format!("create {}: {error}", state_dir.display()))?;
    write_atomic(&path, text.as_bytes())?;
    println!("lkg: wrote initial LKG {} -> {id}", path.display());
    Ok(())
}

/// supervisor 状态目录的唯一默认解析。
///
/// 默认位置是 profile 根目录的同级 `state/supervisor`，与 unit 模板里
/// `ACB_UNIT_STATE_DIR` 的派生规则一致；`ACB_STATE_DIR` 可以显式覆盖。
/// `seed`/`switch`/`run`/`status`/`upgrade` 都走这里，避免各自看不同的
/// `current.json` 或 upgrade mailbox。
pub fn default_state_dir() -> PathBuf {
    state_dir_for(&root())
}

fn state_dir_for(profile_root: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("ACB_STATE_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    profile_root
        .parent()
        .map(|parent| parent.join("state").join("supervisor"))
        .unwrap_or_else(|| PathBuf::from("state/supervisor"))
}

/// `current` 是否可用于启动：软链存在、目标 root 存在、且 `bin/acb-bridge` 存在。
enum CurrentState {
    Healthy,
    NeedsRepair(String),
}

fn current_state(root: &Path) -> Result<CurrentState, String> {
    let current = root.join("current");
    match fs::symlink_metadata(&current) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CurrentState::NeedsRepair(format!(
                "current symlink missing: {}",
                current.display()
            )));
        }
        Err(error) => return Err(format!("read {}: {error}", current.display())),
        Ok(metadata) if !metadata.file_type().is_symlink() => {
            return Ok(CurrentState::NeedsRepair(format!(
                "current is not a symlink: {}",
                current.display()
            )));
        }
        Ok(_) => {}
    }
    let target = fs::read_link(&current)
        .map_err(|error| format!("read link {}: {error}", current.display()))?;
    let resolved = if target.is_absolute() {
        target
    } else {
        root.join(&target)
    };
    if !resolved.is_dir() {
        return Ok(CurrentState::NeedsRepair(format!(
            "current target missing: {}",
            resolved.display()
        )));
    }
    let bridge = resolved.join("bin/acb-bridge");
    if !bridge.is_file() {
        return Ok(CurrentState::NeedsRepair(format!(
            "current target missing bin/acb-bridge: {}",
            bridge.display()
        )));
    }
    Ok(CurrentState::Healthy)
}

fn repair_seed(
    root: &Path,
    payload: &Path,
    detail: &str,
    strategy: Strategy,
    state_dir: &Path,
) -> Result<ExitCode, String> {
    let config = config_identity(None)?;
    let meta = add_generation(root, payload, None, config, None, strategy)
        .map_err(|error| repair_error(detail, &error))?;
    commit_profile_switch(root, &meta.id, state_dir, "acbctl-profile-seed-repair")?;
    println!("seed repaired current -> {} (was: {detail})", meta.id);
    Ok(ExitCode::SUCCESS)
}

fn repair_error(detail: &str, cause: &str) -> String {
    format!(
        "profile current is not usable: {detail}\n\
         seed cannot repair it: {cause}\n\
         fix manually: acbctl profile add <payload> --strategy store|copy [--label LABEL], \
         then acbctl profile switch <id>"
    )
}

fn add_generation(
    root: &Path,
    source: &Path,
    label: Option<String>,
    config: Option<ConfigIdentity>,
    version: Option<String>,
    strategy: Strategy,
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
    if strategy == Strategy::Store && !source.to_string_lossy().starts_with("/nix/store/") {
        return Err(format!(
            "store strategy requires the payload under /nix/store: {}\n\
             use --strategy copy for a local snapshot",
            source.display()
        ));
    }

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
    let payload = match strategy {
        Strategy::Store => match symlink(&source, &gen_dir.join("root")) {
            Ok(()) => Payload {
                strategy,
                snapshot: None,
            },
            Err(error) => {
                let _ = fs::remove_dir_all(&gen_dir);
                return Err(error);
            }
        },
        Strategy::Copy => {
            let snapshot_root = gen_dir.join("root");
            let (entries, bytes) = match copy_payload(&source, &snapshot_root) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    let _ = fs::remove_dir_all(&gen_dir);
                    return Err(error);
                }
            };
            let manifest = Manifest {
                schema: MANIFEST_SCHEMA,
                files: entries,
            };
            if let Err(error) = write_manifest(&gen_dir.join("manifest.json"), &manifest) {
                let _ = fs::remove_dir_all(&gen_dir);
                return Err(error);
            }
            Payload {
                strategy,
                snapshot: Some(SnapshotInfo {
                    files: manifest.files.len(),
                    bytes,
                }),
            }
        }
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
        payload: Some(payload),
    };
    if let Err(error) = write_meta(&gen_dir.join("meta.json"), &meta) {
        let _ = fs::remove_dir_all(&gen_dir);
        return Err(error);
    }
    // store payload 随时可能被 nix GC 回收；add 成功后立刻建 pin（失败只告警）。
    pin_generation(root, &meta.id);
    Ok(meta)
}

/// copy 策略：把 payload 逐文件复制到 `dest`，同时生成 manifest 条目。
fn copy_payload(source: &Path, dest: &Path) -> Result<(Vec<ManifestEntry>, u64), String> {
    fs::create_dir(dest).map_err(|error| format!("create {}: {error}", dest.display()))?;
    let mut entries = Vec::new();
    let mut bytes = 0u64;
    copy_dir(source, dest, "", &mut entries, &mut bytes)?;
    Ok((entries, bytes))
}

fn copy_dir(
    source: &Path,
    dest: &Path,
    prefix: &str,
    entries: &mut Vec<ManifestEntry>,
    bytes: &mut u64,
) -> Result<(), String> {
    for child in sorted_children(source)? {
        let name = child.file_name().to_string_lossy().into_owned();
        let relative = join_relative(prefix, &name);
        let child_path = child.path();
        let target_path = dest.join(&name);
        let file_type = child
            .file_type()
            .map_err(|error| format!("stat {}: {error}", child_path.display()))?;
        if file_type.is_dir() {
            fs::create_dir(&target_path)
                .map_err(|error| format!("create {}: {error}", target_path.display()))?;
            copy_dir(&child_path, &target_path, &relative, entries, bytes)?;
        } else if file_type.is_file() {
            let (size, sha256) = copy_file(&child_path, &target_path)?;
            *bytes += size;
            entries.push(ManifestEntry {
                path: relative,
                kind: "file".to_string(),
                size: Some(size),
                sha256: Some(sha256),
                target: None,
            });
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&child_path)
                .map_err(|error| format!("read link {}: {error}", child_path.display()))?;
            symlink(&link_target, &target_path)?;
            entries.push(ManifestEntry {
                path: relative,
                kind: "symlink".to_string(),
                size: None,
                sha256: None,
                target: Some(link_target.to_string_lossy().into_owned()),
            });
        } else {
            return Err(format!(
                "payload entry is not a file, directory or symlink: {}",
                child_path.display()
            ));
        }
    }
    Ok(())
}

/// 流式复制 + 计算 sha256；保留文件权限（目录权限不保留，保证 gc 可删）。
fn copy_file(source: &Path, dest: &Path) -> Result<(u64, String), String> {
    let mut input = fs::File::open(source)
        .map_err(|error| format!("open {}: {error}", source.display()))?;
    let mut output =
        fs::File::create(dest).map_err(|error| format!("create {}: {error}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", source.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("write {}: {error}", dest.display()))?;
        size += read as u64;
    }
    output
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", dest.display()))?;
    drop(output);
    let permissions = fs::metadata(source)
        .map_err(|error| format!("stat {}: {error}", source.display()))?
        .permissions();
    fs::set_permissions(dest, permissions)
        .map_err(|error| format!("chmod {}: {error}", dest.display()))?;
    let digest = hasher.finalize();
    Ok((size, format!("{digest:x}")))
}

/// 扫描一棵目录树，产出与 manifest 同形状的条目（用于 copy 校验和源漂移检测）。
fn scan_tree(base: &Path) -> Result<Vec<ManifestEntry>, String> {
    if fs::symlink_metadata(base)
        .map_err(|error| format!("stat {}: {error}", base.display()))?
        .file_type()
        .is_symlink()
    {
        return Err(format!(
            "payload root is a symlink, not a copied snapshot: {}",
            base.display()
        ));
    }
    let mut entries = Vec::new();
    scan_dir(base, "", &mut entries)?;
    Ok(entries)
}

fn scan_dir(dir: &Path, prefix: &str, entries: &mut Vec<ManifestEntry>) -> Result<(), String> {
    for child in sorted_children(dir)? {
        let name = child.file_name().to_string_lossy().into_owned();
        let relative = join_relative(prefix, &name);
        let child_path = child.path();
        let file_type = child
            .file_type()
            .map_err(|error| format!("stat {}: {error}", child_path.display()))?;
        if file_type.is_dir() {
            scan_dir(&child_path, &relative, entries)?;
        } else if file_type.is_file() {
            let (size, sha256) = hash_file(&child_path)?;
            entries.push(ManifestEntry {
                path: relative,
                kind: "file".to_string(),
                size: Some(size),
                sha256: Some(sha256),
                target: None,
            });
        } else if file_type.is_symlink() {
            let link_target = fs::read_link(&child_path)
                .map_err(|error| format!("read link {}: {error}", child_path.display()))?;
            entries.push(ManifestEntry {
                path: relative,
                kind: "symlink".to_string(),
                size: None,
                sha256: None,
                target: Some(link_target.to_string_lossy().into_owned()),
            });
        } else {
            return Err(format!(
                "entry is not a file, directory or symlink: {}",
                child_path.display()
            ));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, String), String> {
    let mut input = fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }
    let digest = hasher.finalize();
    Ok((size, format!("{digest:x}")))
}

fn sorted_children(dir: &Path) -> Result<Vec<fs::DirEntry>, String> {
    let read = fs::read_dir(dir).map_err(|error| format!("read {}: {error}", dir.display()))?;
    let mut children = Vec::new();
    for entry in read {
        children.push(entry.map_err(|error| format!("read {}: {error}", dir.display()))?);
    }
    children.sort_by_key(|entry| entry.file_name());
    Ok(children)
}

fn join_relative(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn write_manifest(path: &Path, manifest: &Manifest) -> Result<(), String> {
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| format!("serialize manifest.json: {error}"))?
        + "\n";
    write_atomic(path, text.as_bytes())
}

/// `profile verify [<gen-or-label>]`：按策略校验。
/// store：只确认软链目标（store 本身不可变）；copy：逐文件 manifest + 源漂移；
/// legacy：没有 manifest，明确告知不可校验并返回成功。
fn verify_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir"])?;
    if positional.len() > 1 {
        return Err("usage: acbctl profile verify [<gen-or-label>] [--profile-dir DIR]".to_string());
    }
    let root = profile_root(&options);
    let id = match positional.first() {
        Some(target) => resolve_target(&root, target)?,
        None => current_id(&root)?
            .ok_or_else(|| "profile current is missing; pass a generation id".to_string())?,
    };
    let meta = load_meta(&root, &id)?.ok_or_else(|| format!("generation not found: {id}"))?;
    let generation_root = generation_dir(&root, &id).join("root");

    let Some(payload) = &meta.payload else {
        println!("verify: {id} is a legacy symlink generation; no strategy or manifest to verify");
        println!("verify: run `acbctl profile migrate --to copy` if this generation must survive without nix");
        return Ok(ExitCode::SUCCESS);
    };

    match payload.strategy {
        Strategy::Store => verify_store(&id, &meta, &generation_root),
        Strategy::Copy => verify_copy(&id, &meta, payload, &generation_root),
    }
}

fn verify_store(id: &str, meta: &Meta, generation_root: &Path) -> Result<ExitCode, String> {
    println!(
        "verify: gen={id} strategy=store payload={} (store is immutable)",
        meta.source.path
    );
    let target = match fs::read_link(generation_root) {
        Ok(target) => target,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "verify: snapshot damaged: {id} generation root missing: {}",
                generation_root.display()
            );
            return Ok(ExitCode::from(1));
        }
        Err(error) => {
            return Err(format!("read link {}: {error}", generation_root.display()));
        }
    };
    if !target.is_dir() {
        println!(
            "verify: snapshot damaged: store target missing: {}",
            target.display()
        );
        println!("verify: fix by re-adding the payload with `acbctl profile add --strategy store`");
        return Ok(ExitCode::from(1));
    }
    if target != Path::new(&meta.source.path) {
        println!(
            "verify: note: root points to {} (recorded source {})",
            target.display(),
            meta.source.path
        );
    }
    println!("verify: store payload ok");
    Ok(ExitCode::SUCCESS)
}

fn verify_copy(
    id: &str,
    meta: &Meta,
    payload: &Payload,
    generation_root: &Path,
) -> Result<ExitCode, String> {
    let Some(snapshot) = &payload.snapshot else {
        println!("verify: snapshot damaged: {id} copy strategy has no snapshot metadata");
        println!("verify: fix by re-adding the payload or switching to another generation");
        return Ok(ExitCode::from(1));
    };
    let Some(manifest) = load_manifest_for_verify(id, generation_root)? else {
        return Ok(ExitCode::from(1));
    };

    println!(
        "verify: gen={id} strategy=copy files={} bytes={}",
        manifest.files.len(),
        snapshot.bytes
    );
    let actual = match scan_tree(generation_root) {
        Ok(actual) => actual,
        Err(error) => {
            println!("verify: snapshot damaged: {error}");
            println!("verify: fix by re-adding the payload or switching to another generation");
            return Ok(ExitCode::from(1));
        }
    };
    let diff = diff_entries(&manifest.files, &actual);
    if diff.clean() {
        println!("verify: snapshot ok");
    } else {
        println!("verify: snapshot damaged: {}", diff.summary());
        print_diff("snapshot", &diff);
        println!("verify: fix by re-adding the payload or switching to another generation");
    }

    let source = Path::new(&meta.source.path);
    if source.is_dir() {
        match scan_tree(source) {
            Ok(source_entries) => {
                let drift = diff_entries(&manifest.files, &source_entries);
                if drift.clean() {
                    println!("verify: source unchanged ({})", source.display());
                } else {
                    println!(
                        "verify: source differs ({}): {}",
                        source.display(),
                        drift.summary()
                    );
                    print_diff("source", &drift);
                }
            }
            Err(error) => println!("verify: source unavailable: {error}"),
        }
    } else {
        println!("verify: source unavailable ({})", source.display());
    }

    Ok(ExitCode::from(if diff.clean() { 0 } else { 1 }))
}

fn load_manifest_for_verify(
    id: &str,
    generation_root: &Path,
) -> Result<Option<Manifest>, String> {
    let path = generation_root
        .parent()
        .map(|dir| dir.join("manifest.json"))
        .unwrap_or_else(|| PathBuf::from("manifest.json"));
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            println!("verify: snapshot damaged: {id} has no manifest.json");
            println!("verify: fix by re-adding the payload or switching to another generation");
            return Ok(None);
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(format!(
            "{}: unsupported manifest schema {}",
            path.display(),
            manifest.schema
        ));
    }
    Ok(Some(manifest))
}

struct Diff {
    changed: Vec<String>,
    missing: Vec<String>,
    extra: Vec<String>,
}

impl Diff {
    fn clean(&self) -> bool {
        self.changed.is_empty() && self.missing.is_empty() && self.extra.is_empty()
    }

    fn summary(&self) -> String {
        format!(
            "{} changed, {} missing, {} extra",
            self.changed.len(),
            self.missing.len(),
            self.extra.len()
        )
    }
}

fn diff_entries(expected: &[ManifestEntry], actual: &[ManifestEntry]) -> Diff {
    let expected_map: BTreeMap<&str, &ManifestEntry> = expected
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let actual_map: BTreeMap<&str, &ManifestEntry> = actual
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut changed = Vec::new();
    let mut missing = Vec::new();
    for (path, entry) in &expected_map {
        match actual_map.get(path) {
            Some(other) if *other == *entry => {}
            Some(_) => changed.push(path.to_string()),
            None => missing.push(path.to_string()),
        }
    }
    let extra: Vec<String> = actual_map
        .keys()
        .filter(|path| !expected_map.contains_key(**path))
        .map(|path| path.to_string())
        .collect();
    Diff {
        changed,
        missing,
        extra,
    }
}

fn print_diff(label: &str, diff: &Diff) {
    const LIMIT: usize = 20;
    let total = diff.changed.len() + diff.missing.len() + diff.extra.len();
    let mut shown = 0usize;
    for (kind, paths) in [
        ("changed", &diff.changed),
        ("missing", &diff.missing),
        ("extra", &diff.extra),
    ] {
        for path in paths {
            if shown >= LIMIT {
                println!("  {label}: ... {} more", total - shown);
                return;
            }
            println!("  {label} {kind} {path}");
            shown += 1;
        }
    }
}

/// `profile migrate --to copy [<gen-or-label>]`：把已有 generation（legacy 或 store 策略）
/// 原地转成 copy 快照，保留 id/label/created_at，`current`/LKG 指针一律不动。
fn migrate_command(args: &[String]) -> Result<ExitCode, String> {
    let (positional, options) = parse_options(args, &["profile-dir", "to"])?;
    if positional.len() > 1 {
        return Err(
            "usage: acbctl profile migrate --to copy [<gen-or-label>] [--profile-dir DIR]"
                .to_string(),
        );
    }
    let target = options.get("to").ok_or_else(|| {
        "usage: acbctl profile migrate --to copy [<gen-or-label>] (only copy is supported)"
            .to_string()
    })?;
    if target != "copy" {
        return Err(format!(
            "unsupported migrate target: {target} (only copy is supported)\n\
             to go back to store, re-add the store payload with `acbctl profile add --strategy store`"
        ));
    }
    let root = profile_root(&options);
    let ids: Vec<String> = match positional.first() {
        Some(target) => vec![resolve_target(&root, target)?],
        None => load_generations(&root)?
            .into_iter()
            .map(|(id, _)| id)
            .collect(),
    };
    if ids.is_empty() {
        println!("profile: empty ({})", root.display());
        return Ok(ExitCode::SUCCESS);
    }

    let mut migrated = 0usize;
    for id in &ids {
        if migrate_one(&root, id)? {
            migrated += 1;
        }
    }
    if migrated > 0 {
        println!(
            "migrate: {migrated} generation(s) converted to copy; current and LKG pointers were not touched"
        );
        println!(
            "migrate: runtime LKG promotion/rollback stays with the launcher and supervisor"
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn migrate_one(root: &Path, id: &str) -> Result<bool, String> {
    let mut meta =
        load_meta(root, id)?.ok_or_else(|| format!("generation not found: {id}"))?;
    if meta.payload.as_ref().map(|payload| payload.strategy) == Some(Strategy::Copy) {
        println!("migrate: {id} already copy; skipped");
        return Ok(false);
    }

    let gen_dir = generation_dir(root, id);
    let generation_root = gen_dir.join("root");
    let source = fs::canonicalize(&generation_root)
        .map_err(|error| format!("resolve {}: {error}", generation_root.display()))?;
    let staging = gen_dir.join(format!(".root.tmp-{}", now_ms()));
    let (entries, bytes) = copy_payload(&source, &staging).map_err(|error| {
        let _ = fs::remove_dir_all(&staging);
        error
    })?;
    let manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        files: entries,
    };
    if let Err(error) = write_manifest(&gen_dir.join("manifest.json"), &manifest) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    let backup = gen_dir.join(format!(".root.old-{}", now_ms()));
    fs::rename(&generation_root, &backup)
        .map_err(|error| format!("move {}: {error}", generation_root.display()))?;
    if let Err(error) = fs::rename(&staging, &generation_root) {
        let _ = fs::rename(&backup, &generation_root);
        return Err(format!("install snapshot {}: {error}", generation_root.display()));
    }
    match fs::symlink_metadata(&backup) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let _ = fs::remove_file(&backup);
        }
        Ok(_) => {
            let _ = fs::remove_dir_all(&backup);
        }
        Err(_) => {}
    }

    meta.payload = Some(Payload {
        strategy: Strategy::Copy,
        snapshot: Some(SnapshotInfo {
            files: manifest.files.len(),
            bytes,
        }),
    });
    write_meta(&gen_dir.join("meta.json"), &meta)?;
    // payload 已是本地快照，store pin 不再需要；留着只会延长 store 路径寿命。
    for pin in pins::unpin(root, id) {
        println!("unpinned {}", pin.display());
    }
    println!(
        "migrated {id} -> copy ({} files, {} bytes)",
        manifest.files.len(),
        bytes
    );
    Ok(true)
}

fn validate_payload(source: &Path) -> Result<(), String> {
    // payload 只校验它自己该带的东西：acbl 的 launcher / supervisor / unit 模板
    // 已剥离到 acbl 包，不再出现在 generation 里。
    let dirs = ["bin", "harnesses"];
    let files = [
        "bin/acb-bridge",
        "deploy/start-drivers.sh",
        "harnesses/dsh/src/driver.mjs",
        "harnesses/dsh/dsh-mcp.patch.yml",
        "harnesses/opencode/plugin/index.js",
        "harnesses/opencode/plugin/acb-wire.mjs",
        "harnesses/shared/tools.mjs",
    ];
    let mut missing = Vec::new();
    for relative in dirs {
        if !source.join(relative).is_dir() {
            missing.push(format!("{relative} (directory)"));
        }
    }
    for relative in files {
        if !source.join(relative).is_file() {
            missing.push(format!("{relative} (file)"));
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "generation payload {} missing required entries: {}",
            source.display(),
            missing.join(", ")
        ));
    }
    verify_contract(source)?;
    Ok(())
}

/// payload 契约声明校验；缺文件时默认按契约 v1 放行并打印醒目告警（过渡行为），
/// 设置 `ACB_CONTRACT_STRICT` 后改为按 `contract_unreadable` 拒绝。
fn verify_contract(payload: &Path) -> Result<(), String> {
    match contract::verify_with(payload, contract::configured_missing_policy()) {
        Ok(contract::State::Declared(_)) => Ok(()),
        Ok(contract::State::Legacy) => {
            eprintln!(
                "WARNING: payload {} has no {}; treating it as acb_contract={}.",
                payload.display(),
                contract::CONTRACT_FILE,
                contract::CONTRACT_MIN
            );
            eprintln!(
                "WARNING: TRANSITIONAL fallback only - the next minor acbl release will \
                 HARD-REJECT such payloads. Ship {} or set {}=1 to enforce strict mode now.",
                contract::CONTRACT_FILE,
                contract::STRICT_ENV
            );
            Ok(())
        }
        Err(violation) => Err(format!(
            "generation payload {} rejected: {violation}",
            payload.display()
        )),
    }
}

/// generation 的 payload 契约校验；`add` / `switch` / 升级预检都走这里，
/// 保证「不兼容的目标代」在停旧之前就被拒。
fn verify_generation_contract(root: &Path, id: &str) -> Result<(), String> {
    let payload = generation_dir(root, id).join("root");
    verify_contract(&payload).map_err(|error| format!("generation {id}: {error}"))
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
    let path = state_dir_for(root).join("lkg.json");
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
  acbctl profile add <path> --strategy store|copy [--label LABEL] [--config PATH] [--version VERSION] [--profile-dir DIR]
  acbctl profile switch <gen-or-label> [--profile-dir DIR]
  acbctl profile gc      [--keep N] [--profile-dir DIR]
  acbctl profile seed [<path>] --strategy store|copy [--state-dir DIR] [--profile-dir DIR]
  acbctl profile verify  [<gen-or-label>] [--profile-dir DIR]
  acbctl profile migrate --to copy [<gen-or-label>] [--profile-dir DIR]

strategy: store = 软链到 nix store（靠不可变回退）；copy = 本地复制快照 + manifest。
模式必须显式给出（--strategy 或 ACB_PROFILE_STRATEGY），不做自动判断。"
    );
}
