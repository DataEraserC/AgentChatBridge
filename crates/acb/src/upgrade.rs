//! `acbctl upgrade`：把升级意图写成请求文件，不直接切版本。
//!
//! ACB 侧只做原子写 `state/supervisor/upgrade/requests/<id>.json`；认领、stage、
//! 停旧、提交和 75 退出都由 launcher 完成。没有外部重拉方时直接拒绝。

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_supervisor::upgrade::{
    DEFAULT_TTL_MS, Mailbox, SubmitOutcome, UpgradeRequest, UpgradeTarget,
};

pub fn command(args: &[String]) -> Result<ExitCode, String> {
    match args.first().map(String::as_str) {
        Some("-h" | "--help" | "help") => {
            print_help();
            Ok(ExitCode::SUCCESS)
        }
        Some("status") => status_command(&args[1..]),
        Some(other) if other.starts_with("--") => request_command(args),
        Some(other) => Err(format!("unknown upgrade subcommand: {other}")),
        None => {
            print_help();
            Err("missing upgrade arguments (use --generation ID or --payload PATH --strategy store|copy)".to_string())
        }
    }
}

/// 生成稳定的幂等键/请求号。时间 + pid + 纳秒，重复概率可忽略。
fn new_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let seed = format!("{}:{nanos}:{}", std::process::id(), crate::fsutil::now_ms());
    let hash = crate::fsutil::sha256_hex(seed.as_bytes());
    format!("req-{}-{}", crate::fsutil::now_ms(), &hash[..8])
}

struct RequestArgs {
    target: UpgradeTarget,
    request_id: Option<String>,
    idempotency_key: Option<String>,
    issue_id: Option<String>,
    reason: String,
    expected_current_id: Option<String>,
    ttl_secs: i64,
    state_dir: Option<PathBuf>,
}

fn parse_request_args(args: &[String]) -> Result<RequestArgs, String> {
    let mut generation = None;
    let mut payload = None;
    let mut strategy = None;
    let mut request_id = None;
    let mut idempotency_key = None;
    let mut issue_id = None;
    let mut reason = String::new();
    let mut expected_current_id = None;
    let mut ttl_secs = DEFAULT_TTL_MS / 1000;
    let mut state_dir = None;

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (arg.as_str(), None),
        };
        let value = |args: &[String], index: &mut usize| -> Result<String, String> {
            if let Some(value) = &inline {
                if value.is_empty() {
                    return Err(format!("{flag} requires a value"));
                }
                return Ok(value.clone());
            }
            *index += 1;
            args.get(*index)
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| format!("{flag} requires a value"))
        };
        match flag {
            "--generation" | "--target-generation" => {
                generation = Some(value(args, &mut index)?)
            }
            "--payload" => payload = Some(PathBuf::from(value(args, &mut index)?)),
            "--strategy" => strategy = Some(value(args, &mut index)?),
            "--request-id" => request_id = Some(value(args, &mut index)?),
            "--idempotency-key" => idempotency_key = Some(value(args, &mut index)?),
            "--issue" | "--issue-id" => issue_id = Some(value(args, &mut index)?),
            "--reason" => reason = value(args, &mut index)?,
            "--expected-current" => expected_current_id = Some(value(args, &mut index)?),
            "--ttl-secs" => {
                let raw = value(args, &mut index)?;
                ttl_secs = raw
                    .parse::<i64>()
                    .map_err(|_| format!("--ttl-secs must be a positive integer: {raw}"))?;
                if ttl_secs <= 0 {
                    return Err("--ttl-secs must be a positive integer".to_string());
                }
            }
            "--state-dir" => state_dir = Some(PathBuf::from(value(args, &mut index)?)),
            other => return Err(format!("unknown argument for upgrade: {other}")),
        }
        index += 1;
    }

    let target = match (generation, payload) {
        (Some(id), None) => {
            if strategy.is_some() {
                return Err("--strategy is only valid with --payload".to_string());
            }
            UpgradeTarget::Generation { id }
        }
        (None, Some(path)) => {
            let strategy = strategy.ok_or_else(|| {
                "--payload requires --strategy store|copy (no silent fallback)".to_string()
            })?;
            if !matches!(strategy.as_str(), "store" | "copy") {
                return Err(format!(
                    "unsupported strategy: {strategy} (supported: store, copy)"
                ));
            }
            UpgradeTarget::Payload {
                path: path.to_string_lossy().into_owned(),
                strategy,
            }
        }
        (Some(_), Some(_)) => {
            return Err("choose exactly one target: --generation ID or --payload PATH".to_string());
        }
        (None, None) => {
            return Err("missing target: use --generation ID or --payload PATH --strategy store|copy"
                .to_string());
        }
    };

    Ok(RequestArgs {
        target,
        request_id,
        idempotency_key,
        issue_id,
        reason,
        expected_current_id,
        ttl_secs,
        state_dir,
    })
}

