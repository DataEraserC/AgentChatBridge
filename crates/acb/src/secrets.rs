//! 密钥来源运维命令：`acbctl secrets check` / `resolve`。
//!
//! 输出只含来源名、kind、路径或变量名。`resolve` 才打印值，供部署脚本
//! 把「按会话 / 控制台」的密钥交给子进程；配置文件里始终只有引用。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acb_kernel::Config;

const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 78;

const DEFAULT_CONFIG: &str = "config.toml";

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    let Some(subcommand) = args.first() else {
        print_help();
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    let result = match subcommand.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "check" => check_command(&args[1..]),
        "resolve" => resolve_command(&args[1..]),
        other => Err(format!("unknown secrets subcommand: {other}")),
    };
    match result {
        Ok(code) => Ok(code),
        Err(message) => {
            eprintln!("acbctl secrets: {message}");
            Ok(ExitCode::from(EXIT_CONFIG))
        }
    }
}

fn check_command(args: &[String]) -> Result<ExitCode, String> {
    let options = Options::parse(args)?;
    let loaded = load(&options)?;
    for entry in loaded.secrets.entries() {
        println!("{}\t{}\t{}", entry.name, entry.kind, entry.origin);
    }
    Ok(ExitCode::SUCCESS)
}

fn resolve_command(args: &[String]) -> Result<ExitCode, String> {
    let options = Options::parse(args)?;
    let loaded = load(&options)?;
    let value = match (&options.name, &options.session, options.console) {
        (Some(name), _, _) => loaded
            .secrets
            .value(name)
            .ok_or_else(|| format!("unknown secret source: {name}"))?,
        (None, Some(session), _) => loaded
            .config
            .session(session)
            .ok_or_else(|| format!("unknown session: {session}"))?
            .token
            .as_str(),
        (None, None, true) => loaded
            .config
            .console
            .as_ref()
            .ok_or_else(|| "config has no [console]".to_string())?
            .token
            .as_str(),
        (None, None, false) => {
            return Err("resolve needs a source name, --session ID, or --console".to_string());
        }
    };
    println!("{value}");
    Ok(ExitCode::SUCCESS)
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
    name: Option<String>,
    session: Option<String>,
    console: bool,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            config: None,
            config_dir: None,
            name: None,
            session: None,
            console: false,
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
                "--session" => {
                    options.session = Some(next(args, &mut index, "--session")?);
                }
                "--console" => options.console = true,
                "-h" | "--help" => {
                    print_help();
                    return Err("help requested".to_string());
                }
                other if other.starts_with('-') => {
                    return Err(format!("unknown option: {other}"));
                }
                other => {
                    if options.name.replace(other.to_owned()).is_some() {
                        return Err(format!("unexpected extra argument: {other}"));
                    }
                }
            }
            index += 1;
        }
        Ok(options)
    }

    fn desired_config(&self) -> PathBuf {
        self.config
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))
    }
}

fn next(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

fn print_help() {
    println!(
        "Usage:
  acbctl secrets check [--config PATH] [--config-dir DIR]
  acbctl secrets resolve NAME [--config PATH] [--config-dir DIR]
  acbctl secrets resolve --session ID [--config PATH] [--config-dir DIR]
  acbctl secrets resolve --console [--config PATH] [--config-dir DIR]"
    );
}
