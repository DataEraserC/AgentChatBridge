//! `acbctl run`：没有 systemd 时的前台父循环。
//!
//! launcher 模式 spawn `acb-launcher`，由 launcher 读 `current.json` 并监督 bridge；
//! direct 模式直接 spawn bridge。父循环只负责 75 重拉、78 终止、限流和信号转发。

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use acb_supervisor::{CommandSpec, load_config, load_current};
use serde::{Deserialize, Serialize};

use crate::fsutil::{now_ms, pid_alive, send_sigterm, sha256_hex, write_atomic};

const RUN_JSON: &str = "run.json";
const RATE_LIMIT_COUNT: usize = 3;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(45);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

const SUPERVISOR_TEMPLATE: &str = include_str!("../../../deploy/run/supervisor.toml.tmpl");

static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Launcher,
    Direct,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Launcher => "launcher",
            Self::Direct => "direct",
        }
    }
}

struct Arguments {
    mode: Mode,
    config: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}

struct Prepared {
    mode: Mode,
    child_program: PathBuf,
    child_args: Vec<String>,
    state_dir: PathBuf,
    config_sha: String,
    selected: CommandSpec,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub(crate) parent_pid: u32,
    pub(crate) child_pid: u32,
    pub(crate) child_exe: String,
    pub(crate) args: Vec<String>,
    pub(crate) started_at: i64,
    pub(crate) config_sha: String,
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) selected: Option<CommandSpec>,
    pub(crate) state_dir: String,
}

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    if is_help(args) {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }

    let arguments = parse_arguments(args)?;
    let prepared = prepare(arguments)?;
    if let Some(pid) = existing_run_pid(&prepared.state_dir)? {
        return Err(format!(
            "another acbctl run is active (parent pid {pid}); remove {} if it is stale",
            run_json_path(&prepared.state_dir).display()
        ));
    }

    install_signal_handlers();
    let mut failures: VecDeque<Instant> = VecDeque::new();
    loop {
        let selected = if prepared.mode == Mode::Launcher {
            load_current(&prepared.state_dir).unwrap_or_else(|| prepared.selected.clone())
        } else {
            prepared.selected.clone()
        };

        let mut child = spawn_child(&prepared)?;
        if let Err(error) = write_run_record(&prepared, child.id(), &selected) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(run_json_path(&prepared.state_dir));
            return Err(error);
        }
        let outcome = match wait_child(&mut child) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(run_json_path(&prepared.state_dir));
                return Err(error);
            }
        };
        let _ = fs::remove_file(run_json_path(&prepared.state_dir));

        match outcome {
            Outcome::Shutdown => {
                eprintln!("acbctl run: stopped by SIGTERM/SIGINT");
                return Ok(ExitCode::SUCCESS);
            }
            Outcome::Code(0) => {
                eprintln!("acbctl run: child exited 0");
                return Ok(ExitCode::SUCCESS);
            }
            Outcome::Code(75) => {
                let now = Instant::now();
                failures.push_back(now);
                while let Some(front) = failures.front() {
                    if now.duration_since(*front) > RATE_LIMIT_WINDOW {
                        failures.pop_front();
                    } else {
                        break;
                    }
                }
                if failures.len() >= RATE_LIMIT_COUNT {
                    eprintln!(
                        "acbctl run: 已触发限流：{} 秒内 {} 次启动失败，停止重拉",
                        RATE_LIMIT_WINDOW.as_secs(),
                        RATE_LIMIT_COUNT
                    );
                    return Ok(ExitCode::from(78));
                }

                let selected = if prepared.mode == Mode::Launcher {
                    load_current(&prepared.state_dir).unwrap_or_else(|| prepared.selected.clone())
                } else {
                    prepared.selected.clone()
                };
                eprintln!(
                    "acbctl run: child requested rollback (75), re-read current.json -> {}",
                    selected.id()
                );
            }
            Outcome::Code(78) => {
                eprintln!("acbctl run: launcher gave up (78), see reason above");
                return Ok(ExitCode::from(78));
            }
            Outcome::Code(code) => {
                eprintln!("acbctl run: child exited {code}");
                return Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)));
            }
            Outcome::Signal(signal) => {
                eprintln!("acbctl run: child terminated by signal {signal}");
                return Ok(ExitCode::from(u8::try_from(128 + signal).unwrap_or(1)));
            }
        }
    }
}

