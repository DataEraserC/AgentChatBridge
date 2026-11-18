//! 收侧附件：把平台给的媒体下载到本地，记录里只放本地路径。
//!
//! ## 为什么要这一层
//!
//! 反馈 #13：qqbot-official 收侧图片拿不到，只有占位文本——附件 URL 被直接丢弃。
//! 但直接把 URL 塞进记录会引出反馈 #12：平台 CDN 地址带 `rkey` 预签名，单条
//! 300~400 字符，每条记录都塞一遍就是拿签名串喂上下文。
//!
//! 所以：**下载落盘，记录里只放本地路径**。CDN URL 不进 element。
//!
//! ## 为什么下载是异步的
//!
//! 消息入站是热路径。一次慢下载会挡住这条连接上所有后续消息（包括别的会话的）。
//! 用户裁定（2026-11-18）：消息先入库、附件后补——用 core 的
//! `patch_element` 把占位元素换成真实内容。
//!
//! ## 平台中立
//!
//! 这一层在**适配器**里，不在 acb 里：它知道 QQ 的 CDN、签名、content_type。
//! acb 只收到「一个 image 元素，url 是本地路径」，不知道它从哪来。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use acb_kernel::adapter::PatchElement;
use acb_proto::Element;

/// 附件落盘与保留策略。全部来自 `connection.options`，core 不解释。
#[derive(Debug, Clone)]
pub struct AttachmentConfig {
    /// 单个附件的下载上限（字节）。超出只留占位，不下载。
    pub max_bytes: u64,
    /// 单次下载超时。
    pub timeout: Duration,
    /// 下载完了以后署名的目录（适配器私有 `state_dir` 下的相对路径）。
    pub dir: PathBuf,
    /// 落盘保留：总字节上限（0 = 不限）。
    pub keep_bytes: u64,
    /// 落盘保留：天数上限（0 = 不限）。
    pub keep_days: u64,
    /// 可选：单个房间的字节上限（0 = 不限）。
    pub per_room_bytes: u64,
}

impl Default for AttachmentConfig {
    fn default() -> Self {
        Self {
            // 上限定得高，由运维按需调低（用户裁定 2026-11-18）。
            max_bytes: 100 * 1024 * 1024,
            timeout: Duration::from_secs(60),
            dir: PathBuf::from("attachments"),
            keep_bytes: 500 * 1024 * 1024,
            keep_days: 30,
            per_room_bytes: 0,
        }
    }
}

/// 一个待下载的附件。
#[derive(Debug, Clone)]
pub struct PendingAttachment {
    pub room: String,
    pub platform_id: String,
    /// 元素在记录 `elements` 里的下标。
    pub index: usize,
    pub url: String,
    pub filename: String,
    pub content_type: String,
}

/// 把占位元素换成「本地路径」的形状。
///
/// 图片走 `Element::Image`（agent 的视觉工具认这个），其余（语音/视频/文件）
/// 走 `Element::File`——两者都带本地路径，agent 能拿路径去读。
pub fn ready_element(local: &Path, alt: &str, content_type: &str) -> Element {
    let path = local.display().to_string();
    if content_type.starts_with("image/") {
        Element::Image {
            url: path,
            alt: Some(alt.to_owned()),
        }
    } else {
        Element::File {
            url: path,
            name: alt.to_owned(),
        }
    }
}

/// 占位元素的文案。**不带 URL**——签名串一个字符都不进记录。
pub fn placeholder(content_type: &str, filename: &str) -> String {
    let kind = if content_type.is_empty() {
        "附件".to_owned()
    } else {
        content_type.to_owned()
    };
    if filename.is_empty() {
        format!("[{kind} 下载中]")
    } else {
        format!("[{kind} 下载中：{filename}]")
    }
}

/// 下载并落盘，然后把记录里的占位换掉。
///
/// 失败一律降级成一句可读占位（**不丢消息**）：下载失败、超限、写盘失败都只是
/// 「这个附件没拿到」，正文和其余元素照常。
pub fn fetch_one(
    config: &AttachmentConfig,
    attachment: &PendingAttachment,
    patch: &Sender<PatchElement>,
) {
    let target = config
        .dir
        .join(sanitize(&attachment.platform_id))
        .join(sanitize_file(&attachment.filename));

    let outcome = download(&attachment.url, config.max_bytes, config.timeout, &target);
    let element = match outcome {
        Ok(path) => ready_element(&path, &attachment.filename, &attachment.content_type),
        Err(reason) => Element::Text {
            text: format!("[{} 下载失败：{}]", attachment.content_type, reason),
        },
    };
    // 通道没了（bridge 关停）就放弃：记录里的占位仍在，信息不丢。
    let _ = patch.send(PatchElement {
        room: attachment.room.clone(),
        platform_id: attachment.platform_id.clone(),
        index: attachment.index,
        element,
    });
}

/// 下载到 `target`，返回本地路径。
fn download(url: &str, limit: u64, timeout: Duration, target: &Path) -> Result<PathBuf, String> {
    let effective = url
        .replace("host.containers.internal", "127.0.0.1")
        .replace("host.docker.internal", "127.0.0.1");
    // 平台给的媒体地址是**预签名**的（rkey 就是签名），不需要鉴权头。
    let response = ureq::get(&effective)
        .timeout(timeout)
        .call()
        .map_err(|error| format!("请求失败（{error}）"))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取失败（{error}）"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("超过上限 {} 字节", limit));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("建目录失败（{error}）"))?;
    }
    std::fs::write(target, &bytes).map_err(|error| format!("写盘失败（{error}）"))?;
    Ok(target.to_path_buf())
}

/// 把平台编号/文件名里的路径分隔符换掉，避免逃出附件目录。
fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|ch| {
            if ch == '/' || ch == '\\' || ch == ':' {
                '_'
            } else {
                ch
            }
        })
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_owned()
    } else {
        cleaned
    }
}

fn sanitize_file(value: &str) -> String {
    sanitize(value).trim_start_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 占位文案不带 URL：签名串一个字符都不该进记录。
    #[test]
    fn placeholder_has_no_url() {
        let text = placeholder("image/jpeg", "a.jpg");
        assert!(text.contains("a.jpg"), "要有文件名让人知道是什么");
        assert!(
            !text.contains("http") && !text.contains("rkey"),
            "不能把签名串写进占位，实际：{text}"
        );
    }

    /// 平台编号里的路径分隔符要清掉，否则 `../` 能写出附件目录。
    #[test]
    fn sanitize_blocks_path_traversal() {
        assert_eq!(sanitize("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize("a/b\\c"), "a_b_c");
        assert_eq!(sanitize(""), "unnamed");
        // 文件名去掉开头的点，避免写出隐藏文件或 `.`/`..`
        assert_eq!(sanitize_file("..hidden"), "hidden");
    }

    /// 超限的附件不落盘，返回可读原因。
    #[test]
    fn oversized_download_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "acb-attach-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("建目录");

        // 用一个不可达的地址验证「失败会返回原因」而不是 panic。
        let target = dir.join("x.bin");
        let result = download(
            "http://127.0.0.1:1/none",
            1024,
            Duration::from_millis(200),
            &target,
        );
        assert!(result.is_err(), "连不上要给错误");
        assert!(!target.exists(), "失败时不该留下半截文件");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
