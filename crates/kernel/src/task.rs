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
    /// 发到触发任务的那个房间。
    Send(Vec<Element>),
}

/// 按 handler 名字执行任务。未知 handler 明确报错，不静默成功。
pub(crate) fn run(handler: &str, params: &Value) -> Result<TaskOutcome, String> {
    match handler {
        "exec" => exec(params),
        other => Err(format!(
            "没有这个任务处理器：{other}（目前只支持 exec）"
        )),
    }
}

fn exec(params: &Value) -> Result<TaskOutcome, String> {
    let command = params
        .get("command")
        .and_then(Value::as_array)
        .ok_or_else(|| "exec 任务要 command 数组".to_owned())?;
    let mut parts = Vec::with_capacity(command.len());
    for item in command {
        let part = item
            .as_str()
            .ok_or_else(|| "exec 的 command 里每一项都要是字符串".to_owned())?;
        parts.push(part.to_owned());
    }
    if parts.is_empty() {
        return Err("exec 的 command 不能为空".to_owned());
    }

    let cwd = params.get("cwd").and_then(Value::as_str);
    if let Some(dir) = cwd {
        if params
            .get("create_dir")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("建任务目录 {dir} 失败：{error}"))?;
        }
    }

    let timeout_ms = params
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(600_000)
        .min(1_800_000);
    let send = params
        .get("send")
        .and_then(Value::as_str)
        .unwrap_or("none");
    if !matches!(send, "none" | "text" | "file" | "video") {
        return Err(format!(
            "exec 的 send 只支持 none / text / file / video，收到 {send:?}"
        ));
    }

    let mut command = Command::new(&parts[0]);
    command.args(&parts[1..]);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 {} 失败：{error}", parts[0]))?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("任务超过 {} 秒，已杀掉", timeout_ms / 1000));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(format!("等待任务失败：{error}")),
        }
    };
    if !status.success() {
        return Err(format!("任务退出码 {}（stderr 未捕获）", status));
    }

    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        pipe.read_to_string(&mut stdout)
            .map_err(|error| format!("读任务输出失败：{error}"))?;
    }
    let stdout = stdout.trim();

    match send {
        "none" => Ok(TaskOutcome::Nothing),
        "text" => {
            if stdout.is_empty() {
                return Err("任务没有输出，发不了 text".to_owned());
            }
            Ok(TaskOutcome::Send(vec![Element::text(stdout)]))
        }
        "file" | "video" => {
            let path = stdout
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .ok_or_else(|| "任务没有输出文件路径（最后一行 stdout 要是产物路径）".to_owned())?;
            if !Path::new(path).is_file() {
                return Err(format!("任务说产物是 {path}，但文件不存在"));
            }
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    Path::new(path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "file".to_owned());
            let element = if send == "video" {
                Element::Video {
                    url: path.to_owned(),
                }
            } else {
                Element::File {
                    url: path.to_owned(),
                    name,
                }
            };
            Ok(TaskOutcome::Send(vec![element]))
        }
        _ => unreachable!("send 已在上面校验"),
    }
}
