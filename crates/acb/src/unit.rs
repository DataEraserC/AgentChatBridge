//! `acbctl unit`：模板 + ACB_UNIT_* 值 → systemd unit 文件。
//!
//! 安全边界：
//! - 只读白名单环境变量；出现任何其它 `ACB_UNIT_*` 键立即报错。
//! - 模板里只放路径/端口/profile/env 文件占位符；重启策略、退出码、依赖拓扑写死。
//! - 卸载/孤儿清理只删除 manifest 登记且首行带 `# acb-managed:` 的文件。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::{Deserialize, Serialize};

const MANAGED_HEADER: &str = "# acb-managed:";
const MANIFEST_NAME: &str = "acb-units.manifest.json";
const MANIFEST_VERSION: u32 = 1;

/// `acbctl unit install-user` 落盘的单元名与现役 nix-config 单元同名：
/// 用户目录优先，安装后遮蔽 `/etc/systemd/user` 下的 nix 版本。
const USER_UNIT_NAME: &str = "acb-bridge.service";
const USER_UNIT_TEMPLATE: &str =
    include_str!("../../../share/acb/units/acb-bridge-user.service.tmpl");

/// 模板在编译期嵌入二进制，部署时不依赖仓库目录仍然存在。
const TEMPLATES: &[(&str, &str)] = &[
    (
        "dsh-acb.service",
        include_str!("../../../share/acb/units/dsh-acb.service.tmpl"),
    ),
    (
        "acb-bridge.service",
        include_str!("../../../share/acb/units/acb-bridge.service.tmpl"),
    ),
    (
        "acb-drivers.service",
        include_str!("../../../share/acb/units/acb-drivers.service.tmpl"),
    ),
    (
        "acb-opencode-serve.service",
        include_str!("../../../share/acb/units/acb-opencode-serve.service.tmpl"),
    ),
    (
        "acb-launcher.service",
        include_str!("../../../share/acb/units/acb-launcher.service.tmpl"),
    ),
    (
        "acb-launcher-user.service",
        include_str!("../../../share/acb/units/acb-launcher-user.service.tmpl"),
    ),
    (
        "acb-stack.target",
        include_str!("../../../share/acb/units/acb-stack.target.tmpl"),
    ),
];

const REQUIRED_KEYS: &[&str] = &[
    "ACB_UNIT_LOG_DIR",
    "ACB_UNIT_PROFILE_DIR",
    "ACB_UNIT_CONFIG_FILE",
    "ACB_UNIT_ENV_FILE",
    "ACB_UNIT_OPENCODE_DIR",
];

const PORT_KEYS: &[&str] = &["ACB_UNIT_CONSOLE_PORT", "ACB_UNIT_OPENCODE_PORT"];

const OPTIONAL_KEYS: &[&str] = &[
    "ACB_UNIT_REPO_DIR",
    "ACB_UNIT_BIN_DIR",
    "ACB_UNIT_SHELL",
    "ACB_UNIT_CONSOLE_PORT",
    "ACB_UNIT_OPENCODE_PORT",
    "ACB_UNIT_INSTALL_SECTION",
    "ACB_UNIT_LAUNCHER_BIN",
    "ACB_UNIT_ACBL_PREFIX",
    "ACB_UNIT_STATE_DIR",
    "ACB_UNIT_WORK_DIR",
    "ACB_UNIT_SUPERVISOR_CONFIG_FILE",
    "ACB_UNIT_ENDPOINT_LISTEN",
    "ACB_UNIT_CONSOLE_LISTEN",
    "ACB_UNIT_RELAY_LISTEN",
    "ACB_UNIT_SYSTEM_UNIT_DIR",
];

const KEY_DEFAULTS: &[(&str, &str)] = &[
    ("ACB_UNIT_SHELL", "/run/current-system/sw/bin/bash"),
    ("ACB_UNIT_CONSOLE_PORT", "3100"),
    ("ACB_UNIT_OPENCODE_PORT", "4097"),
    ("ACB_UNIT_INSTALL_SECTION", "0"),
    ("ACB_UNIT_ACBL_PREFIX", "/usr/local"),
    ("ACB_UNIT_ENDPOINT_LISTEN", "127.0.0.1:7700"),
    ("ACB_UNIT_CONSOLE_LISTEN", "0.0.0.0:3100"),
    ("ACB_UNIT_RELAY_LISTEN", "0.0.0.0:3131"),
    ("ACB_UNIT_SYSTEM_UNIT_DIR", "/etc/systemd/user"),
];

struct RenderedUnit {
    name: &'static str,
    content: String,
}

enum RenderMode {
    Print,
    OutputDir(PathBuf),
    Diff(PathBuf),
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    files: BTreeSet<String>,
    #[serde(default)]
    hashes: BTreeMap<String, String>,
}

enum WriteStatus {
    Created,
    Updated,
    Unchanged,
}

impl WriteStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
        }
    }
}

