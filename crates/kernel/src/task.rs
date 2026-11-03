//! 路由插件能请求的后台任务。
//!
//! 目前只有一个 `exec`：跑一条命令，把最后一行 stdout 当作产物路径，再交给
//! 发送管线发出去。它是给「匹配到链接 → yt-dlp 下载 → 上传」这类慢活用的：
//! 路由回调必须快，真正的下载/转码在这里跑，不挡入站队列。

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use acb_proto::Element;
use serde_json::Value;

/// 任务跑完要做什么。
pub(crate) enum TaskOutcome {
    /// 没有要发的东西。
    Nothing,
    /// 发到触发任务的那个房间。外层是候选，按顺序试；只有平台返回
    /// `Unsupported`（编码阶段、还没发出去）才换下一个。
    Send(Vec<Vec<Element>>),
}

/// 按 handler 名字执行任务。未知 handler 明确报错，不静默成功。
pub(crate) fn run(handler: &str, params: &Value) -> Result<TaskOutcome, String> {
    match handler {
        "exec" => exec(params),
        other => Err(acb_i18n::t!("kernel.task.unknown_handler", handler = other)),
    }
}

fn exec(params: &Value) -> Result<TaskOutcome, String> {
    let command = params
        .get("command")
        .and_then(Value::as_array)
        .ok_or_else(|| acb_i18n::t!("kernel.task.command_array_required"))?;
    let mut parts = Vec::with_capacity(command.len());
    for item in command {
        let part = item
            .as_str()
            .ok_or_else(|| acb_i18n::t!("kernel.task.command_item_not_string"))?;
        parts.push(part.to_owned());
    }
    if parts.is_empty() {
        return Err(acb_i18n::t!("kernel.task.command_empty"));
    }

    let cwd = params.get("cwd").and_then(Value::as_str);
    if let Some(dir) = cwd {
        if params
            .get("create_dir")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            std::fs::create_dir_all(dir).map_err(|error| {
                acb_i18n::t!("kernel.task.create_dir_failed", dir = dir, error = error)
            })?;
        }
    }

    let timeout_ms = params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(600_000)
        .min(1_800_000);
    let send = send_kinds(params)?;
    if send.is_empty() {
        return Ok(TaskOutcome::Nothing);
    }

    let mut command = Command::new(&parts[0]);
    command.args(&parts[1..]);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().map_err(|error| {
        acb_i18n::t!(
            "kernel.task.start_failed",
            command = parts[0],
            error = error
        )
    })?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(acb_i18n::t!(
                        "kernel.task.timeout_killed",
                        seconds = timeout_ms / 1000
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(acb_i18n::t!("kernel.task.wait_failed", error = error)),
        }
    };
    if !status.success() {
        return Err(acb_i18n::t!("kernel.task.exit_code", code = status));
    }

    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .map_err(|error| acb_i18n::t!("kernel.task.read_output_failed", error = error))?;
    }
    let stdout = stdout.trim();

    let mut candidates = Vec::with_capacity(send.len());
    let mut file_path: Option<String> = None;
    for kind in &send {
        match kind.as_str() {
            "text" => {
                if stdout.is_empty() {
                    return Err(acb_i18n::t!("kernel.task.no_output_for_text"));
                }
                candidates.push(vec![Element::text(stdout)]);
            }
            "file" | "video" => {
                let path = match &file_path {
                    Some(path) => path.clone(),
                    None => {
                        let path = stdout
                            .lines()
                            .rev()
                            .map(str::trim)
                            .find(|line| !line.is_empty())
                            .ok_or_else(|| acb_i18n::t!("kernel.task.no_artifact_path"))?
                            .to_owned();
                        if !Path::new(&path).is_file() {
                            return Err(acb_i18n::t!("kernel.task.artifact_missing", path = path));
                        }
                        file_path = Some(path.clone());
                        path
                    }
                };
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        Path::new(&path)
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "file".to_owned());
                if kind == "video" {
                    candidates.push(vec![Element::Video { url: path }]);
                } else {
                    candidates.push(vec![Element::File { url: path, name }]);
                }
            }
            other => {
                return Err(acb_i18n::t!(
                    "kernel.task.unknown_send_kind",
                    kind = format!("{other:?}")
                ));
            }
        }
    }
    Ok(TaskOutcome::Send(candidates))
}

/// `send` 可以是一个字符串，也可以是按优先级排列的数组，例如
/// `send = { "video", "file" }`：先试视频，平台不支持再试文件。
fn send_kinds(params: &Value) -> Result<Vec<String>, String> {
    let mut kinds = Vec::new();
    match params.get("send") {
        None => return Ok(kinds),
        Some(Value::String(kind)) => kinds.push(kind.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                let kind = item
                    .as_str()
                    .ok_or_else(|| acb_i18n::t!("kernel.task.send_item_not_string"))?;
                kinds.push(kind.to_owned());
            }
        }
        Some(_) => {
            return Err(acb_i18n::t!("kernel.task.send_kind_invalid"));
        }
    }
    if kinds.iter().any(|kind| kind == "none") {
        if kinds.len() > 1 {
            return Err(acb_i18n::t!("kernel.task.none_cannot_mix"));
        }
        return Ok(Vec::new());
    }
    // 去重但保留顺序：video, file, video 第二个 video 没有意义。
    let mut unique = Vec::with_capacity(kinds.len());
    for kind in kinds {
        if !unique.contains(&kind) {
            unique.push(kind);
        }
    }
    Ok(unique)
}