fn parse_arguments(args: &[String]) -> Result<Arguments, String> {
    let mut mode = None;
    let mut config = None;
    let mut state_dir = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--mode" => {
                mode = Some(parse_mode(&next_value(args, &mut index, "--mode")?)?);
            }
            "--config" => {
                config = Some(PathBuf::from(next_value(args, &mut index, "--config")?));
            }
            "--state-dir" => {
                state_dir = Some(PathBuf::from(next_value(args, &mut index, "--state-dir")?));
            }
            _ if arg.starts_with("--mode=") => {
                mode = Some(parse_mode(&non_empty_value(arg, "--mode")?)?);
            }
            _ if arg.starts_with("--config=") => {
                config = Some(PathBuf::from(non_empty_value(arg, "--config")?));
            }
            _ if arg.starts_with("--state-dir=") => {
                state_dir = Some(PathBuf::from(non_empty_value(arg, "--state-dir")?));
            }
            other => return Err(format!("unknown argument for run: {other}")),
        }
        index += 1;
    }

    Ok(Arguments {
        mode: mode.ok_or_else(|| "run requires --mode=launcher|direct".to_string())?,
        config,
        state_dir,
    })
}

fn parse_mode(value: &str) -> Result<Mode, String> {
    match value {
        "launcher" => Ok(Mode::Launcher),
        "direct" => Ok(Mode::Direct),
        other => Err(format!("unknown run mode: {other}")),
    }
}

fn prepare(arguments: Arguments) -> Result<Prepared, String> {
    let bin_dir = env_path("ACB_UNIT_BIN_DIR")
        .ok_or_else(|| "missing required variable: ACB_UNIT_BIN_DIR".to_string())?;

    match arguments.mode {
        Mode::Launcher => {
            let state_dir = match &arguments.state_dir {
                Some(path) => path.clone(),
                None => default_state_dir()?,
            };
            let (config_path, config_text) = match arguments.config {
                Some(path) => {
                    let text = fs::read_to_string(&path)
                        .map_err(|error| format!("read {}: {error}", path.display()))?;
                    (path, text)
                }
                None => {
                    let text = render_supervisor_config(&state_dir)?;
                    let path = state_dir.join("supervisor.toml");
                    fs::create_dir_all(&state_dir)
                        .map_err(|error| format!("create {}: {error}", state_dir.display()))?;
                    write_atomic(&path, text.as_bytes())?;
                    (path, text)
                }
            };
            let config = load_config(&config_path)?;
            if let Some(override_dir) = &arguments.state_dir {
                if Path::new(&config.state_dir) != override_dir.as_path() {
                    return Err(format!(
                        "--state-dir {} does not match supervisor.state_dir {}",
                        override_dir.display(),
                        config.state_dir
                    ));
                }
            }
            let effective_state_dir = match arguments.state_dir {
                Some(path) => path,
                None => PathBuf::from(&config.state_dir),
            };
            let config_sha = sha256_hex(config_text.as_bytes());
            let launcher = bin_dir.join("acb-launcher");
            let child_args = vec![
                "--config".to_string(),
                config_path.to_string_lossy().into_owned(),
            ];
            Ok(Prepared {
                mode: Mode::Launcher,
                child_program: launcher,
                child_args,
                state_dir: effective_state_dir,
                config_sha,
                selected: CommandSpec {
                    program: config.program,
                    args: config.args,
                },
            })
        }
        Mode::Direct => {
            let config_path = arguments
                .config
                .or_else(|| env_path("ACB_UNIT_CONFIG_FILE"))
                .ok_or_else(|| {
                    "direct mode requires --config or ACB_UNIT_CONFIG_FILE".to_string()
                })?;
            let config_text = fs::read_to_string(&config_path)
                .map_err(|error| format!("read {}: {error}", config_path.display()))?;
            let state_dir = arguments.state_dir.unwrap_or(default_state_dir()?);
            let bridge = bin_dir.join("acb-bridge");
            let config_string = config_path.to_string_lossy().into_owned();
            let child_args = vec!["--config".to_string(), config_string.clone()];
            Ok(Prepared {
                mode: Mode::Direct,
                child_program: bridge.clone(),
                child_args,
                state_dir,
                config_sha: sha256_hex(config_text.as_bytes()),
                selected: CommandSpec {
                    program: bridge.to_string_lossy().into_owned(),
                    args: vec!["--config".to_string(), config_string],
                },
            })
        }
    }
}