fn request_command(args: &[String]) -> Result<ExitCode, String> {
    let parsed = parse_request_args(args)?;
    let state_dir = parsed.state_dir.clone().unwrap_or_else(upgrade_state_dir);
    let mailbox = Mailbox::new(&state_dir);

    let request_id = parsed.request_id.unwrap_or_else(new_request_id);
    let now = acb_supervisor::now_ms();
    let mut request = UpgradeRequest::new(request_id.clone(), parsed.target, now);
    request.expires_at_ms = now + parsed.ttl_secs.saturating_mul(1000);
    if let Some(key) = parsed.idempotency_key {
        request.idempotency_key = key;
    }
    request.issue_id = parsed.issue_id;
    request.reason = parsed.reason;
    request.expected_current_id = parsed.expected_current_id;
    request.requested_by.insert("actor".to_string(), "local-cli".to_string());
    request
        .requested_by
        .insert("source".to_string(), "acbctl".to_string());

    match mailbox.submit(request)? {
        SubmitOutcome::Queued { request_id, path } => {
            println!(
                "queued request_id={request_id} state_dir={} path={}",
                state_dir.display(),
                path.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        SubmitOutcome::Duplicate {
            request_id,
            status,
            path,
        } => {
            println!(
                "duplicate request_id={request_id} status={status} path={}",
                path.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        SubmitOutcome::Busy {
            active_request_id,
        } => {
            println!(
                "busy active_request_id={active_request_id} (not queued; retry after it finishes)"
            );
            Ok(ExitCode::from(1))
        }
        SubmitOutcome::Rejected {
            reason_code,
            reason,
        } => {
            println!("rejected reason_code={reason_code} reason={reason}");
            Ok(ExitCode::from(1))
        }
    }
}

fn status_command(args: &[String]) -> Result<ExitCode, String> {
    let mut request_id = None;
    let mut state_dir = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--state-dir" => {
                index += 1;
                state_dir = Some(PathBuf::from(
                    args.get(index)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| "--state-dir requires a value".to_string())?,
                ));
            }
            _ if arg.starts_with("--state-dir=") => {
                state_dir = Some(PathBuf::from(
                    arg.split_once('=')
                        .map(|(_, value)| value)
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| "--state-dir requires a value".to_string())?,
                ));
            }
            "-h" | "--help" | "help" => {
                print_help();
                return Ok(ExitCode::SUCCESS);
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown argument for upgrade status: {other}"));
            }
            other => request_id = Some(other.to_string()),
        }
        index += 1;
    }

    let state_dir = state_dir.unwrap_or_else(upgrade_state_dir);
    let mailbox = Mailbox::new(&state_dir);

    match request_id {
        Some(id) => match mailbox.read_result(&id) {
            Some(result) => {
                println!(
                    "request_id={} status={} reason_code={} reason={} target_generation={} at_ms={}",
                    result.request_id,
                    result.status,
                    result.reason_code,
                    empty_dash(&result.reason),
                    result.target_generation_id.as_deref().unwrap_or("-"),
                    result.at_ms
                );
                Ok(ExitCode::SUCCESS)
            }
            None => {
                println!("request_id={id} status=unknown (no result in history)");
                Ok(ExitCode::from(1))
            }
        },
        None => {
            println!("state_dir={}", state_dir.display());
            match mailbox.read_active() {
                Some(active) => println!(
                    "active request_id={} status={} target_generation={} updated_at_ms={}",
                    active.request_id,
                    active.status,
                    active.target_generation_id.as_deref().unwrap_or("-"),
                    active.updated_at_ms
                ),
                None => println!("active none"),
            }
            match mailbox.read_capabilities() {
                Some(capabilities) => println!(
                    "restart_owner={} launcher_pid={} profile_dir={}",
                    capabilities.restart_owner,
                    capabilities.launcher_pid,
                    empty_dash(&capabilities.profile_dir)
                ),
                None => println!("capabilities none (launcher not running?)"),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn upgrade_state_dir() -> PathBuf {
    crate::profile::default_state_dir()
}

fn empty_dash(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn print_help() {
    println!(
        "Usage:
  acbctl upgrade --generation ID [options]
  acbctl upgrade --payload PATH --strategy store|copy [options]
  acbctl upgrade status [REQUEST_ID] [--state-dir DIR]

Options:
  --request-id ID          复用固定请求号（默认自动生成）
  --idempotency-key KEY    同 key 的重复提交返回已有请求
  --issue ID               事件/结果绑定 issue；不写则只进系统流
  --reason TEXT            人读的原因
  --expected-current ID    乐观并发：当前 generation 不符则拒绝
  --ttl-secs N             请求有效期，默认 300 秒
  --state-dir DIR          默认 ACB_STATE_DIR 或 <profile 父目录>/state/supervisor"
    );
}
