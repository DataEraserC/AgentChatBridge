//! `acbctl logs [NAME]`：列出并 tail 组件日志与 bridge 日志。
//!
//! 路径不写死，按实际落盘规则推导：
//! - 组件默认 `<state_root(state_dir)>/logs/<name>.log`，与进程管理内核一致；
//!   声明了 `log.target = "file"` 且给了 `log.path` 时用声明值。
//! - bridge 日志取单元文件里的 `StandardOutput=append:<path>`，取不到时退回
//!   XDG state 目录下的 `acb/bridge.log`。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use acb::profile;
use acb::supervisor_owner;
use acb_supervisor::component::LogTarget;
use acb_supervisor::component_state::{self, state_root};
use acb_supervisor::{Config, load_supervisor_config};

const UNIT: &str = "acb-bridge.service";
const BRIDGE_NAME: &str = "bridge";
const DEFAULT_LINES: u64 = 30;

/// 未知名字 / 用法错误。
const EXIT_USAGE: u8 = 2;
/// 名字认识但日志文件还没生成。
const EXIT_NO_LOG: u8 = 1;

struct Source {
    name: String,
    path: PathBuf,
}

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    let options = parse_arguments(args)?;
    if options.help {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    let sources = sources();
    let Some(name) = options.name.as_deref() else {
        let existing: Vec<&Source> = sources
            .iter()
            .filter(|source| source.path.is_file())
            .collect();
        if existing.is_empty() {
            println!("没有可 tail 的日志");
            return Ok(ExitCode::SUCCESS);
        }
        for source in existing {
            println!("{:<16} {}", source.name, source.path.display());
        }
        return Ok(ExitCode::SUCCESS);
    };

    let Some(source) = sources.iter().find(|source| source.name == name) else {
        eprintln!("acbctl: unknown log source: {name}");
        print_available(&sources);
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    if !source.path.is_file() {
        eprintln!(
            "acbctl: log file for {name} does not exist yet: {}",
            source.path.display()
        );
        print_available(&sources);
        return Ok(ExitCode::from(EXIT_NO_LOG));
    }

    tail(&source.path, options.lines, options.follow)
}

struct Options {
    name: Option<String>,
    follow: bool,
    lines: u64,
    help: bool,
}

fn parse_arguments(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        name: None,
        follow: false,
        lines: DEFAULT_LINES,
        help: false,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "-f" | "--follow" => options.follow = true,
            "-h" | "--help" => options.help = true,
            "-n" | "--lines" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{arg} requires a value"))?;
                options.lines = parse_lines(value)?;
            }
            _ if arg.starts_with("-n") => {
                options.lines = parse_lines(&arg[2..])?;
            }
            _ if arg.starts_with("--lines=") => {
                options.lines = parse_lines(&arg["--lines=".len()..])?;
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unknown argument for logs: {arg}"));
            }
            _ => {
                if options.name.is_some() {
                    return Err("logs accepts at most one log name".to_string());
                }
                options.name = Some(arg.clone());
            }
        }
        index += 1;
    }
    Ok(options)
}

fn parse_lines(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid line count: {value}"))
}

fn sources() -> Vec<Source> {
    let mut sources = component_sources();
    if let Some(path) = bridge_log_path() {
        sources.push(Source {
            name: BRIDGE_NAME.to_string(),
            path,
        });
    }
    sources
}

fn component_sources() -> Vec<Source> {
    let config = supervisor_owner::owned_config_path(&profile::default_state_dir());
    component_sources_at(&config, &profile::default_state_dir())
}

/// 组件的日志来源：先按声明的组件取路径，再补上 `<state>/logs/*.log` 里
/// 实际存在的文件（预设有变动或组件表读不出来时仍能 tail 到已有日志）。
fn component_sources_at(config_path: &Path, fallback_state_dir: &Path) -> Vec<Source> {
    let mut seen = BTreeSet::new();
    let mut sources = Vec::new();
    let (state_dir, table) = match load_supervisor_config(config_path) {
        Ok((config, table)) => (
            configured_state_dir(&config, fallback_state_dir),
            Some(table),
        ),
        Err(error) => {
            eprintln!("acbctl logs: cannot read components: {error}");
            (fallback_state_dir.to_path_buf(), None)
        }
    };
    let logs_dir = state_root(&state_dir).join(component_state::LOGS_DIR);

    if let Some(table) = &table {
        for entry in &table.components {
            if !seen.insert(entry.name.clone()) {
                continue;
            }
            let path = match (&entry.log.target, entry.log.path.as_deref()) {
                (LogTarget::File, Some(path)) if !path.is_empty() => PathBuf::from(path),
                _ => logs_dir.join(format!("{}.log", entry.name)),
            };
            sources.push(Source {
                name: entry.name.clone(),
                path,
            });
        }
    }

    let mut scanned = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".log"))
            else {
                continue;
            };
            scanned.push((name.to_string(), path));
        }
    }
    scanned.sort();
    for (name, path) in scanned {
        if seen.insert(name.clone()) {
            sources.push(Source { name, path });
        }
    }
    sources
}

