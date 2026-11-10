//! `acbctl settings`：运行时可变的全局设置。
//!
//! 初始值在代码里；这个命令只读写库，不碰配置文件。值用 JSON 表示：
//! `acbctl settings set delivery_limit 20` 也接受裸数字/字符串。

use std::path::PathBuf;
use std::process::ExitCode;

use acb_kernel::storage::SettingsStore;

const EXIT_USAGE: u8 = 2;

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    let Some(subcommand) = args.first() else {
        print_help();
        return Ok(ExitCode::from(EXIT_USAGE));
    };
    match subcommand.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "list" => list_command(&args[1..]),
        "set" => set_command(&args[1..]),
        "delete" => delete_command(&args[1..]),
        other => Err(format!("unknown settings subcommand: {other}")),
    }
}

fn list_command(args: &[String]) -> Result<ExitCode, String> {
    let (config, config_dir) = options(args)?;
    let loaded = load(&config, config_dir)?;
    let store = open(&loaded)?;
    for (key, value) in store
        .settings_load()
        .map_err(|error| format!("read settings: {error}"))?
    {
        println!("{key}\t{value}");
    }
    Ok(ExitCode::SUCCESS)
}

fn set_command(args: &[String]) -> Result<ExitCode, String> {
    let (config, config_dir, positional) = options_with_positional(args, 2)?;
    let key = &positional[0];
    let raw = &positional[1];
    let value: serde_json::Value = serde_json::from_str(raw).unwrap_or_else(|_| {
        // 裸字符串（不是合法 JSON）按字符串存，省得每次都写引号。
        serde_json::Value::String(raw.clone())
    });
    let loaded = load(&config, config_dir)?;
    let mut store = open(&loaded)?;
    store
        .settings_put(key, &value, now_ms())
        .map_err(|error| format!("write setting {key}: {error}"))?;
    println!("{key} = {value}");
    Ok(ExitCode::SUCCESS)
}

fn delete_command(args: &[String]) -> Result<ExitCode, String> {
    let (config, config_dir, positional) = options_with_positional(args, 1)?;
    let key = &positional[0];
    let loaded = load(&config, config_dir)?;
    let mut store = open(&loaded)?;
    let removed = store
        .settings_delete(key)
        .map_err(|error| format!("delete setting {key}: {error}"))?;
    println!("{key}: {}", if removed { "deleted" } else { "not found" });
    Ok(ExitCode::SUCCESS)
}

fn load(config: &PathBuf, config_dir: Option<PathBuf>) -> Result<acb_kernel::LoadedConfig, String> {
    let dir = config_dir.unwrap_or_else(|| crate::config_source::default_config_dir(config));
    crate::config_source::load(config, Some(&dir))
}

fn open(loaded: &acb_kernel::LoadedConfig) -> Result<acb_kernel::SqliteStore, String> {
    acb_kernel::SqliteStore::open(&loaded.config.storage.path)
        .map_err(|error| format!("open store {}: {error}", loaded.config.storage.path))
}

fn options(args: &[String]) -> Result<(PathBuf, Option<PathBuf>), String> {
    let (config, dir, _) = options_with_positional(args, 0)?;
    Ok((config, dir))
}

fn options_with_positional(
    args: &[String],
    wanted: usize,
) -> Result<(PathBuf, Option<PathBuf>, Vec<String>), String> {
    let mut config = PathBuf::from("config.toml");
    let mut config_dir = None;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                config = PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--config needs a value".to_string())?,
                );
            }
            "--config-dir" => {
                index += 1;
                config_dir = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--config-dir needs a value".to_string())?,
                ));
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown option: {value}"));
            }
            value => positional.push(value.to_string()),
        }
        index += 1;
    }
    if positional.len() != wanted {
        return Err(format!(
            "expected {wanted} positional argument(s), got {}",
            positional.len()
        ));
    }
    Ok((config, config_dir, positional))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn print_help() {
    println!(
        "Usage:
  acbctl settings list [--config PATH] [--config-dir DIR]
  acbctl settings set KEY VALUE [--config PATH] [--config-dir DIR]
  acbctl settings delete KEY [--config PATH] [--config-dir DIR]"
    );
}
