//! `acbctl plugin`：插件注册表的增删改查与导入导出。
//!
//! 只碰库，不碰配置文件。`[[tool_plugins]]` 只在桥首次启动时 seed 一次；
//! 之后所有人（控制台、agent、运维）都走这里或同一个库写入函数。
//!
//! 命令不可执行不阻止注册：桥在启动时把这类插件标 degraded，跳过安装但继续跑。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acb_kernel::plugin_registry::{
    CommandKind, PluginKind, PluginPolicyConfig, PluginStorageConfig, RegistryPlugin,
    check_command as inspect_command, command_fingerprint,
};
use acb_kernel::storage::PluginsStore;

const EXIT_USAGE: u8 = 2;
const EXIT_NOT_FOUND: u8 = 4;

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
        "show" => show_command(&args[1..]),
        "add" => add_command(&args[1..]),
        "remove" => remove_command(&args[1..]),
        "enable" => toggle_command(&args[1..], true),
        "disable" => toggle_command(&args[1..], false),
        "check" => check_all_command(&args[1..]),
        "config" => config_command(&args[1..]),
        "records" => records_command(&args[1..]),
        "export" => export_command(&args[1..]),
        "import" => import_command(&args[1..]),
        other => Err(format!("unknown plugin subcommand: {other}")),
    }
}

