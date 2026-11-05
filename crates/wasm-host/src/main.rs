//! `acb-wasm-host`：独立进程里的 WASM 策略宿主。
//!
//! 由 `acb-plugin-process-provider` 启动；stdout 只走行 JSON 协议，日志走 stderr。
//! 加载失败退出 66；运行期 trap / 非法输出退出 70——两者都让 core 侧 provider
//! 立即 down，而不是带病继续。

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireError, WireResult, decode_line, encode_line,
};
use acb_wasm_host::WasmHost;

const EXIT_LOAD: u8 = 66;
const EXIT_TRAP: u8 = 70;
const DEFAULT_MAX_MEMORY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_FUEL: u64 = 10_000_000;

struct Args {
    module: PathBuf,
    provider: String,
    extension: String,
    max_memory_bytes: usize,
    fuel: u64,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut module = None;
        let mut provider = "demo.access_allowlist".to_owned();
        let mut extension = "access".to_owned();
        let mut max_memory_bytes = DEFAULT_MAX_MEMORY_BYTES;
        let mut fuel = DEFAULT_FUEL;
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--module" => {
                    module = Some(PathBuf::from(
                        args.next().ok_or("--module requires a path")?,
                    ));
                }
                "--provider" => {
                    provider = args.next().ok_or("--provider requires an id")?;
                }
                "--extension" => {
                    extension = args.next().ok_or("--extension requires a name")?;
                }
                "--max-memory-bytes" => {
                    max_memory_bytes = args
                        .next()
                        .ok_or("--max-memory-bytes requires a number")?
                        .parse()
                        .map_err(|error| format!("invalid --max-memory-bytes: {error}"))?;
                }
                "--fuel" => {
                    fuel = args
                        .next()
                        .ok_or("--fuel requires a number")?
                        .parse()
                        .map_err(|error| format!("invalid --fuel: {error}"))?;
                }
                "--help" | "-h" => {
                    println!(
                        "usage: acb-wasm-host --module <access.wasm|access.wat> \
                         [--provider demo.access_allowlist] [--extension access] \
                         [--max-memory-bytes 16777216] [--fuel 10000000]"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(Self {
            module: module.ok_or("--module is required")?,
            provider,
            extension,
            max_memory_bytes,
            fuel,
        })
    }
}

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("acb-wasm-host: {error}");
            return ExitCode::from(EXIT_LOAD);
        }
    };
    if args.extension != "access" {
        eprintln!("acb-wasm-host: only the access extension is implemented");
        return ExitCode::from(EXIT_LOAD);
    }

    let mut host = match WasmHost::load(&args.module, args.max_memory_bytes, args.fuel) {
        Ok(host) => host,
        Err(error) => {
            eprintln!("acb-wasm-host: load failed: {error}");
            return ExitCode::from(EXIT_LOAD);
        }
    };

    let mut stdout = std::io::stdout();
    let manifest = serde_json::json!({
        "type": "provider_manifest",
        "provider": args.provider,
        "extension": args.extension,
        "api_version": 1,
        "metadata": {
            "title": "WASM access provider",
            "summary": "独立进程 wasm 宿主（reference）：按房间返回 allow/deny",
            "tags": ["example", "wasm"],
            "effects": []
        }
    });
    if let Err(error) = writeln!(stdout, "{manifest}") {
        eprintln!("acb-wasm-host: cannot write manifest: {error}");
        return ExitCode::from(EXIT_LOAD);
    }
    if let Err(error) = stdout.flush() {
        eprintln!("acb-wasm-host: cannot flush manifest: {error}");
        return ExitCode::from(EXIT_LOAD);
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("acb-wasm-host: cannot read stdin: {error}");
                return ExitCode::from(EXIT_TRAP);
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: CoreMessage = match decode_line(&line) {
            Ok(message) => message,
            Err(error) => {
                eprintln!("acb-wasm-host: bad core message: {error}");
                continue;
            }
        };

        let reply = match message {
            CoreMessage::Decide {
                id, point, request, ..
            } if point == "access" => match host.decide(&request) {
                Ok(decision) => AdapterMessage::Reply {
                    id,
                    result: WireResult::Decision {
                        decision: if decision.allow {
                            "allow".to_owned()
                        } else {
                            "deny".to_owned()
                        },
                        reason: decision.reason,
                        delay_ms: 0,
                    },
                },
                Err(error) => {
                    eprintln!("acb-wasm-host: wasm trap: {error}");
                    return ExitCode::from(EXIT_TRAP);
                }
            },
            CoreMessage::Decide { id, point, .. } => AdapterMessage::Reply {
                id,
                result: WireResult::Error {
                    error: WireError::NotImplemented {
                        detail: format!("unsupported decide point `{point}`"),
                    },
                },
            },
            CoreMessage::Describe { id }
            | CoreMessage::Send { id, .. }
            | CoreMessage::Call { id, .. } => AdapterMessage::Reply {
                id,
                result: WireResult::Error {
                    error: WireError::NotImplemented {
                        detail: "adapter operations are not implemented".to_owned(),
                    },
                },
            },
            CoreMessage::Shutdown => break,
        };

        let line = match encode_line(&reply) {
            Ok(line) => line,
            Err(error) => {
                eprintln!("acb-wasm-host: cannot encode reply: {error}");
                return ExitCode::from(EXIT_TRAP);
            }
        };
        if let Err(error) = writeln!(stdout, "{line}").and_then(|_| stdout.flush()) {
            eprintln!("acb-wasm-host: cannot write reply: {error}");
            return ExitCode::from(EXIT_TRAP);
        }
    }
    ExitCode::SUCCESS
}