pub fn render_command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_render_help();
        return Ok(ExitCode::SUCCESS);
    }

    let mode = parse_render_args(args)?;
    let units = render_all()?;

    match mode {
        RenderMode::Print => {
            for (index, unit) in units.iter().enumerate() {
                if index > 0 {
                    println!();
                }
                println!("# === {} ===", unit.name);
                print!("{}", unit.content);
            }
            Ok(ExitCode::SUCCESS)
        }
        RenderMode::OutputDir(dir) => {
            fs::create_dir_all(&dir)
                .map_err(|error| format!("create {}: {error}", dir.display()))?;
            for unit in &units {
                let target = dir.join(unit.name);
                let status = write_if_changed(&target, &unit.content)?;
                println!("{} {}", status.as_str(), target.display());
            }
            Ok(ExitCode::SUCCESS)
        }
        RenderMode::Diff(root) => {
            let same = diff_against(&root, &units)?;
            Ok(if same {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
    }
}

pub fn install_command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_install_help();
        return Ok(ExitCode::SUCCESS);
    }

    let parsed = parse_install_args(args)?;
    let units = render_all_with_install_section(
        parsed.install_section.as_deref(),
        parsed.scope.as_deref(),
    )?;
    let root = parsed.root;
    fs::create_dir_all(&root).map_err(|error| format!("create {}: {error}", root.display()))?;

    let previous = read_manifest(&root)?;
    let mut current = BTreeSet::new();
    let mut hashes = BTreeMap::new();

    for unit in &units {
        current.insert(unit.name.to_string());
        hashes.insert(
            unit.name.to_string(),
            crate::fsutil::sha256_hex(unit.content.as_bytes()),
        );
        let target = root.join(unit.name);
        let status = write_if_changed(&target, &unit.content)?;
        println!("{} {}", status.as_str(), unit.name);
    }

    if let Some(previous) = previous {
        for name in previous.files.difference(&current) {
            remove_orphan(&root, name)?;
        }
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        files: current,
        hashes,
    };
    write_manifest_if_changed(&root, &manifest)?;
    print_reload_hint();
    Ok(ExitCode::SUCCESS)
}

pub fn uninstall_command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_uninstall_help();
        return Ok(ExitCode::SUCCESS);
    }
    if args
        .iter()
        .any(|arg| arg == "--purge" || arg.starts_with("--purge="))
    {
        return Err(
            "acbctl unit uninstall has no --purge: profile and LKG state are never removed"
                .to_string(),
        );
    }

    let root = parse_root_arg(args, "uninstall")?;
    let manifest_path = root.join(MANIFEST_NAME);
    let Some(manifest) = read_manifest(&root)? else {
        println!("manifest not found: {}", manifest_path.display());
        return Ok(ExitCode::SUCCESS);
    };

    for name in &manifest.files {
        remove_managed_file(&root, name)?;
    }

    fs::remove_file(&manifest_path)
        .map_err(|error| format!("remove {}: {error}", manifest_path.display()))?;
    println!("removed {MANIFEST_NAME}");
    print_reload_hint();
    Ok(ExitCode::SUCCESS)
}

/// 安装/启用 acbl 自己的 user 单元（`~/.config/systemd/user/acb-bridge.service`）。
///
/// 与 `unit install` 的区别：只写这一个单元、自动 `daemon-reload` 并 `enable`，
/// 且 `ExecStart` 从 acbctl 自身安装前缀推导，不依赖 nix 的 `/run/current-system`。
pub fn install_user_command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_install_user_help();
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(other) = args.first() {
        return Err(format!("unknown argument for install-user: {other}"));
    }

    let values = resolved_user_unit_values()?;
    let content = substitute("acb-bridge-user.service", USER_UNIT_TEMPLATE, &values)?;
    let system_unit_dir = PathBuf::from(
        values
            .get("ACB_UNIT_SYSTEM_UNIT_DIR")
            .expect("defaults cover system unit dir"),
    );
    let root = user_unit_dir()?;
    fs::create_dir_all(&root).map_err(|error| format!("create {}: {error}", root.display()))?;
    let target = root.join(USER_UNIT_NAME);

    if let Ok(existing) = fs::read_to_string(&target) {
        if !is_managed(&existing) {
            return Err(format!(
                "refusing to overwrite {}: no `{MANAGED_HEADER}` header; \
                 remove the file manually if it is not acbctl's",
                target.display()
            ));
        }
    }

    if let Some(fragment) = shadowing_unit_fragment(&root, &system_unit_dir) {
        print_shadow_warning(&fragment, &root);
    }

    let status = write_if_changed(&target, &content)?;
    println!("{} {}", status.as_str(), target.display());
    upsert_manifest_entry(&root, USER_UNIT_NAME, &content)?;

    let changed = !matches!(status, WriteStatus::Unchanged);
    if changed {
        systemctl_user(&["daemon-reload"])?;
        println!("daemon-reload: systemd --user");
    }
    if !unit_is_enabled(USER_UNIT_NAME) {
        systemctl_user(&["enable", USER_UNIT_NAME])?;
        println!("enabled {USER_UNIT_NAME}");
    } else if !changed {
        println!("unchanged {USER_UNIT_NAME} (already enabled)");
    }
    Ok(ExitCode::SUCCESS)
}

