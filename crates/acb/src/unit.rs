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
use std::process::ExitCode;

use serde::{Deserialize, Serialize};

const MANAGED_HEADER: &str = "# acb-managed:";
const MANIFEST_NAME: &str = "acb-units.manifest.json";
const MANIFEST_VERSION: u32 = 1;

/// 模板在编译期嵌入二进制，部署时不依赖仓库目录仍然存在。
const TEMPLATES: &[(&str, &str)] = &[
    (
        "dsh-acb.service",
        include_str!("../../../deploy/units/systemd/dsh-acb.service.tmpl"),
    ),
    (
        "acb-bridge.service",
        include_str!("../../../deploy/units/systemd/acb-bridge.service.tmpl"),
    ),
    (
        "acb-drivers.service",
        include_str!("../../../deploy/units/systemd/acb-drivers.service.tmpl"),
    ),
    (
        "acb-opencode-serve.service",
        include_str!("../../../deploy/units/systemd/acb-opencode-serve.service.tmpl"),
    ),
    (
        "acb-stack.target",
        include_str!("../../../deploy/units/systemd/acb-stack.target.tmpl"),
    ),
];

const REQUIRED_KEYS: &[&str] = &[
    "ACB_UNIT_REPO_DIR",
    "ACB_UNIT_LOG_DIR",
    "ACB_UNIT_PROFILE_DIR",
    "ACB_UNIT_BIN_DIR",
    "ACB_UNIT_CONFIG_FILE",
    "ACB_UNIT_ENV_FILE",
    "ACB_UNIT_OPENCODE_DIR",
];

const PORT_KEYS: &[&str] = &["ACB_UNIT_CONSOLE_PORT", "ACB_UNIT_OPENCODE_PORT"];

const OPTIONAL_KEYS: &[&str] = &[
    "ACB_UNIT_CONSOLE_PORT",
    "ACB_UNIT_OPENCODE_PORT",
    "ACB_UNIT_AUTOSTART",
];

const KEY_DEFAULTS: &[(&str, &str)] = &[
    ("ACB_UNIT_CONSOLE_PORT", "3100"),
    ("ACB_UNIT_OPENCODE_PORT", "4097"),
    ("ACB_UNIT_AUTOSTART", "0"),
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

    let root = parse_root_arg(args, "install")?;
    let units = render_all()?;
    fs::create_dir_all(&root).map_err(|error| format!("create {}: {error}", root.display()))?;

    let previous = read_manifest(&root)?;
    let mut current = BTreeSet::new();

    for unit in &units {
        current.insert(unit.name.to_string());
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

fn render_all() -> Result<Vec<RenderedUnit>, String> {
    let values = values_from_env()?;
    TEMPLATES
        .iter()
        .map(|(name, template)| {
            let content = substitute(name, template, &values)?;
            Ok(RenderedUnit { name, content })
        })
        .collect()
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

    let autostart = values
        .get("ACB_UNIT_AUTOSTART")
        .expect("defaults cover autostart");
    let install_section = match autostart.as_str() {
        "0" => String::new(),
        "1" => "\n[Install]\nWantedBy=default.target".to_string(),
        other => return Err(format!("ACB_UNIT_AUTOSTART must be 0 or 1: {other}")),
    };
    values.insert("INSTALL_SECTION".to_string(), install_section);

    Ok(values)
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
    if (key.ends_with("_DIR") || key.ends_with("_FILE")) && !value.starts_with('/') {
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
    println!("Usage: acbctl unit install --root DIR");
}

fn print_uninstall_help() {
    println!("Usage: acbctl unit uninstall --root DIR");
}
