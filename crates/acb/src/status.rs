//! `acbctl status --mode=run`：只读 `run.json` / `current.json` / `lkg.json` + HTTP 探针。
//! 不调用 systemctl；systemd 状态由 `unit doctor` 负责。

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use acb_supervisor::{HttpProbe, Probe, load_config, load_current};

use crate::fsutil::pid_alive;

#[derive(Debug, Default)]
struct Arguments {
    state_dir: Option<PathBuf>,
    config: Option<PathBuf>,
    health_url: Option<String>,
}

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    let arguments = parse_arguments(args)?;
    let config = match &arguments.config {
        Some(path) => Some(load_config(path)?),
        None => None,
    };
    let state_dir = match arguments.state_dir {
        Some(path) => path,
        None => match &config {
            Some(config) => PathBuf::from(&config.state_dir),
            None => default_state_dir(),
        },
    };
    let health_url = arguments
        .health_url
        .or_else(|| config.as_ref().and_then(|config| config.health_url.clone()))
        .or_else(|| {
            std::env::var("ACB_UNIT_CONSOLE_PORT")
                .ok()
                .map(|port| format!("http://127.0.0.1:{port}/healthz"))
        });

    let record = crate::run::read_run_record(&state_dir)?;
    let mut healthy = true;
    match record {
        None => {
            println!("run: not running (no run.json)");
            healthy = false;
        }
        Some(record) => {
            let parent_alive = pid_alive(record.parent_pid);
            let child_alive = pid_alive(record.child_pid);
            let state = if parent_alive && child_alive {
                "active"
            } else {
                "stale"
            };
            println!(
                "run: {state} mode={} parent_pid={} child_pid={} started_at={}",
                record.mode, record.parent_pid, record.child_pid, record.started_at
            );
            println!("  child: {} {}", record.child_exe, record.args.join(" "));
            println!("  config_sha: {}", record.config_sha);
            if let Some(selected) = &record.selected {
                println!("  selected: {}", selected.id());
            }
            if state == "stale" {
                healthy = false;
            }
        }
    }

    match load_current(&state_dir) {
        Some(command) => println!("current: {}", command.id()),
        None => println!("current: none"),
    }
    match load_lkg(&state_dir) {
        Some(command) => println!("lkg: {}", command.id()),
        None => println!("lkg: none"),
    }

    match health_url {
        Some(url) => {
            let mut probe = HttpProbe::new(&url);
            if probe.healthy() {
                println!("health: ok {url}");
            } else {
                println!("health: unreachable {url}");
                healthy = false;
            }
        }
        None => println!("health: skipped (no health_url)"),
    }

    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn load_lkg(state_dir: &std::path::Path) -> Option<acb_supervisor::CommandSpec> {
    let text = fs::read_to_string(state_dir.join("lkg.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("command")?.clone()).ok()
}

fn default_state_dir() -> PathBuf {
    std::env::var_os("ACB_UNIT_REPO_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|repo| repo.join("state").join("supervisor"))
        .unwrap_or_else(|| PathBuf::from("state/supervisor"))
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    let mut parsed = Arguments::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--mode" => {
                let value = next_value(args, &mut index, "--mode")?;
                require_run_mode(&value)?;
            }
            "--state-dir" => {
                parsed.state_dir =
                    Some(PathBuf::from(next_value(args, &mut index, "--state-dir")?));
            }
            "--config" => {
                parsed.config = Some(PathBuf::from(next_value(args, &mut index, "--config")?));
            }
            "--health-url" => {
                parsed.health_url = Some(next_value(args, &mut index, "--health-url")?);
            }
            _ if arg.starts_with("--mode=") => {
                require_run_mode(&non_empty_value(arg, "--mode")?)?;
            }
            _ if arg.starts_with("--state-dir=") => {
                parsed.state_dir = Some(PathBuf::from(non_empty_value(arg, "--state-dir")?));
            }
            _ if arg.starts_with("--config=") => {
                parsed.config = Some(PathBuf::from(non_empty_value(arg, "--config")?));
            }
            _ if arg.starts_with("--health-url=") => {
                parsed.health_url = Some(non_empty_value(arg, "--health-url")?);
            }
            other => return Err(format!("unknown argument for status: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

fn require_run_mode(value: &str) -> Result<(), String> {
    if value == "run" {
        Ok(())
    } else {
        Err(format!("status only supports --mode=run, got {value}"))
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

fn is_help(args: &[String]) -> bool {
    args.first()
        .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
}

fn print_help() {
    println!(
        "Usage: acbctl status --mode=run [--state-dir DIR] [--config supervisor.toml] [--health-url URL]"
    );
}
