//! `acbctl component list|status|reset`：契约 v1 的入口。
//!
//! 状态从 `state/components/<name>.json` 读取（由 launcher 的进程管理内核落盘）。
//! `phase` 是真实阶段，`pid` 只在组件存活时出现。
//! `reset` 只清状态文件里的失败计数与 `failed` 终态：不 spawn、不停进程、不改配置。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use acb::profile;
use acb::supervisor_owner;
use acb_supervisor::component::{
    ComponentError, ComponentTable, ComponentType, LogTarget, ReadinessKind, ResolvedComponent,
};
use acb_supervisor::component_state::{self, ComponentState};
use acb_supervisor::{Config, load_supervisor_config};

/// 退出码：0 成功；2 用法错误 / 未知组件；3 配置或组件表校验失败（契约 §5.7）。
const EXIT_USAGE: u8 = 2;
const EXIT_CONFIG: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    message: String,
    exit_code: u8,
}

impl Failure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_USAGE,
        }
    }

    fn unknown_component(name: &str) -> Self {
        Self {
            message: format!("unknown component: {name}"),
            exit_code: EXIT_USAGE,
        }
    }

    fn config(error: ComponentError) -> Self {
        Self {
            message: error.to_string(),
            exit_code: EXIT_CONFIG,
        }
    }

    pub fn exit_code(&self) -> u8 {
        self.exit_code
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub fn command(args: &[String]) -> Result<ExitCode, Failure> {
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    match args.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        Some("list") => run_list(&args[1..]),
        Some("status") => run_status(&args[1..]),
        Some("reset") => run_reset(&args[1..]),
        Some(other) => Err(Failure::usage(format!(
            "unknown component subcommand: {other}"
        ))),
        None => {
            print_help();
            Err(Failure::usage("missing component subcommand"))
        }
    }
}

