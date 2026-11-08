//! `acbctl up|down|status|stack`：`acb-bridge.service` 的用户入口。
//!
//! 这一层只做两件事：调用 `systemctl --user start|stop acb-bridge.service`，
//! 以及查询单元与端口状态。组件启停仍由 launcher 的组件管理内核负责；本模块
//! 不直接 spawn 或 kill 组件进程。`down` 只停单元，然后验证 bridge 与端口确实
//! 收敛，不另起一条绕过 launcher 的停机路径。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

const UNIT: &str = "acb-bridge.service";
const ENDPOINT_ENV: &str = "ACB_ENDPOINT_LISTEN";
const CONSOLE_ENV: &str = "ACB_CONSOLE_LISTEN";
const CONFIG_ENV: &str = "ACB_UNIT_CONFIG_FILE";

/// 等 bridge 在 endpoint 上监听的时间；与旧 `up.sh` 的 30 秒一致。
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// `systemctl stop` 返回后，再给 launcher 收尾留一点观察窗口。
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_INTERVAL: Duration = Duration::from_millis(250);

/// `systemctl stop` 已返回但端口还在时，直接失败而不是替 launcher 杀进程。
const EXIT_NOT_CONVERGED: u8 = 1;

pub fn up(args: &[String]) -> Result<ExitCode, String> {
    parse_targets(args, "up")?;
    if let Err(error) = systemctl(&["start", UNIT]) {
        eprintln!("acbctl: {error}");
        return Ok(ExitCode::from(EXIT_NOT_CONVERGED));
    }

    match endpoint(&unit_environment().unwrap_or_default()) {
        Some(addr) => {
            if wait_port(addr, true, START_TIMEOUT) {
                println!("bridge 已就绪（{addr}）");
            } else {
                eprintln!("bridge 在 {:?} 内没有在 {addr} 上监听", START_TIMEOUT);
            }
        }
        None => println!("bridge 已启动（单元未提供 {ENDPOINT_ENV}，跳过端口等待）"),
    }
    status(&["--summary".to_string()])
}

pub fn down(args: &[String]) -> Result<ExitCode, String> {
    parse_targets(args, "down")?;
    if let Err(error) = systemctl(&["stop", UNIT]) {
        eprintln!("acbctl: {error}");
        return Ok(ExitCode::from(EXIT_NOT_CONVERGED));
    }

    match endpoint(&unit_environment().unwrap_or_default()) {
        Some(addr) => {
            if wait_port(addr, false, STOP_TIMEOUT) {
                println!("bridge 已停止（{addr} 已释放）");
                Ok(ExitCode::SUCCESS)
            } else {
                eprintln!(
                    "acbctl: {UNIT} 已停，但 bridge 仍在 {addr} 监听；launcher 的停机收敛没有把 bridge 带走。\
                     这里不强行 kill，避免绕开 launcher 的收尾路径。"
                );
                Ok(ExitCode::from(EXIT_NOT_CONVERGED))
            }
        }
        None => {
            println!("{UNIT} 已停止（单元未提供 {ENDPOINT_ENV}，无法校验端口）");
            Ok(ExitCode::SUCCESS)
        }
    }
}

pub fn stack(args: &[String]) -> Result<ExitCode, String> {
    let mut dry_run = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                print_stack_help();
                return Ok(ExitCode::SUCCESS);
            }
            other => return Err(format!("unknown argument for stack: {other}")),
        }
    }

    if dry_run {
        println!("[dry-run] systemctl --user start {UNIT}");
        println!("[dry-run] wait for {ENDPOINT_ENV} to accept TCP");
        println!("[dry-run] acbctl status --summary");
        return Ok(ExitCode::SUCCESS);
    }

    if let Err(error) = systemctl(&["start", UNIT]) {
        eprintln!("acbctl: {error}");
        return Ok(ExitCode::from(EXIT_NOT_CONVERGED));
    }

    let environment = unit_environment().unwrap_or_default();
    if let Some(addr) = endpoint(&environment) {
        if wait_port(addr, true, START_TIMEOUT) {
            println!("bridge 已就绪（{addr}）");
        } else {
            eprintln!("bridge 在 {:?} 内没有在 {addr} 上监听", START_TIMEOUT);
        }
    }
    println!("acb-bridge 已启动；组件由 acbl 管理，`acbctl down` 会停止该单元");
    status(&["--summary".to_string()])
}