fn list_command(args: &[String]) -> Result<ExitCode, String> {
    let (config, config_dir) = options(args, 0)?.into_config();
    let loaded = load(&config, config_dir)?;
    let store = open(&loaded)?;
    let Some(plugins) = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
    else {
        println!("plugins: not imported yet (bridge has never started)");
        return Ok(ExitCode::SUCCESS);
    };
    println!("id\tenabled\tkind\tcommand_kind\tstatus\tfingerprint\tstorage");
    for plugin in plugins {
        let status = inspect_command(&plugin.command, plugin.command_kind, None);
        let fingerprint = if plugin.fingerprint_matches() {
            "ok"
        } else {
            "drift"
        };
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            plugin.id,
            if plugin.enabled { "yes" } else { "no" },
            plugin.kind.as_str(),
            plugin.command_kind.as_str(),
            status.describe(),
            fingerprint,
            plugin.storage.kind.as_str()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn show_command(args: &[String]) -> Result<ExitCode, String> {
    let parsed = options_with_positional(args, 1)?;
    let id = &parsed.positional[0];
    let loaded = load(&parsed.config, parsed.config_dir)?;
    let store = open(&loaded)?;
    let plugins = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
        .unwrap_or_default();
    let Some(plugin) = plugins.into_iter().find(|plugin| &plugin.id == id) else {
        return Err(format!("plugin {id} not found"));
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&plugin).unwrap_or_else(|_| "{}".to_owned())
    );
    Ok(ExitCode::SUCCESS)
}

fn add_command(args: &[String]) -> Result<ExitCode, String> {
    let parsed = add_options(args)?;
    let loaded = load(&parsed.config, parsed.config_dir.clone())?;
    let mut store = open(&loaded)?;

    let manifest_json = match &parsed.manifest {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("read manifest {}: {error}", path.display()))?,
        None => "{}".to_owned(),
    };
    // manifest 至少得是合法 JSON；P4 才校验里面 config/resources/actions 的形状。
    let manifest: serde_json::Value = serde_json::from_str(&manifest_json)
        .map_err(|error| format!("manifest is not valid JSON: {error}"))?;
    let id = parsed
        .id
        .or_else(|| {
            manifest
                .get("id")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .ok_or_else(|| "missing plugin id (use --id or put \"id\" in manifest)".to_string())?;

    let at = now_ms();
    let existing = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
        .unwrap_or_default()
        .into_iter()
        .find(|plugin| plugin.id == id);
    let mut plugin = existing.unwrap_or_else(|| RegistryPlugin {
        id: id.clone(),
        kind: PluginKind::Tool,
        trust_tier: "tool".to_owned(),
        command: String::new(),
        args: Vec::new(),
        timeout_ms: 5000,
        command_kind: CommandKind::Absolute,
        command_fingerprint: None,
        manifest_json: "{}".to_owned(),
        schema_version: 0,
        source_seed: None,
        enabled: true,
        created_at: at,
        updated_at: at,
        version: 1,
        policy: PluginPolicyConfig::default(),
        storage: PluginStorageConfig::default(),
        capabilities: Vec::new(),
    });
    plugin.command = parsed.command.clone();
    plugin.args = parsed.args.clone();
    plugin.timeout_ms = parsed.timeout_ms;
    plugin.command_kind = parsed
        .command_kind
        .unwrap_or_else(|| CommandKind::infer(&parsed.command));
    plugin.command_fingerprint = Some(command_fingerprint(&plugin.command, &plugin.args));
    plugin.manifest_json = manifest_json.clone();
    plugin.updated_at = at;
    if plugin.created_at == 0 {
        plugin.created_at = at;
    }
    plugin.validate()?;

    let status = inspect_command(&plugin.command, plugin.command_kind, None);
    if !status.is_ok() {
        eprintln!(
            "warning: {} — registered but will be skipped at startup",
            status.describe()
        );
    }
    store
        .plugin_put(&plugin, at)
        .map_err(|error| format!("write plugin {id}: {error}"))?;
    println!("{id}: registered ({} )", status.describe());
    Ok(ExitCode::SUCCESS)
}

fn remove_command(args: &[String]) -> Result<ExitCode, String> {
    let parsed = options_with_positional(args, 1)?;
    let id = &parsed.positional[0];
    let loaded = load(&parsed.config, parsed.config_dir)?;
    let mut store = open(&loaded)?;
    let removed = store
        .plugin_delete(id)
        .map_err(|error| format!("delete plugin {id}: {error}"))?;
    if !removed {
        return Err(format!("plugin {id} not found"));
    }
    println!("{id}: removed");
    Ok(ExitCode::SUCCESS)
}

fn toggle_command(args: &[String], enabled: bool) -> Result<ExitCode, String> {
    let parsed = options_with_positional(args, 1)?;
    let id = &parsed.positional[0];
    let loaded = load(&parsed.config, parsed.config_dir)?;
    let mut store = open(&loaded)?;
    let changed = store
        .plugin_set_enabled(id, enabled, now_ms())
        .map_err(|error| format!("update plugin {id}: {error}"))?;
    if !changed {
        return Err(format!("plugin {id} not found"));
    }
    println!("{id}: {}", if enabled { "enabled" } else { "disabled" });
    Ok(ExitCode::SUCCESS)
}

fn check_all_command(args: &[String]) -> Result<ExitCode, String> {
    let (config, config_dir) = options(args, 0)?.into_config();
    let loaded = load(&config, config_dir)?;
    let store = open(&loaded)?;
    let plugins = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
        .unwrap_or_default();
    if plugins.is_empty() {
        println!("plugins: none registered");
        return Ok(ExitCode::SUCCESS);
    }
    let mut bad = 0;
    for plugin in plugins {
        let status = inspect_command(&plugin.command, plugin.command_kind, None);
        let fingerprint = plugin.fingerprint_matches();
        let ok = plugin.enabled && status.is_ok() && fingerprint;
        if !ok {
            bad += 1;
            let reason = if !plugin.enabled {
                "disabled".to_owned()
            } else if !fingerprint {
                "fingerprint drift".to_owned()
            } else {
                status.describe()
            };
            println!("{}\tDEGRADED\t{reason}", plugin.id);
        } else {
            println!("{}\tOK", plugin.id);
        }
    }
    if bad > 0 {
        return Ok(ExitCode::from(EXIT_NOT_FOUND));
    }
    Ok(ExitCode::SUCCESS)
}

/// `plugin config ID [--set KEY=VALUE]...`：不带 --set 就是读，带就写。
fn config_command(args: &[String]) -> Result<ExitCode, String> {
    let mut config = PathBuf::from("config.toml");
    let mut config_dir = None;
    let mut id = None;
    let mut sets: Vec<(String, String)> = Vec::new();
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
            "--set" => {
                index += 1;
                let pair = args
                    .get(index)
                    .ok_or_else(|| "--set needs KEY=VALUE".to_string())?;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| format!("--set expects KEY=VALUE, got {pair}"))?;
                sets.push((key.to_owned(), value.to_owned()));
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown option: {value}"));
            }
            value => {
                if id.is_some() {
                    return Err(format!("unexpected argument: {value}"));
                }
                id = Some(value.to_string());
            }
        }
        index += 1;
    }
    let id = id.ok_or_else(|| "plugin id is required".to_string())?;
    let loaded = load(&config, config_dir)?;
    let mut store = open(&loaded)?;
    let plugins = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
        .unwrap_or_default();
    let plugin = plugins
        .into_iter()
        .find(|plugin| plugin.id == id)
        .ok_or_else(|| format!("plugin {id} not found"))?;

    if sets.is_empty() {
        let view = acb_kernel::plugin_admin::config_view(&store, &plugin)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": view.schema,
                "effective": view.effective,
                "stored": view.stored,
            }))
            .map_err(|error| error.to_string())?
        );
        return Ok(ExitCode::SUCCESS);
    }

    let mut assignments = serde_json::Map::new();
    for (key, raw) in sets {
        let value: serde_json::Value =
            serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
        assignments.insert(key, value);
    }
    let written =
        acb_kernel::plugin_admin::set_config(&mut store, &plugin, &assignments, now_ms())?;
    println!("written: {}", written.join(", "));
    Ok(ExitCode::SUCCESS)
}

