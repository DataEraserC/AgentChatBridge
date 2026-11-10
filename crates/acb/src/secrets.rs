//! 密钥来源运维命令：`acbctl secrets check` / `resolve`。
//!
//! 输出只含来源名、kind、路径或变量名。`resolve` 才打印值，供部署脚本
//! 把「按会话 / 控制台」的密钥交给子进程；配置文件里始终只有引用。

use std::path::PathBuf;
use std::process::ExitCode;

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
        "migrate" => migrate_command(&args[1..]),
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

fn migrate_command(args: &[String]) -> Result<ExitCode, String> {
    let mut check = false;
    let mut rest = Vec::new();
    for arg in args {
        if arg == "--check" {
            check = true;
        } else {
            rest.push(arg.clone());
        }
    }
    let options = Options::parse(&rest)?;
    let loaded = load(&options)?;
    let key = acb_kernel::crypto::AcbKey::load()?
        .ok_or_else(|| "ACB_KEY_FILE / ACB_KEY is required to migrate secrets".to_string())?;
    let store_path = loaded.config.storage.path.clone();
    let mut store = acb_kernel::SqliteStore::open(&store_path)
        .map_err(|error| format!("open store {store_path}: {error}"))?;
    use acb_kernel::storage::SecretsStore;
    let mut migrated = 0usize;
    for entry in loaded.secrets.entries() {
        if entry.kind == "db" {
            continue;
        }
        let Some(value) = loaded.secrets.value(&entry.name) else {
            continue;
        };
        if !check {
            let encrypted = key
                .encrypt(value)
                .map_err(|error| format!("encrypt {}: {error}", entry.name))?;
            store
                .secret_put(
                    &entry.name,
                    &encrypted,
                    true,
                    Some(key.kid()),
                    Some(&entry.origin),
                    now_ms(),
                )
                .map_err(|error| format!("write secret {}: {error}", entry.name))?;
        }
        println!(
            "{}\t{}\t{}",
            entry.name,
            entry.kind,
            if check { "would migrate" } else { "migrated" }
        );
        migrated += 1;
    }
    let already = loaded
        .secrets
        .entries()
        .iter()
        .filter(|entry| entry.kind == "db")
        .count();
    if check {
        println!("dry run: {migrated} source(s) would be migrated; {already} already in database");
    } else {
        println!("migrated {migrated} source(s); {already} already in database");
    }
    Ok(ExitCode::SUCCESS)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn load(options: &Options) -> Result<acb_kernel::LoadedConfig, String> {
    let config = options.desired_config();
    let config_dir = options
        .config_dir
        .clone()
        .unwrap_or_else(|| crate::config_source::default_config_dir(&config));
    // --session 的 token 在库里（schema 15）：只读文件会报 unknown session。
    crate::config_source::load(&config, Some(&config_dir))
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
  acbctl secrets resolve --console [--config PATH] [--config-dir DIR]
  acbctl secrets migrate [--check] [--config PATH] [--config-dir DIR]"
    );
}