fn render_supervisor_config(state_dir: &Path) -> Result<String, String> {
    let mut values = crate::unit::values_from_env()?;
    values.insert(
        "RUN_STATE_DIR".to_string(),
        state_dir.to_string_lossy().into_owned(),
    );
    crate::unit::substitute("supervisor.toml", SUPERVISOR_TEMPLATE, &values)
}

fn default_state_dir() -> Result<PathBuf, String> {
    if let Some(repo) = env_path("ACB_UNIT_REPO_DIR") {
        return Ok(repo.join("state").join("supervisor"));
    }
    Ok(PathBuf::from("state/supervisor"))
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn spawn_child(prepared: &Prepared) -> Result<Child, String> {
    let mut command = Command::new(&prepared.child_program);
    command
        .args(&prepared.child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .env_remove("NOTIFY_SOCKET")
        .env_remove("WATCHDOG_USEC");
    command.spawn().map_err(|error| {
        format!(
            "cannot start {} ({}): {error}",
            prepared.child_program.display(),
            prepared.mode.as_str()
        )
    })
}

fn write_run_record(
    prepared: &Prepared,
    child_pid: u32,
    selected: &CommandSpec,
) -> Result<(), String> {
    fs::create_dir_all(&prepared.state_dir)
        .map_err(|error| format!("create {}: {error}", prepared.state_dir.display()))?;
    let record = RunRecord {
        parent_pid: std::process::id(),
        child_pid,
        child_exe: prepared.child_program.to_string_lossy().into_owned(),
        args: prepared.child_args.clone(),
        started_at: now_ms(),
        config_sha: prepared.config_sha.clone(),
        mode: prepared.mode.as_str().to_string(),
        selected: Some(selected.clone()),
        state_dir: prepared.state_dir.to_string_lossy().into_owned(),
    };
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| format!("serialize {RUN_JSON}: {error}"))?
        + "\n";
    write_atomic(&run_json_path(&prepared.state_dir), text.as_bytes())
}

fn existing_run_pid(state_dir: &Path) -> Result<Option<u32>, String> {
    let path = run_json_path(state_dir);
    match fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
        Ok(text) => match serde_json::from_str::<RunRecord>(&text) {
            Ok(record) if pid_alive(record.parent_pid) => Ok(Some(record.parent_pid)),
            _ => {
                let _ = fs::remove_file(&path);
                Ok(None)
            }
        },
    }
}

pub(crate) fn read_run_record(state_dir: &Path) -> Result<Option<RunRecord>, String> {
    let path = run_json_path(state_dir);
    match fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("parse {}: {error}", path.display())),
    }
}

fn run_json_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RUN_JSON)
}

enum Outcome {
    Shutdown,
    Code(i32),
    Signal(i32),
}

fn wait_child(child: &mut Child) -> Result<Outcome, String> {
    loop {
        if shutdown_requested() {
            forward_shutdown(child)?;
            return Ok(Outcome::Shutdown);
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(outcome_of(status)),
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(error) => return Err(format!("wait for child: {error}")),
        }
    }
}

fn forward_shutdown(child: &mut Child) -> Result<(), String> {
    let pid = child.id();
    if let Err(error) = send_sigterm(pid) {
        eprintln!("acbctl run: {error}; killing child directly");
        let _ = child.kill();
        let _ = child.wait();
        return Ok(());
    }
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(());
            }
            Err(error) => return Err(format!("wait for child during shutdown: {error}")),
        }
    }
}

fn outcome_of(status: std::process::ExitStatus) -> Outcome {
    if let Some(code) = status.code() {
        return Outcome::Code(code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return Outcome::Signal(signal);
        }
    }
    Outcome::Code(1)
}

#[cfg(unix)]
fn install_signal_handlers() {
    extern "C" fn mark(_signal: libc::c_int) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }
    // SAFETY: handler 只写原子位，不执行任何非 async-signal-safe 操作。
    unsafe {
        libc::signal(libc::SIGTERM, mark as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, mark as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

fn is_help(args: &[String]) -> bool {
    args.first()
        .is_some_and(|arg| matches!(arg.as_str(), "-h" | "--help" | "help"))
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

fn print_help() {
    println!(
        "Usage:
  acbctl run --mode=launcher [--config supervisor.toml] [--state-dir DIR]
  acbctl run --mode=direct [--config config.toml] [--state-dir DIR]"
    );
}
