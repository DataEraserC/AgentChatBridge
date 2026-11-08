//! `acbctl sessions`：从 kernel 配置推导每个会话的 agent preset 名与 MCP 端口。
//!
//! 推导规则是单一的，`[[sessions]]` 里不再新增 `mcp_port` / `agent_preset`：
//!   - preset 名 = `acb-<session id>`
//!   - MCP 端口 = `3101 + 会话在 [[sessions]] 里的下标`
//!
//! `dsh-presets` 与 `drivers` 两个组件脚本都消费这份输出，避免两处各拼一套名字。
//! 输出按行制表符分隔：`id\tharness\tpreset\tport`，不带表头，便于脚本过滤。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acb_kernel::Config;

const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 78;

const DEFAULT_CONFIG: &str = "config.toml";
const PORT_BASE: u16 = 3101;

pub fn command(args: &[String]) -> Result<ExitCode, String> {
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
        let Some(port) = PORT_BASE.checked_add(index as u16) else {
            eprintln!(
                "acbctl sessions: too many sessions; port {} overflows u16",
                PORT_BASE as u32 + index as u32
            );
            return Ok(ExitCode::from(EXIT_CONFIG));
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
        .unwrap_or_else(|| default_config_dir(&config));
    Config::load(&config, Some(&config_dir)).map_err(|error| {
        format!(
            "load config {} (dir {}): {error}",
            config.display(),
            config_dir.display()
        )
    })
}

fn default_config_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("config.d")
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

Derives one row per [[sessions]] entry: id<TAB>harness<TAB>preset<TAB>port
  preset = acb-<id>, port = 3101 + index"
    );
}

#[cfg(test)]
mod tests {
    use super::preset_name;

    #[test]
    fn preset_name_is_prefixed_session_id() {
        assert_eq!(preset_name("main"), "acb-main");
        assert_eq!(preset_name("dsh-qas"), "acb-dsh-qas");
    }
}