pub fn uninstall_user_command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_uninstall_user_help();
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(other) = args.first() {
        return Err(format!("unknown argument for uninstall-user: {other}"));
    }

    let root = user_unit_dir()?;
    let target = root.join(USER_UNIT_NAME);
    if let Ok(existing) = fs::read_to_string(&target) {
        if !is_managed(&existing) {
            return Err(format!(
                "refusing to remove {}: no `{MANAGED_HEADER}` header",
                target.display()
            ));
        }
    }

    if unit_loaded(USER_UNIT_NAME) {
        match systemctl_user(&["stop", USER_UNIT_NAME]) {
            Ok(()) => println!("stopped {USER_UNIT_NAME}"),
            Err(error) => println!("warn: {error}"),
        }
    }
    match systemctl_user(&["disable", USER_UNIT_NAME]) {
        Ok(()) => println!("disabled {USER_UNIT_NAME}"),
        Err(error) => println!("note: {error}"),
    }
    match fs::remove_file(&target) {
        Ok(()) => println!("removed {}", target.display()),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            println!("missing {}", target.display());
        }
        Err(error) => return Err(format!("remove {}: {error}", target.display())),
    }
    remove_manifest_entry(&root, USER_UNIT_NAME)?;
    match systemctl_user(&["daemon-reload"]) {
        Ok(()) => println!("daemon-reload: systemd --user"),
        Err(error) => println!("warn: {error}"),
    }
    Ok(ExitCode::SUCCESS)
}

/// systemd 的 user 单元目录：`$XDG_CONFIG_HOME/systemd/user` 优先，否则 `~/.config/systemd/user`。
fn user_unit_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(dir);
        if path.is_absolute() {
            return Ok(path.join("systemd").join("user"));
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; cannot resolve ~/.config/systemd/user".to_string())?;
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

fn systemctl_user(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            stderr.trim()
        ))
    }
}

fn unit_is_enabled(name: &str) -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", name])
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "enabled"
        })
        .unwrap_or(false)
}

fn unit_loaded(name: &str) -> bool {
    let Ok(output) = Command::new("systemctl")
        .args(["--user", "show", "-p", "LoadState", "--value", name])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && state != "not-found"
}

/// 找当前会被 systemd 加载的同名单元；只要不在本次安装目录里就算冲突来源。
fn shadowing_unit_fragment(root: &Path, system_unit_dir: &Path) -> Option<PathBuf> {
    if let Some(fragment) = unit_fragment_path(USER_UNIT_NAME) {
        let path = PathBuf::from(&fragment);
        if !is_inside(&path, root) {
            return Some(path);
        }
    }
    let system_unit = system_unit_dir.join(USER_UNIT_NAME);
    if system_unit.exists() {
        return Some(system_unit);
    }
    None
}

fn is_inside(path: &Path, root: &Path) -> bool {
    let canonical = |candidate: &Path| {
        candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf())
    };
    canonical(path).starts_with(canonical(root))
}