fn run_list(args: &[String]) -> Result<ExitCode, Failure> {
    let (config, _rest, explicit) = parse_arguments(args, false)?;
    if !explicit && !config.exists() {
        println!(
            "components: 0 declared (no config at {})",
            config.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let (_config, table) = load(config)?;
    if table.is_empty() {
        println!("components: 0 declared");
        return Ok(ExitCode::SUCCESS);
    }

    let enabled = table.components.iter().filter(|entry| entry.enabled).count();
    println!(
        "components: {} declared ({enabled} enabled)",
        table.components.len()
    );
    println!(
        "{:<16} {:<8} {:<10} {:<9} {:<24} {}",
        "name", "type", "source", "enabled", "readiness", "origin"
    );
    for entry in &table.components {
        println!(
            "{:<16} {:<8} {:<10} {:<9} {:<24} {}",
            entry.name,
            entry.kind.to_string(),
            entry.source.to_string(),
            entry.enabled,
            readiness_label(entry),
            entry.preset.clone().unwrap_or_else(|| "command".to_owned())
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn run_status(args: &[String]) -> Result<ExitCode, Failure> {
    let (config_path, rest, explicit) = parse_arguments(args, true)?;
    if !explicit && !config_path.exists() {
        println!(
            "components: 0 declared (no config at {})",
            config_path.display()
        );
        return Ok(ExitCode::SUCCESS);
    }
    let (config, table) = load(config_path)?;
    let state_dir = state_dir(&config);
    let name = match rest.as_slice() {
        [] => None,
        [name] => Some(name.as_str()),
        _ => {
            return Err(Failure::usage(
                "component status accepts at most one component name",
            ))
        }
    };

    match name {
        Some(name) => {
            let entries: Vec<&ResolvedComponent> = table.get(name).collect();
            if entries.is_empty() {
                return Err(Failure::unknown_component(name));
            }
            for (position, entry) in entries.iter().enumerate() {
                if position > 0 {
                    println!();
                }
                print_details(entry, &state_dir);
            }
        }
        None => {
            if table.is_empty() {
                println!("components: 0 declared");
                return Ok(ExitCode::SUCCESS);
            }
            for entry in &table.components {
                println!(
                    "{}: declared enabled={} type={} source={} {}",
                    entry.name,
                    entry.enabled,
                    entry.kind,
                    entry.source,
                    state_summary(&state_dir, &entry.name)
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// 清掉失败计数与 `failed` 终态，让组件回到可启动的 `stopped`。
///
/// 只改 `state/components/<name>.json`：不 spawn、不停进程、不改配置。
/// 运行态（有 pid 或处于 starting/healthy/unhealthy/restarting）一律拒绝，
/// 否则等于把一个正在跑的组件改成 stopped。
fn run_reset(args: &[String]) -> Result<ExitCode, Failure> {
    let (config_path, rest, _explicit) = parse_arguments(args, true)?;
    let (config, table) = load(config_path)?;
    let name = match rest.as_slice() {
        [name] => name.as_str(),
        [] => return Err(Failure::usage("component reset requires a component name")),
        _ => {
            return Err(Failure::usage(
                "component reset accepts exactly one component name",
            ))
        }
    };
    if table.get(name).next().is_none() {
        return Err(Failure::unknown_component(name));
    }

    let state_dir = state_dir(&config);
    let path = ComponentState::path(&state_dir, name);
    let Some(mut state) = ComponentState::load(&state_dir, name).map_err(state_failure)? else {
        println!("reset: {name}");
        println!("  before: no state file (never started)");
        println!("  cleared: nothing (no counters to reset)");
        println!("  after: not-started");
        println!("  state file: {}", path.display());
        return Ok(ExitCode::SUCCESS);
    };

    if state.pid.is_some() || is_running_phase(&state.phase) {
        return Err(Failure::usage(format!(
            "refuse to reset {name}: component is running (phase={} pid={}); \
             stop it first — reset only rewrites the state file and must not \
             turn a live component into stopped",
            state.phase,
            state
                .pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned())
        )));
    }

    println!("reset: {name}");
    println!(
        "  before: phase={} attempts={} consecutive_failures={} restarts={} \
         failed_at_ms={} last_error={}",
        state.phase,
        state.attempts,
        state.consecutive_failures,
        state.restarts,
        optional(&state.failed_at_ms),
        state.last_error.as_deref().unwrap_or("-")
    );

    let before = state.clone();
    state.phase = "stopped".to_string();
    state.pid = None;
    state.attempts = 0;
    state.consecutive_failures = 0;
    state.restarts = 0;
    state.failed_at_ms = None;
    state.ready_at_ms = None;
    state.last_error = None;
    state.last_result = Some("reset".to_string());
    state.updated_at_ms = now_ms();
    component_state::save(&state_dir, &state).map_err(state_failure)?;

    println!(
        "  cleared: attempts {} -> 0; consecutive_failures {} -> 0; restarts {} -> 0; \
         failed_at_ms={} -> -; ready_at_ms={} -> -; last_error -> -",
        before.attempts,
        before.consecutive_failures,
        before.restarts,
        optional(&before.failed_at_ms),
        optional(&before.ready_at_ms)
    );
    println!(
        "  after: phase={} attempts={} consecutive_failures={} restarts={} last_result={}",
        state.phase,
        state.attempts,
        state.consecutive_failures,
        state.restarts,
        state.last_result.as_deref().unwrap_or("-")
    );
    println!("  state file: {}", path.display());
    println!(
        "  note: configuration untouched; a launcher that is already running keeps \
         its in-memory failed phase, so the reset takes effect when the component \
         manager is rebuilt (next launcher start) and the component is enabled"
    );
    Ok(ExitCode::SUCCESS)
}

fn is_running_phase(phase: &str) -> bool {
    matches!(phase, "starting" | "healthy" | "unhealthy" | "restarting")
}

fn optional(value: &Option<i64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn state_failure(error: String) -> Failure {
    Failure {
        message: error,
        exit_code: EXIT_CONFIG,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn print_details(entry: &ResolvedComponent, state_dir: &Path) {
    println!("component: {}", entry.name);
    println!("  declaration: {}", entry.entry_index);
    println!("  enabled: {}", entry.enabled);
    println!("  type: {}", entry.kind);
    match &entry.preset {
        Some(preset) => println!("  source: {} (preset {preset})", entry.source),
        None => println!("  source: {}", entry.source),
    }
    println!("  readiness: {}", readiness_details(entry));
    println!(
        "  restart: {} max_attempts={} backoff={:?} cap={}s",
        entry.restart.policy,
        entry.restart.max_attempts,
        entry.restart.backoff_secs,
        entry.restart.backoff_cap_secs
    );
    println!("  stop_timeout_secs: {}", entry.stop_timeout_secs);
    println!(
        "  depends: after={:?} requires={:?} wants={:?} part_of={:?}",
        entry.depends.after, entry.depends.requires, entry.depends.wants, entry.depends.part_of
    );
    println!("  log: {}", log_details(entry));
    if entry.kind == ComponentType::Oneshot {
        println!(
            "  oneshot: file={} compose_bin={:?} stop_action={}",
            entry.file.as_deref().unwrap_or(""),
            entry.compose_bin,
            entry.stop_action
        );
    }
    print_state(entry, state_dir);
}

fn print_state(entry: &ResolvedComponent, state_dir: &Path) {
    let path = ComponentState::path(state_dir, &entry.name);
    println!("  state file: {}", path.display());
    match ComponentState::load(state_dir, &entry.name) {
        Ok(Some(state)) => {
            println!(
                "  process: {} pid={}",
                state.phase,
                state
                    .pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "-".to_owned())
            );
            println!(
                "  state: enabled={} instance_id={} config_sha256={}",
                state.enabled,
                state.instance_id.as_deref().unwrap_or("-"),
                state.config_sha256
            );
            println!(
                "  counters: attempts={} consecutive_failures={} restarts={}",
                state.attempts, state.consecutive_failures, state.restarts
            );
            println!(
                "  last_run_at_ms={} last_result={} last_error={}",
                state
                    .last_run_at_ms
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                state.last_result.as_deref().unwrap_or("-"),
                state.last_error.as_deref().unwrap_or("-")
            );
            println!(
                "  probe_url={} updated_at_ms={}",
                state.probe_url.as_deref().unwrap_or("-"),
                state.updated_at_ms
            );
        }
        Ok(None) => println!("  process: not started (no state file)"),
        Err(error) => println!("  process: unknown (state unreadable: {error})"),
    }
}

fn state_summary(state_dir: &Path, name: &str) -> String {
    match ComponentState::load(state_dir, name) {
        Ok(Some(state)) => format!(
            "phase={} instance={} last_run_at_ms={} last_result={} failures={}",
            state.phase,
            state.instance_id.as_deref().unwrap_or("-"),
            state
                .last_run_at_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            state.last_result.as_deref().unwrap_or("-"),
            state.consecutive_failures
        ),
        Ok(None) => "phase=not-started (no state file)".to_owned(),
        Err(error) => format!("phase=unknown (state unreadable: {error})"),
    }
}

fn state_dir(config: &Config) -> PathBuf {
    std::env::var_os("ACB_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.state_dir))
}

fn readiness_label(entry: &ResolvedComponent) -> String {
    let readiness = &entry.readiness;
    match readiness.kind {
        ReadinessKind::Tcp => format!("tcp {}", readiness.endpoint()),
        ReadinessKind::Http => format!("http {}{}", readiness.endpoint(), readiness.path),
        ReadinessKind::Process => "process".to_owned(),
        // oneshot 专属：只看退出码，没有端点。
        ReadinessKind::Exit => "exit".to_owned(),
    }
}

fn readiness_details(entry: &ResolvedComponent) -> String {
    format!(
        "{} interval={}s failures={} success={} startup_grace={}s",
        readiness_label(entry),
        entry.readiness.interval_secs,
        entry.readiness.failures_threshold,
        entry.readiness.success_threshold,
        entry.readiness.startup_grace_secs
    )
}

fn log_details(entry: &ResolvedComponent) -> String {
    match entry.log.target {
        LogTarget::File => match &entry.log.path {
            Some(path) => format!("file path={path}"),
            None => "file path=default".to_owned(),
        },
        LogTarget::Journald => "journald".to_owned(),
    }
}

fn load(config: PathBuf) -> Result<(Config, ComponentTable), Failure> {
    load_supervisor_config(&config).map_err(Failure::config)
}

fn parse_arguments(
    args: &[String],
    allow_name: bool,
) -> Result<(PathBuf, Vec<String>, bool), Failure> {
    let mut config = None;
    let mut rest = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--config" => {
                index += 1;
                let value = args
                    .get(index)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Failure::usage("--config requires a value"))?;
                config = Some(PathBuf::from(value));
            }
            _ if arg.starts_with("--config=") => {
                let value = arg
                    .split_once('=')
                    .map(|(_, value)| value)
                    .unwrap_or_default();
                if value.is_empty() {
                    return Err(Failure::usage("--config requires a value"));
                }
                config = Some(PathBuf::from(value));
            }
            other if other.starts_with('-') => {
                return Err(Failure::usage(format!("unknown argument: {other}")));
            }
            other if allow_name => rest.push(other.to_owned()),
            other => {
                return Err(Failure::usage(format!("unknown argument: {other}")));
            }
        }
        index += 1;
    }

    if let Some(config) = config {
        return Ok((config, rest, true));
    }
    if let Some(config) = std::env::var("ACB_UNIT_CONFIG_FILE")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return Ok((config, rest, true));
    }
    // 迁移后 launcher 自持配置；默认读它，而不是要求调用方再传一次路径。
    Ok((
        supervisor_owner::owned_config_path(&profile::default_state_dir()),
        rest,
        false,
    ))
}

fn print_help() {
    println!(
        "Usage:
  acbctl component list [--config PATH]
  acbctl component status [NAME] [--config PATH]
  acbctl component reset NAME [--config PATH]

默认读取 <state>/supervisor/supervisor.toml（或 ACB_UNIT_CONFIG_FILE）。
状态来自 <state>/components/<name>.json；enabled 默认 false，未启用组件永远不会 spawn。
reset 只清失败计数与 failed 终态（不 spawn、不停进程、不改配置）；
未知组件退出 2，运行中的组件拒绝重置。"
    );
}
