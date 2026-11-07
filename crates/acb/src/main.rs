//! ACB 运维 CLI：`acbctl unit` / `run` / `status`。

mod grants;
mod run;
mod secrets;
mod status;
mod unit;
mod upgrade;

// profile/fsutil 与库入口共用同一份实现，避免 bin 与 lib 各编译一份。
use acb::{fsutil, profile};

use std::process::ExitCode;

fn main() -> ExitCode {
    // `render --print` 常被管道消费；恢复默认 SIGPIPE，避免 head/grep 提前退出时 panic。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("acbctl: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        print_help();
        return Err("missing command".to_string());
    };

    match command.as_str() {
        "-h" | "--help" | "help" => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        "unit" => {
            let rest: Vec<String> = args.collect();
            match rest.first().map(String::as_str) {
                Some("render") => unit::render_command(&rest[1..]),
                Some("install") => unit::install_command(&rest[1..]),
                Some("uninstall") => unit::uninstall_command(&rest[1..]),
                Some("doctor") => unit::doctor_command(&rest[1..]),
                Some("-h" | "--help" | "help") => {
                    print_unit_help();
                    Ok(ExitCode::SUCCESS)
                }
                Some(other) => Err(format!("unknown unit subcommand: {other}")),
                None => {
                    print_unit_help();
                    Err("missing unit subcommand".to_string())
                }
            }
        }
        "run" => run::command(&args.collect::<Vec<_>>()),
        "status" => status::command(&args.collect::<Vec<_>>()),
        "profile" => profile::command(&args.collect::<Vec<_>>()),
        "grants" => grants::command(&args.collect::<Vec<_>>()),
        "secrets" => secrets::command(&args.collect::<Vec<_>>()),
        "upgrade" => upgrade::command(&args.collect::<Vec<_>>()),
        other => Err(format!("unknown command: {other}")),
    }
}

fn print_help() {
    println!(
        "acbctl {version}

Usage:
  acbctl unit render --print
  acbctl unit render --output-dir DIR
  acbctl unit render --diff --root DIR
  acbctl unit install --root DIR
  acbctl unit uninstall --root DIR
  acbctl unit doctor --root DIR
  acbctl run --mode=launcher|direct [--config PATH] [--state-dir DIR]
  acbctl status --mode=run [--state-dir DIR] [--config PATH] [--health-url URL]
  acbctl profile list|history|current|add|switch|rollback|gc|seed
  acbctl upgrade --generation ID | --payload PATH --strategy store|copy
  acbctl upgrade status [REQUEST_ID]
  acbctl grants explain|matrix|diff
  acbctl secrets check|resolve
  acbctl unit --help

Values are read only from whitelisted ACB_UNIT_* environment variables.",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn print_unit_help() {
    println!(
        "Usage:
  acbctl unit render --print
  acbctl unit render --output-dir DIR
  acbctl unit render --diff --root DIR
  acbctl unit install --root DIR
  acbctl unit uninstall --root DIR
  acbctl unit doctor --root DIR"
    );
}