pub fn status(args: &[String]) -> Result<ExitCode, String> {
    let mut summary = false;
    for arg in args {
        match arg.as_str() {
            "--summary" => summary = true,
            "-h" | "--help" => {
                print_status_help();
                return Ok(ExitCode::SUCCESS);
            }
            other => return Err(format!("unknown argument for status: {other}")),
        }
    }

    let (state, state_error) = match unit_state() {
        Ok(state) => (Some(state), None),
        Err(error) => (None, Some(error)),
    };
    let environment = unit_environment().unwrap_or_default();
    let endpoint = endpoint(&environment);
    let endpoint_open = endpoint.map(tcp_open);
    let console = environment
        .get(CONSOLE_ENV)
        .and_then(|value| parse_listen(value));
    let token = console_token(&environment);
    let console_code = console.map(|addr| {
        http_status(addr, "/api/status", token.as_deref())
            .map_err(|error| format!("{addr}: {error}"))
    });

    let unit_text = match (&state, &state_error) {
        (Some(state), _) => format!(
            "{} (sub={}{})",
            state.active,
            state.sub,
            state
                .main_pid
                .map(|pid| format!(", pid={pid}"))
                .unwrap_or_default()
        ),
        (None, Some(error)) => format!("unknown ({error})"),
        (None, None) => "unknown".to_string(),
    };
    let endpoint_text = match (endpoint, endpoint_open) {
        (Some(addr), Some(true)) => format!("{addr} open"),
        (Some(addr), Some(false)) => format!("{addr} closed"),
        _ => "(not configured)".to_string(),
    };
    let console_text = match (&console, &console_code) {
        (Some(addr), Some(Ok(code))) => format!("{addr} /api/status HTTP {code}"),
        (Some(addr), Some(Err(error))) => format!("{addr} /api/status unreachable ({error})"),
        _ => "(not configured)".to_string(),
    };

    if summary {
        let endpoint_state = match endpoint_open {
            Some(true) => "open",
            Some(false) => "closed",
            None => "unknown",
        };
        let console_state = match console_code {
            Some(Ok(code)) => code.to_string(),
            Some(Err(_)) => "unreachable".to_string(),
            None => "unknown".to_string(),
        };
        println!(
            "unit={} endpoint={endpoint_state} console={console_state}",
            state
                .as_ref()
                .map_or("unknown", |state| state.active.as_str())
        );
    } else {
        println!("unit: {unit_text}");
        println!("endpoint: {endpoint_text}");
        println!("console: {console_text}");
    }

    let healthy = state.as_ref().is_some_and(|state| state.active == "active")
        && endpoint_open.unwrap_or(true);
    Ok(if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_NOT_CONVERGED)
    })
}

struct UnitState {
    active: String,
    sub: String,
    main_pid: Option<u32>,
}

fn unit_state() -> Result<UnitState, String> {
    let text = systemctl_show(&["ActiveState", "SubState", "MainPID"])?;
    let mut active = String::from("unknown");
    let mut sub = String::from("unknown");
    let mut main_pid = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "ActiveState" => active = value.to_string(),
            "SubState" => sub = value.to_string(),
            "MainPID" => main_pid = value.parse().ok().filter(|pid| *pid != 0),
            _ => {}
        }
    }
    Ok(UnitState {
        active,
        sub,
        main_pid,
    })
}

fn unit_environment() -> Result<BTreeMap<String, String>, String> {
    let text = systemctl_show_value("Environment")?;
    let mut environment = BTreeMap::new();
    for item in text.split_whitespace() {
        if let Some((key, value)) = item.split_once('=') {
            environment.insert(key.to_string(), value.to_string());
        }
    }
    Ok(environment)
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| format!("cannot run systemctl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            stderr.trim()
        ))
    }
}