/// `plugin records ID RESOURCE`：列出通用资源记录。
fn records_command(args: &[String]) -> Result<ExitCode, String> {
    let parsed = options_with_positional(args, 2)?;
    let id = &parsed.positional[0];
    let resource = &parsed.positional[1];
    let loaded = load(&parsed.config, parsed.config_dir)?;
    let store = open(&loaded)?;
    let plugin = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
        .unwrap_or_default()
        .into_iter()
        .find(|plugin| plugin.id == *id)
        .ok_or_else(|| format!("plugin {id} not found"))?;
    let records = acb_kernel::plugin_admin::list_records(&store, &plugin, resource)?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &records
                .into_iter()
                .map(|record| serde_json::json!({"id": record.record_id, "value": record.value}))
                .collect::<Vec<_>>()
        )
        .map_err(|error| error.to_string())?
    );
    Ok(ExitCode::SUCCESS)
}

fn export_command(args: &[String]) -> Result<ExitCode, String> {
    let (config, config_dir) = options(args, 0)?.into_config();
    let loaded = load(&config, config_dir)?;
    let store = open(&loaded)?;
    let plugins = store
        .plugins_load()
        .map_err(|error| format!("read plugins: {error}"))?
        .unwrap_or_default();
    println!(
        "{}",
        serde_json::to_string_pretty(&plugins).map_err(|error| error.to_string())?
    );
    Ok(ExitCode::SUCCESS)
}

fn import_command(args: &[String]) -> Result<ExitCode, String> {
    let parsed = import_options(args)?;
    let raw = match &parsed.file {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?,
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map_err(|error| format!("read stdin: {error}"))?;
            buffer
        }
    };
    let plugins: Vec<RegistryPlugin> =
        serde_json::from_str(&raw).map_err(|error| format!("parse plugins JSON: {error}"))?;
    for plugin in &plugins {
        plugin.validate()?;
    }
    let loaded = load(&parsed.config, parsed.config_dir)?;
    let mut store = open(&loaded)?;
    store
        .plugins_replace(&plugins, now_ms())
        .map_err(|error| format!("write plugins: {error}"))?;
    println!("imported {} plugin(s)", plugins.len());
    Ok(ExitCode::SUCCESS)
}

struct ParsedOptions {
    config: PathBuf,
    config_dir: Option<PathBuf>,
    positional: Vec<String>,
}

impl ParsedOptions {
    fn into_config(self) -> (PathBuf, Option<PathBuf>) {
        (self.config, self.config_dir)
    }
}

fn options(args: &[String], wanted: usize) -> Result<ParsedOptions, String> {
    options_with_positional(args, wanted)
}

fn options_with_positional(args: &[String], wanted: usize) -> Result<ParsedOptions, String> {
    let mut parsed = ParsedOptions {
        config: PathBuf::from("config.toml"),
        config_dir: None,
        positional: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                parsed.config = PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--config needs a value".to_string())?,
                );
            }
            "--config-dir" => {
                index += 1;
                parsed.config_dir = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--config-dir needs a value".to_string())?,
                ));
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown option: {value}"));
            }
            value => parsed.positional.push(value.to_string()),
        }
        index += 1;
    }
    if parsed.positional.len() != wanted {
        return Err(format!(
            "expected {wanted} positional argument(s), got {}",
            parsed.positional.len()
        ));
    }
    Ok(parsed)
}