fn unit_fragment_path(name: &str) -> Option<String> {
    let output = Command::new("systemctl")
        .args(["--user", "show", "-p", "FragmentPath", "--value", name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then_some(path)
}

fn print_shadow_warning(fragment: &Path, root: &Path) {
    println!(
        "warn: 检测到同名单元 {USER_UNIT_NAME} 由 nix-config 提供：{}",
        fragment.display()
    );
    println!(
        "      当前生效的是 nix 版本；安装到 {} 后，用户目录版本优先级更高，",
        root.display()
    );
    println!("      会遮蔽 nix 版本。请随后从 nix-config 删除该单元定义，避免双重来源。");
}

fn upsert_manifest_entry(root: &Path, name: &str, content: &str) -> Result<(), String> {
    let mut manifest = read_manifest(root)?.unwrap_or(Manifest {
        version: MANIFEST_VERSION,
        files: BTreeSet::new(),
        hashes: BTreeMap::new(),
    });
    manifest.files.insert(name.to_string());
    manifest.hashes.insert(
        name.to_string(),
        crate::fsutil::sha256_hex(content.as_bytes()),
    );
    write_manifest_if_changed(root, &manifest)
}

fn remove_manifest_entry(root: &Path, name: &str) -> Result<(), String> {
    let Some(mut manifest) = read_manifest(root)? else {
        return Ok(());
    };
    manifest.files.remove(name);
    manifest.hashes.remove(name);
    if manifest.files.is_empty() {
        let path = root.join(MANIFEST_NAME);
        match fs::remove_file(&path) {
            Ok(()) => println!("removed {MANIFEST_NAME}"),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", path.display())),
        }
        Ok(())
    } else {
        write_manifest_if_changed(root, &manifest)
    }
}

/// 只读已装文件和 manifest 元数据；不读 env 文件内容。
pub fn doctor_command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_doctor_help();
        return Ok(ExitCode::SUCCESS);
    }

    let root = parse_root_arg(args, "doctor")?;
    let Some(manifest) = read_manifest(&root)? else {
        println!(
            "drift: manifest not found: {}",
            root.join(MANIFEST_NAME).display()
        );
        return Ok(ExitCode::from(1));
    };

    let mut drift = false;
    for name in &manifest.files {
        let path = root.join(name);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                println!("drift: {name}: installed file missing");
                drift = true;
                continue;
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };

        match manifest.hashes.get(name) {
            Some(expected) => {
                let actual = crate::fsutil::sha256_hex(text.as_bytes());
                if &actual != expected {
                    println!(
                        "drift: {name}: sha256 mismatch (manifest {expected}, installed {actual})"
                    );
                    drift = true;
                }
            }
            None => {
                println!("drift: {name}: manifest has no sha256 record");
                drift = true;
            }
        }

        for line in text.lines() {
            if let Some(program) = exec_start_program(line) {
                check_exec_start_program(name, &program, &mut drift);
            }
        }
    }

    let home = std::env::var_os("HOME")
        .ok_or_else(|| "HOME is not set; cannot resolve ~/.config/systemd/user".to_string())?;
    let user_unit_dir = PathBuf::from(home).join(".config/systemd/user");

    for name in &manifest.files {
        let output = Command::new("systemctl")
            .args(["--user", "show", "-p", "FragmentPath", name])
            .output()
            .map_err(|error| format!("run systemctl --user show {name}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "systemctl --user show {name} failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let fragment = stdout
            .lines()
            .find_map(|line| line.trim().strip_prefix("FragmentPath="));
        match fragment {
            Some("") => {
                println!("drift: {name}: empty FragmentPath (unit not loaded)");
                drift = true;
            }
            Some(path) => {
                let fragment_path = Path::new(path);
                if !fragment_path.starts_with(&user_unit_dir) {
                    println!(
                        "drift: {name}: FragmentPath {path} is outside {}",
                        user_unit_dir.display()
                    );
                    drift = true;
                }
            }
            None => {
                println!("drift: {name}: systemctl output missing FragmentPath");
                drift = true;
            }
        }
    }

    match crate::profile::status() {
        crate::profile::Status::Missing { root } => {
            println!(
                "warn: profile not used; using fallback ACB_UNIT_REPO_DIR/ACB_UNIT_BIN_DIR paths (no current at {})",
                root.display()
            );
        }
        crate::profile::Status::Active { id, bin, .. } => {
            println!("profile: current {id} ({})", bin.display());
        }
        crate::profile::Status::Broken { root, detail } => {
            println!(
                "drift: profile current broken at {}: {detail}",
                root.display()
            );
            drift = true;
        }
    }

    if drift {
        Ok(ExitCode::from(1))
    } else {
        println!("ok: {} units verified", manifest.files.len());
        Ok(ExitCode::SUCCESS)
    }
}

fn exec_start_program(line: &str) -> Option<String> {
    let value = line.trim_start().strip_prefix("ExecStart=")?;
    let mut rest = value.trim();
    while let Some(first) = rest.chars().next() {
        if matches!(first, '-' | '@' | '+' | '!') {
            rest = &rest[first.len_utf8()..];
        } else {
            break;
        }
    }
    let token = rest.split_whitespace().next()?;
    let token = token.trim_matches(|character| character == '"' || character == '\'');
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn check_exec_start_program(unit: &str, program: &str, drift: &mut bool) {
    let path = Path::new(program);
    if !path.is_absolute() {
        println!("drift: {unit}: ExecStart program is not absolute: {program}");
        *drift = true;
        return;
    }
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && is_executable(metadata.permissions()) => {}
        Ok(metadata) if !metadata.is_file() => {
            println!("drift: {unit}: ExecStart program is not a file: {program}");
            *drift = true;
        }
        Ok(_) => {
            println!("drift: {unit}: ExecStart program is not executable: {program}");
            *drift = true;
        }
        Err(error) => {
            println!("drift: {unit}: ExecStart program missing: {program} ({error})");
            *drift = true;
        }
    }
}

#[cfg(unix)]
fn is_executable(permissions: fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    permissions.mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_permissions: fs::Permissions) -> bool {
    true
}

fn parse_render_args(args: &[String]) -> Result<RenderMode, String> {
    let mut print = false;
    let mut output_dir = None;
    let mut diff = false;
    let mut root = None;

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--print" => print = true,
            "--diff" => diff = true,
            "--output-dir" => {
                output_dir = Some(PathBuf::from(next_value(args, &mut index, "--output-dir")?));
            }
            "--root" => {
                root = Some(PathBuf::from(next_value(args, &mut index, "--root")?));
            }
            _ if arg.starts_with("--output-dir=") => {
                output_dir = Some(PathBuf::from(non_empty_value(arg, "--output-dir")?));
            }
            _ if arg.starts_with("--root=") => {
                root = Some(PathBuf::from(non_empty_value(arg, "--root")?));
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        index += 1;
    }

    let modes = u8::from(print) + u8::from(output_dir.is_some()) + u8::from(diff);
    if modes != 1 {
        return Err("render requires exactly one of --print, --output-dir, --diff".to_string());
    }

    if diff {
        let root = root.ok_or_else(|| "--diff requires --root DIR".to_string())?;
        return Ok(RenderMode::Diff(root));
    }
    if root.is_some() {
        return Err("--root is only valid with --diff".to_string());
    }
    if let Some(dir) = output_dir {
        return Ok(RenderMode::OutputDir(dir));
    }
    Ok(RenderMode::Print)
}

fn parse_root_arg(args: &[String], command: &str) -> Result<PathBuf, String> {
    let mut root = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--root" => {
                root = Some(PathBuf::from(next_value(args, &mut index, "--root")?));
            }
            _ if arg.starts_with("--root=") => {
                root = Some(PathBuf::from(non_empty_value(arg, "--root")?));
            }
            other => return Err(format!("unknown argument for {command}: {other}")),
        }
        index += 1;
    }
    root.ok_or_else(|| format!("{command} requires --root DIR"))
}