fn systemctl_show(properties: &[&str]) -> Result<String, String> {
    let mut command = Command::new("systemctl");
    command.args(["--user", "show", UNIT]);
    for property in properties {
        command.arg("-p").arg(property);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run systemctl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "systemctl --user show {UNIT} failed: {}",
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn systemctl_show_value(property: &str) -> Result<String, String> {
    let output = Command::new("systemctl")
        .args(["--user", "show", UNIT, "-p", property, "--value"])
        .output()
        .map_err(|error| format!("cannot run systemctl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "systemctl --user show {UNIT} failed: {}",
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn endpoint(environment: &BTreeMap<String, String>) -> Option<SocketAddr> {
    environment
        .get(ENDPOINT_ENV)
        .and_then(|value| parse_listen(value))
}

fn parse_listen(value: &str) -> Option<SocketAddr> {
    let (host, port) = value.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let host = host.trim_matches(['[', ']']);
    let ip = match host {
        "" | "0.0.0.0" | "::" => IpAddr::V4(Ipv4Addr::LOCALHOST),
        other => other.parse().ok()?,
    };
    Some(SocketAddr::new(ip, port))
}

fn tcp_open(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn wait_port(addr: SocketAddr, open: bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if tcp_open(addr) == open {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

fn console_token(environment: &BTreeMap<String, String>) -> Option<String> {
    let path = PathBuf::from(environment.get(CONFIG_ENV)?);
    let config_dir = default_config_dir(&path);
    let loaded = acb_kernel::Config::load(&path, Some(&config_dir)).ok()?;
    let token = loaded.config.console.as_ref()?.token.as_str();
    (!token.is_empty()).then(|| token.to_string())
}

fn default_config_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("config.d")
}

fn http_status(addr: SocketAddr, path: &str, token: Option<&str>) -> Result<u16, String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(800))
        .map_err(|error| format!("connect: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set read timeout: {error}"))?;

    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n",
        addr.ip(),
        addr.port()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write request: {error}"))?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| format!("read status line: {error}"))?;
    line.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("malformed status line: {}", line.trim()))
}

fn parse_targets(args: &[String], subcommand: &str) -> Result<(), String> {
    for target in args {
        match target.as_str() {
            "all" | "bridge" => {}
            "drivers" | "dsh" | "opencode" => {
                return Err(format!(
                    "「{target}」由 acbl 组件管理，不再由 `acbctl {subcommand}` 处理；\
                     请用 `acbctl component list|status` 查看组件"
                ));
            }
            other => {
                return Err(format!("不认识的组件：{other}（可用：all / bridge）"));
            }
        }
    }
    Ok(())
}

fn print_status_help() {
    println!(
        "Usage:
  acbctl status [--summary]

Report the acb-bridge unit, its listening endpoint, and the console probe.
The run-level status is still available as `acbctl status --mode=run`."
    );
}

fn print_stack_help() {
    println!(
        "Usage:
  acbctl stack [--dry-run]

Start acb-bridge.service, wait for its endpoint, then print a summary."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn parse_listen_keeps_explicit_host() {
        assert_eq!(
            parse_listen("127.0.0.1:7700"),
            Some(SocketAddr::from(([127, 0, 0, 1], 7700)))
        );
    }

    #[test]
    fn parse_listen_normalizes_wildcards() {
        assert_eq!(
            parse_listen("0.0.0.0:3100"),
            Some(SocketAddr::from(([127, 0, 0, 1], 3100)))
        );
        assert_eq!(
            parse_listen("[::]:3100"),
            Some(SocketAddr::from(([127, 0, 0, 1], 3100)))
        );
    }

    #[test]
    fn parse_listen_rejects_garbage() {
        assert_eq!(parse_listen("127.0.0.1"), None);
        assert_eq!(parse_listen("127.0.0.1:not-a-port"), None);
    }

    #[test]
    fn parse_targets_accepts_all_and_bridge() {
        assert!(parse_targets(&[], "up").is_ok());
        assert!(parse_targets(&["all".to_string()], "up").is_ok());
        assert!(parse_targets(&["bridge".to_string()], "up").is_ok());
    }

    #[test]
    fn parse_targets_rejects_component_names() {
        let error = parse_targets(&["drivers".to_string()], "up").unwrap_err();
        assert!(error.contains("acbl 组件"));
        assert!(error.contains("acbctl up"));
        let error = parse_targets(&["unknown".to_string()], "down").unwrap_err();
        assert!(error.contains("可用：all / bridge"));
    }

    #[test]
    fn wait_port_sees_listener_and_release() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        assert!(wait_port(addr, true, Duration::from_secs(1)));
        drop(listener);
        assert!(wait_port(addr, false, Duration::from_secs(1)));
    }

    #[test]
    fn http_status_reads_response_code() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .expect("write response");
        });
        assert_eq!(http_status(addr, "/api/status", Some("token")), Ok(204));
        handle.join().expect("join");
    }
}