fn configured_state_dir(config: &Config, fallback: &Path) -> PathBuf {
    let configured = std::env::var_os("ACB_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.state_dir));
    if configured.as_os_str().is_empty() {
        fallback.to_path_buf()
    } else {
        configured
    }
}

fn bridge_log_path() -> Option<PathBuf> {
    unit_log_path().or_else(|| {
        let base = std::env::var_os("XDG_STATE_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|value| !value.is_empty())
                    .map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        Some(base.join("acb").join("bridge.log"))
    })
}

/// 从已安装单元里读 `StandardOutput=append:<path>`；没有单元时返回 `None`。
fn unit_log_path() -> Option<PathBuf> {
    let output = Command::new("systemctl")
        .args(["--user", "cat", UNIT])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        if key != "StandardOutput" && key != "StandardError" {
            continue;
        }
        if let Some(path) = value.strip_prefix("append:") {
            let path = path.trim();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }
    None
}

fn print_available(sources: &[Source]) {
    let names: Vec<&str> = sources
        .iter()
        .filter(|source| source.path.is_file())
        .map(|source| source.name.as_str())
        .collect();
    if names.is_empty() {
        eprintln!("available: （没有可 tail 的日志）");
    } else {
        eprintln!("available: {}", names.join(", "));
    }
}

fn tail(path: &Path, lines: u64, follow: bool) -> Result<ExitCode, String> {
    let mut command = Command::new("tail");
    command.arg("-n").arg(lines.to_string());
    if follow {
        command.arg("-f");
    }
    command.arg(path);
    match command.status() {
        Ok(status) if status.success() => Ok(ExitCode::SUCCESS),
        Ok(status) => Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(0, 255) as u8
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("cannot find `tail` in PATH".to_string())
        }
        Err(error) => Err(format!("run tail: {error}")),
    }
}

fn print_help() {
    println!(
        "Usage:
  acbctl logs [NAME] [-f] [-n LINES]

不带 NAME 时列出当前可 tail 的组件日志与 bridge 日志。
NAME 可以是组件名或 `bridge`；-f 跟随新增内容，-n 指定行数（默认 {DEFAULT_LINES}）。"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "acbctl-logs-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn parse_defaults() {
        let options = parse_arguments(&[]).expect("parse");
        assert_eq!(options.name, None);
        assert!(!options.follow);
        assert_eq!(options.lines, DEFAULT_LINES);
    }

    #[test]
    fn parse_name_and_flags_in_any_order() {
        let options = parse_arguments(&[
            "-f".to_string(),
            "dsh".to_string(),
            "-n".to_string(),
            "42".to_string(),
        ])
        .expect("parse");
        assert_eq!(options.name.as_deref(), Some("dsh"));
        assert!(options.follow);
        assert_eq!(options.lines, 42);

        let options = parse_arguments(&["-n7".to_string(), "bridge".to_string()]).expect("parse");
        assert_eq!(options.name.as_deref(), Some("bridge"));
        assert_eq!(options.lines, 7);
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(parse_arguments(&["-n".to_string()]).is_err());
        assert!(parse_arguments(&["-n".to_string(), "x".to_string()]).is_err());
        assert!(parse_arguments(&["-x".to_string()]).is_err());
        assert!(parse_arguments(&["a".to_string(), "b".to_string()]).is_err());
    }

    #[test]
    fn component_paths_follow_state_root_and_overrides() {
        let root = temp_dir("paths");
        let state_dir = root.join("state").join("supervisor");
        let logs_dir = root.join("state").join("logs");
        std::fs::create_dir_all(&logs_dir).expect("create logs dir");
        std::fs::write(logs_dir.join("alpha.log"), "").expect("write alpha log");
        std::fs::write(logs_dir.join("extra.log"), "").expect("write extra log");
        let config = root.join("supervisor.toml");
        std::fs::write(
            &config,
            format!(
                "[supervisor]\nprogram = \"/bin/true\"\nstate_dir = \"{}\"\n\
                 [[component]]\nname = \"alpha\"\ncommand = [\"/bin/true\"]\n\
                 [[component]]\nname = \"beta\"\ncommand = [\"/bin/true\"]\n\
                 [component.log]\ntarget = \"file\"\npath = \"{}/custom/beta.log\"\n",
                state_dir.display(),
                root.display()
            ),
        )
        .expect("write config");

        let sources = component_sources_at(&config, &state_dir);
        let path_of = |name: &str| {
            sources
                .iter()
                .find(|source| source.name == name)
                .map(|source| source.path.clone())
        };
        assert_eq!(path_of("alpha"), Some(logs_dir.join("alpha.log")));
        assert_eq!(path_of("beta"), Some(root.join("custom/beta.log")));
        assert_eq!(path_of("extra"), Some(logs_dir.join("extra.log")));

        let _ = std::fs::remove_dir_all(&root);
    }
}