struct InstallArgs {
    root: PathBuf,
    /// `None` 表示沿用 `ACB_UNIT_INSTALL_SECTION` 环境值；`Some("1")` 才写 `[Install]`。
    install_section: Option<String>,
    /// `user` / `system`，决定 stack target 的 `WantedBy`。
    scope: Option<String>,
}

fn parse_install_args(args: &[String]) -> Result<InstallArgs, String> {
    let mut root = None;
    let mut install_section = None;
    let mut scope = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--root" => {
                root = Some(PathBuf::from(next_value(args, &mut index, "--root")?));
            }
            "--scope" => {
                let value = next_value(args, &mut index, "--scope")?;
                scope = Some(validate_scope(&value)?);
            }
            "--with-install-section" => {
                // 允许 `--with-install-section 0|1`，也允许单独出现表示 1。
                match args.get(index + 1) {
                    Some(value) if value == "0" || value == "1" => {
                        install_section = Some(value.clone());
                        index += 1;
                    }
                    _ => install_section = Some("1".to_string()),
                }
            }
            "--no-install-section" => install_section = Some("0".to_string()),
            _ if arg.starts_with("--root=") => {
                root = Some(PathBuf::from(non_empty_value(arg, "--root")?));
            }
            _ if arg.starts_with("--scope=") => {
                scope = Some(validate_scope(&non_empty_value(arg, "--scope")?)?);
            }
            _ if arg.starts_with("--with-install-section=") => {
                let value = non_empty_value(arg, "--with-install-section")?;
                if value != "0" && value != "1" {
                    return Err(format!("--with-install-section must be 0 or 1: {value}"));
                }
                install_section = Some(value);
            }
            other => return Err(format!("unknown argument for install: {other}")),
        }
        index += 1;
    }
    Ok(InstallArgs {
        root: root.ok_or_else(|| "install requires --root DIR".to_string())?,
        install_section,
        scope,
    })
}

fn validate_scope(value: &str) -> Result<String, String> {
    match value {
        "user" | "system" => Ok(value.to_string()),
        other => Err(format!("--scope must be user or system: {other}")),
    }
}

fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn non_empty_value(arg: &str, flag: &str) -> Result<String, String> {
    let value = arg
        .split_once('=')
        .map(|(_, value)| value)
        .unwrap_or_default();
    if value.is_empty() {
        return Err(format!("{flag} requires a value"));
    }
    Ok(value.to_string())
}

fn render_all_with_install_section(
    install_section: Option<&str>,
    scope: Option<&str>,
) -> Result<Vec<RenderedUnit>, String> {
    let mut values = resolved_values()?;
    apply_install_section(&mut values, install_section, scope)?;
    TEMPLATES
        .iter()
        .map(|(name, template)| {
            let content = substitute(name, template, &values)?;
            Ok(RenderedUnit { name, content })
        })
        .collect()
}

fn render_all() -> Result<Vec<RenderedUnit>, String> {
    render_all_with_install_section(None, None)
}

/// 从哪里取 launcher 路径。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LauncherSource {
    /// 沿用既有 `unit render|install`：`ACB_UNIT_LAUNCHER_BIN` 或 `ACB_UNIT_ACBL_PREFIX`。
    Configured,
    /// `unit install-user`：从 acbctl 自身的安装前缀推导，避免写死 NixOS 路径。
    CurrentExe,
}

/// 先收集 ACB_UNIT_*，再把 BIN_DIR / REPO_DIR 换成 profile/current 下的路径（可用时）。
pub(crate) fn resolved_values() -> Result<BTreeMap<String, String>, String> {
    resolve_values(LauncherSource::Configured)
}

fn resolved_user_unit_values() -> Result<BTreeMap<String, String>, String> {
    resolve_values(LauncherSource::CurrentExe)
}