struct AddOptions {
    config: PathBuf,
    config_dir: Option<PathBuf>,
    id: Option<String>,
    command: String,
    args: Vec<String>,
    timeout_ms: u64,
    command_kind: Option<CommandKind>,
    manifest: Option<PathBuf>,
}

fn add_options(args: &[String]) -> Result<AddOptions, String> {
    let mut config = PathBuf::from("config.toml");
    let mut config_dir = None;
    let mut id = None;
    let mut command = None;
    let mut plugin_args = Vec::new();
    let mut timeout_ms = 5000u64;
    let mut command_kind = None;
    let mut manifest = None;
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
            "--id" => {
                index += 1;
                id = Some(
                    args.get(index)
                        .ok_or_else(|| "--id needs a value".to_string())?
                        .clone(),
                );
            }
            "--command" => {
                index += 1;
                command = Some(
                    args.get(index)
                        .ok_or_else(|| "--command needs a value".to_string())?
                        .clone(),
                );
            }
            "--arg" => {
                index += 1;
                plugin_args.push(
                    args.get(index)
                        .ok_or_else(|| "--arg needs a value".to_string())?
                        .clone(),
                );
            }
            "--timeout" => {
                index += 1;
                timeout_ms = args
                    .get(index)
                    .ok_or_else(|| "--timeout needs a value".to_string())?
                    .parse()
                    .map_err(|_| "--timeout must be milliseconds".to_string())?;
            }
            "--command-kind" => {
                index += 1;
                let raw = args
                    .get(index)
                    .ok_or_else(|| "--command-kind needs a value".to_string())?;
                command_kind = Some(
                    CommandKind::parse(raw)
                        .ok_or_else(|| format!("unknown command kind: {raw}"))?,
                );
            }
            "--manifest" => {
                index += 1;
                manifest = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--manifest needs a value".to_string())?,
                ));
            }
            value if value.starts_with('-') => {
                return Err(format!("unknown option: {value}"));
            }
            _ => {}
        }
        index += 1;
    }
    let command = command.ok_or_else(|| "--command is required".to_string())?;
    Ok(AddOptions {
        config,
        config_dir,
        id,
        command,
        args: plugin_args,
        timeout_ms,
        command_kind,
        manifest,
    })
}

struct ImportOptions {
    config: PathBuf,
    config_dir: Option<PathBuf>,
    file: Option<PathBuf>,
}

fn import_options(args: &[String]) -> Result<ImportOptions, String> {
    let mut config = PathBuf::from("config.toml");
    let mut config_dir = None;
    let mut file = None;
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
            "--file" => {
                index += 1;
                file = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| "--file needs a value".to_string())?,
                ));
            }
            value => return Err(format!("unknown option: {value}")),
        }
        index += 1;
    }
    Ok(ImportOptions {
        config,
        config_dir,
        file,
    })
}

fn load(config: &Path, config_dir: Option<PathBuf>) -> Result<acb_kernel::LoadedConfig, String> {
    let dir = config_dir.unwrap_or_else(|| crate::config_source::default_config_dir(config));
    crate::config_source::load(config, Some(&dir))
}

fn open(loaded: &acb_kernel::LoadedConfig) -> Result<acb_kernel::SqliteStore, String> {
    acb_kernel::SqliteStore::open(&loaded.config.storage.path)
        .map_err(|error| format!("open store {}: {error}", loaded.config.storage.path))
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
  acbctl plugin list [--config PATH] [--config-dir DIR]
  acbctl plugin show ID [--config PATH] [--config-dir DIR]
  acbctl plugin add --command CMD [--id ID] [--arg VALUE]... [--timeout MS]
                     [--command-kind absolute|profile|nix_store|relative]
                     [--manifest PATH] [--config PATH] [--config-dir DIR]
  acbctl plugin remove ID [--config PATH] [--config-dir DIR]
  acbctl plugin enable ID [--config PATH] [--config-dir DIR]
  acbctl plugin disable ID [--config PATH] [--config-dir DIR]
  acbctl plugin check [--config PATH] [--config-dir DIR]
  acbctl plugin config ID [--set KEY=VALUE]... [--config PATH] [--config-dir DIR]
  acbctl plugin records ID RESOURCE [--config PATH] [--config-dir DIR]
  acbctl plugin export [--config PATH] [--config-dir DIR]
  acbctl plugin import [--file PATH] [--config PATH] [--config-dir DIR]"
    );
}
