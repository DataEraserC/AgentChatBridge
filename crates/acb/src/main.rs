//! ACB 运维 CLI。PoC 阶段只实现 `acbctl unit`。

mod unit;

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
  acbctl unit uninstall --root DIR"
    );
}