fn resolve_values(source: LauncherSource) -> Result<BTreeMap<String, String>, String> {
    let mut values = values_from_env()?;
    let fallback_bin = values.get("ACB_UNIT_BIN_DIR").map(PathBuf::from);
    let bin_dir = crate::profile::resolve_bin_dir(fallback_bin.as_deref())?;
    let fallback_repo = values.get("ACB_UNIT_REPO_DIR").map(PathBuf::from);
    let repo_dir = crate::profile::resolve_repo_dir(fallback_repo.as_deref())?;
    values.insert(
        "ACB_UNIT_BIN_DIR".to_string(),
        bin_dir.to_string_lossy().into_owned(),
    );
    values.insert(
        "ACB_UNIT_REPO_DIR".to_string(),
        repo_dir.to_string_lossy().into_owned(),
    );

    // state 必须落在 profile 外的稳定目录，不能跟着 generation 换。
    let profile_dir = PathBuf::from(
        values
            .get("ACB_UNIT_PROFILE_DIR")
            .expect("required key validated above"),
    );
    let state_root = profile_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| profile_dir.clone());
    let state_dir = values
        .get("ACB_UNIT_STATE_DIR")
        .cloned()
        .unwrap_or_else(|| {
            state_root
                .join("state")
                .join("supervisor")
                .to_string_lossy()
                .into_owned()
        });
    let supervisor_config = values
        .get("ACB_UNIT_SUPERVISOR_CONFIG_FILE")
        .cloned()
        .unwrap_or_else(|| {
            Path::new(&state_dir)
                .join("supervisor.toml")
                .to_string_lossy()
                .into_owned()
        });
    let work_dir = values
        .get("ACB_UNIT_WORK_DIR")
        .cloned()
        .unwrap_or_else(|| state_root.to_string_lossy().into_owned());

    // launcher 是独立安装的静态 acbl，不随 ACB generation 换代。
    // 前缀默认 /usr/local（对应 C-9 的 /usr/local/bin/acb-launcher 软链），
    // NixOS 部署可覆盖为 /run/current-system/sw。
    let acbl_prefix = values
        .get("ACB_UNIT_ACBL_PREFIX")
        .cloned()
        .unwrap_or_else(|| "/usr/local".to_owned());
    let launcher_bin = match source {
        LauncherSource::Configured => resolve_launcher_bin(
            &acbl_prefix,
            values.get("ACB_UNIT_LAUNCHER_BIN").map(String::as_str),
            &profile_dir,
            &bin_dir,
        )?,
        LauncherSource::CurrentExe => match values.get("ACB_UNIT_LAUNCHER_BIN") {
            Some(explicit) => explicit.clone(),
            None => launcher_bin_from_current_exe()?,
        },
    };
    values.insert("ACB_UNIT_ACBL_PREFIX".to_string(), acbl_prefix);
    values.insert("ACB_UNIT_STATE_DIR".to_string(), state_dir);
    values.insert("ACB_UNIT_WORK_DIR".to_string(), work_dir);
    values.insert(
        "ACB_UNIT_SUPERVISOR_CONFIG_FILE".to_string(),
        supervisor_config,
    );
    values.insert("ACB_UNIT_LAUNCHER_BIN".to_string(), launcher_bin);
    Ok(values)
}

/// 从 acbctl 自身路径推导同包装的 launcher。
///
/// `/run/current-system/sw/bin/acbctl` 的父目录是符号链接目录，先 `canonicalize`
/// 解析到真实安装前缀，再取同目录的 `acb-launcher`；不写死任何系统路径。
fn launcher_bin_from_current_exe() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate acbctl executable: {error}"))?;
    launcher_bin_for_exe(&exe)
}

fn launcher_bin_for_exe(exe: &Path) -> Result<String, String> {
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let bin_dir = exe
        .parent()
        .ok_or_else(|| format!("cannot derive bin dir from {}", exe.display()))?;
    Ok(bin_dir.join("acb-launcher").to_string_lossy().into_owned())
}

/// 解析静态 acbl 的安装路径，并拒绝任何落在 profile/generation 里的
/// payload launcher（否则 acbl 会成为版本树的一部分，换代后旧 acbl 仍在跑）。
fn resolve_launcher_bin(
    acbl_prefix: &str,
    explicit: Option<&str>,
    profile_dir: &Path,
    bin_dir: &Path,
) -> Result<String, String> {
    let launcher = match explicit {
        Some(value) => PathBuf::from(value),
        None => Path::new(acbl_prefix).join("bin").join("acb-launcher"),
    };
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let profile = canonical(profile_dir);
    let launcher_canonical = canonical(&launcher);
    let inside_profile = launcher_canonical.starts_with(&profile)
        || launcher
            .components()
            .any(|component| component.as_os_str() == "generations");
    let legacy_default = canonical(&bin_dir.join("acb-launcher")) == launcher_canonical
        && bin_dir.starts_with(profile_dir);
    if inside_profile || legacy_default {
        return Err(format!(
            "ACB_UNIT_LAUNCHER_BIN must point to the static acbl install, not an ACB payload \
             (got {}); set ACB_UNIT_ACBL_PREFIX (default /usr/local) or an explicit static \
             ACB_UNIT_LAUNCHER_BIN",
            launcher.display()
        ));
    }
    Ok(launcher.to_string_lossy().into_owned())
}

pub(crate) fn values_from_env() -> Result<BTreeMap<String, String>, String> {
    let mut values = BTreeMap::new();

    for (key, value) in std::env::vars() {
        if !key.starts_with("ACB_UNIT_") {
            continue;
        }
        if !is_known_key(&key) {
            return Err(format!("unknown ACB_UNIT_* variable: {key}"));
        }
        values.insert(key, value);
    }

    for key in REQUIRED_KEYS {
        let value = values
            .get(*key)
            .ok_or_else(|| format!("missing required variable: {key}"))?;
        validate_value(key, value)?;
    }

    for (key, default) in KEY_DEFAULTS {
        values
            .entry((*key).to_string())
            .or_insert_with(|| (*default).to_string());
    }
    for key in PORT_KEYS {
        let value = values.get(*key).expect("defaults cover every port key");
        validate_port(key, value)?;
    }

    apply_install_section(&mut values, None, None)?;
    Ok(values)
}

/// `ACB_UNIT_INSTALL_SECTION` 控制是否在渲染结果里写 `[Install]`；它只负责渲染，
/// 不代表 `systemctl enable` 已被执行。
/// `--with-install-section 0|1` 显式覆盖环境值；`scope` 只影响 stack target 的 `WantedBy`。
fn apply_install_section(
    values: &mut BTreeMap<String, String>,
    render_install_section: Option<&str>,
    scope: Option<&str>,
) -> Result<(), String> {
    let enabled = match render_install_section {
        Some(value) => value.to_string(),
        None => values
            .get("ACB_UNIT_INSTALL_SECTION")
            .cloned()
            .expect("defaults cover install section"),
    };
    let (user_section, system_section) = match enabled.as_str() {
        "0" => (String::new(), String::new()),
        "1" => (
            "\n[Install]\nWantedBy=default.target".to_string(),
            "\n[Install]\nWantedBy=multi-user.target".to_string(),
        ),
        other => return Err(format!("ACB_UNIT_INSTALL_SECTION must be 0 or 1: {other}")),
    };
    let stack_section = match scope {
        Some("user") => user_section.clone(),
        Some("system") => system_section.clone(),
        Some(other) => return Err(format!("--scope must be user or system: {other}")),
        // 未指定 scope 时沿用旧行为：stack target 跟随 user 安装段。
        None => user_section.clone(),
    };
    values.insert("ACB_UNIT_INSTALL_SECTION".to_string(), enabled);
    values.insert("INSTALL_SECTION".to_string(), user_section);
    values.insert("SYSTEM_INSTALL_SECTION".to_string(), system_section);
    values.insert("STACK_INSTALL_SECTION".to_string(), stack_section);
    Ok(())
}

fn is_known_key(key: &str) -> bool {
    REQUIRED_KEYS.contains(&key) || OPTIONAL_KEYS.contains(&key)
}

fn validate_value(key: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{key} must not be empty"));
    }
    if value.contains('\n') || value.contains('\r') || value.contains('\0') {
        return Err(format!("{key} must not contain line breaks or NUL"));
    }
    if (key.ends_with("_DIR")
        || key.ends_with("_FILE")
        || key.ends_with("_BIN")
        || key.ends_with("_PREFIX")
        || key == "ACB_UNIT_SHELL")
        && !value.starts_with('/')
    {
        return Err(format!("{key} must be an absolute path: {value}"));
    }
    Ok(())
}

fn validate_port(key: &str, value: &str) -> Result<(), String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("{key} must be a decimal port: {value}"))?;
    if port == 0 {
        return Err(format!("{key} must be in 1..=65535"));
    }
    Ok(())
}

pub(crate) fn substitute(
    template_name: &str,
    template: &str,
    values: &BTreeMap<String, String>,
) -> Result<String, String> {
    let mut output = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(start) = rest.find('@') {
        output.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let Some(end) = tail.find('@') else {
            return Err(format!("template {template_name}: unclosed placeholder"));
        };
        let key = &tail[..end];
        let value = values
            .get(key)
            .ok_or_else(|| format!("template {template_name}: unknown placeholder @{key}@"))?;
        output.push_str(value);
        rest = &tail[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn diff_against(root: &Path, units: &[RenderedUnit]) -> Result<bool, String> {
    let mut rendered_names = BTreeSet::new();
    let mut same = true;

    for unit in units {
        rendered_names.insert(unit.name.to_string());
        let path = root.join(unit.name);
        match fs::read_to_string(&path) {
            Ok(existing) if existing == unit.content => {}
            Ok(_) => {
                println!("~ {} (content differs)", unit.name);
                same = false;
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                println!("+ {} (missing)", unit.name);
                same = false;
            }
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        }
    }

    if let Some(manifest) = read_manifest(root)? {
        for name in manifest.files.difference(&rendered_names) {
            println!("- {name} (managed but not rendered)");
            same = false;
        }
    }

    if same {
        println!("no differences");
    }
    Ok(same)
}

fn write_if_changed(path: &Path, content: &str) -> Result<WriteStatus, String> {
    match fs::read_to_string(path) {
        Ok(existing) if existing == content => Ok(WriteStatus::Unchanged),
        Ok(_) => {
            crate::fsutil::write_atomic(path, content.as_bytes())?;
            Ok(WriteStatus::Updated)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            crate::fsutil::write_atomic(path, content.as_bytes())?;
            Ok(WriteStatus::Created)
        }
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn read_manifest(root: &Path) -> Result<Option<Manifest>, String> {
    let path = root.join(MANIFEST_NAME);
    match fs::read_to_string(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
        Ok(text) => {
            let manifest: Manifest = serde_json::from_str(&text)
                .map_err(|error| format!("parse {}: {error}", path.display()))?;
            if manifest.version != MANIFEST_VERSION {
                return Err(format!(
                    "{}: unsupported manifest version {}",
                    path.display(),
                    manifest.version
                ));
            }
            for name in &manifest.files {
                validate_unit_name(name)?;
            }
            for name in manifest.hashes.keys() {
                validate_unit_name(name)?;
            }
            Ok(Some(manifest))
        }
    }
}

fn write_manifest_if_changed(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let path = root.join(MANIFEST_NAME);
    let text = serde_json::to_string_pretty(manifest)
        .map_err(|error| format!("serialize manifest: {error}"))?
        + "\n";

    match fs::read_to_string(&path) {
        Ok(existing) if existing == text => Ok(()),
        Ok(_) => {
            crate::fsutil::write_atomic(&path, text.as_bytes())?;
            println!("wrote {MANIFEST_NAME}");
            Ok(())
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            crate::fsutil::write_atomic(&path, text.as_bytes())?;
            println!("wrote {MANIFEST_NAME}");
            Ok(())
        }
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn remove_orphan(root: &Path, name: &str) -> Result<(), String> {
    let path = root.join(name);
    match fs::read_to_string(&path) {
        Ok(text) if is_managed(&text) => {
            fs::remove_file(&path)
                .map_err(|error| format!("remove {}: {error}", path.display()))?;
            println!("removed {name}");
        }
        Ok(_) => println!("kept {name} (no {MANAGED_HEADER} header)"),
        Err(error) if error.kind() == ErrorKind::NotFound => println!("missing {name}"),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    }
    Ok(())
}

fn remove_managed_file(root: &Path, name: &str) -> Result<(), String> {
    let path = root.join(name);
    match fs::read_to_string(&path) {
        Ok(text) if is_managed(&text) => {
            fs::remove_file(&path)
                .map_err(|error| format!("remove {}: {error}", path.display()))?;
            println!("removed {name}");
        }
        Ok(_) => println!("kept {name} (no {MANAGED_HEADER} header)"),
        Err(error) if error.kind() == ErrorKind::NotFound => println!("missing {name}"),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    }
    Ok(())
}

fn is_managed(text: &str) -> bool {
    text.lines()
        .next()
        .is_some_and(|line| line.starts_with(MANAGED_HEADER))
}

fn validate_unit_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
    {
        return Err(format!("invalid unit file name in manifest: {name:?}"));
    }
    Ok(())
}

fn print_reload_hint() {
    println!("daemon-reload not executed; run:");
    println!("  systemctl --user daemon-reload  # user units");
    println!("  systemctl daemon-reload         # system units");
}

fn is_help(args: &[String]) -> bool {
    args.first()
        .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
}

fn print_render_help() {
    println!("Usage: acbctl unit render --print | --output-dir DIR | --diff --root DIR");
}

fn print_install_help() {
    println!(
        "Usage: acbctl unit install --root DIR [--scope user|system] \
         [--with-install-section 0|1]"
    );
    println!();
    println!("  --root DIR         unit 文件写到哪里（user 通常是 ~/.config/systemd/user，");
    println!("                     system 通常是 /etc/systemd/system）");
    println!("  --scope user|system  决定 stack target 的 [Install] WantedBy");
    println!("  --with-install-section 1  在 unit 里渲染 [Install] 段；0 或省略则不渲染");
    println!("                     （只渲染文件；enable/daemon-reload 由部署者执行：");
    println!(
        "                      systemd --user: systemctl --user enable --now acb-launcher-user.service"
    );
    println!(
        "                      system scope:   sudo systemctl enable --now acb-launcher.service）"
    );
}

fn print_uninstall_help() {
    println!("Usage: acbctl unit uninstall --root DIR");
    println!();
    println!("只删除本工具渲染并记录的 unit 文件；没有 --purge。");
    println!("profile、LKG 与状态目录永不删除。");
}

fn print_install_user_help() {
    println!("Usage: acbctl unit install-user");
    println!();
    println!("写入 ~/.config/systemd/user/acb-bridge.service（$XDG_CONFIG_HOME 优先），");
    println!("然后 systemctl --user daemon-reload 并 enable（WantedBy=default.target）。");
    println!("ExecStart 从 acbctl 自身安装前缀推导同目录的 acb-launcher。");
    println!();
    println!("若检测到 nix-config 提供的同名单元，会提示：用户目录优先级更高，");
    println!("安装后会遮蔽 nix 版本；请随后删除 nix-config 里的定义，避免双重来源。");
}

fn print_uninstall_user_help() {
    println!("Usage: acbctl unit uninstall-user");
    println!();
    println!("停止并 disable acb-bridge.service，删除本工具管理的用户单元文件，");
    println!("最后 systemctl --user daemon-reload。");
}

fn print_doctor_help() {
    println!("Usage: acbctl unit doctor --root DIR");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn launcher_is_sibling_of_canonical_exe() {
        let root = std::env::temp_dir().join(format!(
            "acb-unit-user-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let bin = root.join("prefix").join("bin");
        fs::create_dir_all(&bin).expect("create bin");
        let exe = bin.join("acbctl");
        fs::write(&exe, b"fake").expect("write exe");

        assert_eq!(
            PathBuf::from(launcher_bin_for_exe(&exe).expect("derive launcher")),
            bin.join("acb-launcher")
        );

        // /run/current-system/sw/bin 这类符号链接目录要先解析到真实前缀。
        let linked = root.join("sw").join("bin");
        fs::create_dir_all(linked.parent().expect("sw parent")).expect("create sw");
        std::os::unix::fs::symlink(&bin, &linked).expect("symlink bin");
        assert_eq!(
            PathBuf::from(launcher_bin_for_exe(&linked.join("acbctl")).expect("derive launcher")),
            bin.join("acb-launcher")
        );

        fs::remove_dir_all(&root).ok();
    }
}
