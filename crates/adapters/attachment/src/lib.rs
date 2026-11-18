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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    // 路径带房间：清理时要按房间算占用（`per_room_bytes`），目录结构本身就是索引。
    let target = config
        .dir
        .join(sanitize(&attachment.room))
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

/// 落盘里的一个文件。
#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    /// 所属房间目录名（`sanitize(room)`）。
    room: String,
    bytes: u64,
    /// 修改时间（Unix 秒）。取不到就按 0 处理——会被当成最老的先删。
    age_secs: u64,
}

/// 扫一遍落盘目录。目录不存在返回空（还没下过任何附件）。
fn scan(root: &Path) -> Vec<Entry> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = Vec::new();
    let Ok(rooms) = std::fs::read_dir(root) else {
        return out;
    };
    for room in rooms.flatten() {
        let room_name = room.file_name().to_string_lossy().into_owned();
        let Ok(records) = std::fs::read_dir(room.path()) else {
            continue;
        };
        for record in records.flatten() {
            let Ok(files) = std::fs::read_dir(record.path()) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(meta) = file.metadata() else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let age_secs = meta
                    .modified()
                    .ok()
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| now.saturating_sub(d.as_secs()))
                    .unwrap_or(u64::MAX);
                out.push(Entry {
                    path: file.path(),
                    room: room_name.clone(),
                    bytes: meta.len(),
                    age_secs,
                });
            }
        }
    }
    out
}

/// 一次清理的结果，给日志用。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed_files: u64,
    pub removed_bytes: u64,
    /// 清完之后还剩这些。
    pub kept_bytes: u64,
}

/// 按保留策略清一次落盘。
///
/// 三档**从旧到新**依次判：超龄 → 单房间超限 → 总量超限。同一批文件不重复处理。
///
/// 为什么从旧到新：清理的目的是「让最新的活下来」。先删最老的，剩下的自然满足
/// 上限——反过来先删总量，可能把某个房间刚存的重要附件删掉。
pub fn prune(config: &AttachmentConfig) -> PruneReport {
    let mut entries = scan(&config.dir);
    if entries.is_empty() {
        return PruneReport::default();
    }
    // 最老的排前面。
    entries.sort_by(|a, b| b.age_secs.cmp(&a.age_secs));

    let mut report = PruneReport::default();
    let mut removed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let mut drop_one = |entry: &Entry,
                        report: &mut PruneReport,
                        removed: &mut std::collections::HashSet<PathBuf>| {
        if std::fs::remove_file(&entry.path).is_ok() {
            report.removed_files += 1;
            report.removed_bytes += entry.bytes;
            removed.insert(entry.path.clone());
        }
    };

    // 1) 超龄。
    if config.keep_days > 0 {
        let max_age = config.keep_days.saturating_mul(86_400);
        for entry in entries.iter() {
            if entry.age_secs > max_age {
                drop_one(entry, &mut report, &mut removed);
            }
        }
    }

    // 2) 单房间超限：每个房间从老到新删到不超。
    if config.per_room_bytes > 0 {
        let mut rooms: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        for entry in entries.iter() {
            if !removed.contains(&entry.path) {
                *rooms.entry(entry.room.clone()).or_insert(0) += entry.bytes;
            }
        }
        for entry in entries.iter() {
            if removed.contains(&entry.path) {
                continue;
            }
            let used = rooms.get(&entry.room).copied().unwrap_or(0);
            if used <= config.per_room_bytes {
                continue;
            }
            drop_one(entry, &mut report, &mut removed);
            if let Some(total) = rooms.get_mut(&entry.room) {
                *total = total.saturating_sub(entry.bytes);
            }
        }
    }

    // 3) 总量超限。
    if config.keep_bytes > 0 {
        let mut total: u64 = entries
            .iter()
            .filter(|entry| !removed.contains(&entry.path))
            .map(|entry| entry.bytes)
            .sum();
        for entry in entries.iter() {
            if total <= config.keep_bytes {
                break;
            }
            if removed.contains(&entry.path) {
                continue;
            }
            drop_one(entry, &mut report, &mut removed);
            total = total.saturating_sub(entry.bytes);
        }
        report.kept_bytes = total;
    } else {
        report.kept_bytes = entries
            .iter()
            .filter(|entry| !removed.contains(&entry.path))
            .map(|entry| entry.bytes)
            .sum();
    }

    // 目录空了就收掉，别留一堆空壳。
    if config.keep_days > 0 || config.keep_bytes > 0 || config.per_room_bytes > 0 {
        remove_empty_dirs(&config.dir);
    }
    report
}

/// 删掉落盘里的空房间/空消息目录。只删空目录，任何还有文件的目录都不动。
fn remove_empty_dirs(root: &Path) {
    let Ok(rooms) = std::fs::read_dir(root) else {
        return;
    };
    for room in rooms.flatten() {
        let Ok(records) = std::fs::read_dir(room.path()) else {
            continue;
        };
        for record in records.flatten() {
            let path = record.path();
            if path.is_dir() {
                let empty = std::fs::read_dir(&path)
                    .map(|mut it| it.next().is_none())
                    .unwrap_or(false);
                if empty {
                    let _ = std::fs::remove_dir(&path);
                }
            }
        }
        let room_empty = std::fs::read_dir(room.path())
            .map(|mut it| it.next().is_none())
            .unwrap_or(false);
        if room_empty {
            let _ = std::fs::remove_dir(room.path());
        }
    }
}

/// 起一个落盘清理线程：启动后等 5 分钟、之后每小时跑一次。
///
/// 抽到共享 crate 是因为两个适配器都要用，而这段逻辑与平台无关。
///
/// **先等再跑**：进程刚起时正忙着连网关，不该抢 IO。睡眠分片，shutdown 时能及时
/// 退出而不是等满一个周期。
pub fn spawn_prune(
    config: AttachmentConfig,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    label: String,
) {
    if config.keep_bytes == 0 && config.keep_days == 0 && config.per_room_bytes == 0 {
        return;
    }
    std::thread::spawn(move || {
        use std::sync::atomic::Ordering;
        let mut waited = 0u64;
        while waited < PRUNE_FIRST_DELAY_SECS && !shutdown.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            waited += 1;
        }
        while !shutdown.load(Ordering::Relaxed) {
            let report = prune(&config);
            if report.removed_files > 0 {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.qqbot.attachment_pruned",
                        id = label,
                        files = report.removed_files,
                        bytes = report.removed_bytes,
                        kept = report.kept_bytes
                    )
                );
            }
            let mut slept = 0u64;
            while slept < PRUNE_INTERVAL_SECS && !shutdown.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(1));
                slept += 1;
            }
        }
    });
}

/// 清理线程第一次跑之前等多久（秒）。
const PRUNE_FIRST_DELAY_SECS: u64 = 300;
/// 清理周期（秒）。
const PRUNE_INTERVAL_SECS: u64 = 3600;

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
    /// 建一个临时落盘目录，写入指定大小/年龄的文件。
    fn fixture(label: &str, files: &[(&str, &str, u64)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "acb-prune-{}-{label}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        for (room, record, bytes) in files {
            let dir = root.join(room).join(record);
            std::fs::create_dir_all(&dir).expect("建目录");
            std::fs::write(dir.join("f.bin"), vec![0u8; *bytes as usize]).expect("写文件");
        }
        root
    }

    fn total_bytes(root: &Path) -> u64 {
        scan(root).iter().map(|entry| entry.bytes).sum()
    }

    /// 总量超限：删到不超为止，且**从最老的删**。
    #[test]
    fn prune_respects_total_bytes_cap() {
        let root = fixture(
            "total",
            &[
                ("room-a", "m1", 100),
                ("room-a", "m2", 100),
                ("room-b", "m3", 100),
            ],
        );
        let config = AttachmentConfig {
            dir: root.clone(),
            keep_bytes: 150,
            keep_days: 0,
            per_room_bytes: 0,
            ..AttachmentConfig::default()
        };
        let report = prune(&config);
        assert!(report.removed_files > 0, "超限要真的删东西");
        assert!(
            total_bytes(&root) <= 150,
            "清完不超过上限，实际 {}",
            total_bytes(&root)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 单房间超限：只清那个房间，别的房间不动。
    #[test]
    fn prune_respects_per_room_cap_without_touching_others() {
        let root = fixture(
            "room",
            &[
                ("room-big", "m1", 200),
                ("room-big", "m2", 200),
                ("room-small", "m3", 50),
            ],
        );
        let config = AttachmentConfig {
            dir: root.clone(),
            keep_bytes: 0,
            keep_days: 0,
            per_room_bytes: 100,
            ..AttachmentConfig::default()
        };
        let report = prune(&config);
        assert!(report.removed_files > 0);
        let entries = scan(&root);
        let big: u64 = entries
            .iter()
            .filter(|e| e.room == "room-big")
            .map(|e| e.bytes)
            .sum();
        let small: u64 = entries
            .iter()
            .filter(|e| e.room == "room-small")
            .map(|e| e.bytes)
            .sum();
        assert!(big <= 100, "超限的房间要清到不超，实际 {big}");
        assert_eq!(small, 50, "别的房间一个字节都不该动，实际 {small}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 三项都关 = 完全不清理（配置写 0 的语义是「不管这一维」）。
    #[test]
    fn prune_does_nothing_when_all_limits_are_zero() {
        let root = fixture("off", &[("r", "m", 500)]);
        let config = AttachmentConfig {
            dir: root.clone(),
            keep_bytes: 0,
            keep_days: 0,
            per_room_bytes: 0,
            ..AttachmentConfig::default()
        };
        let report = prune(&config);
        assert_eq!(report.removed_files, 0);
        assert_eq!(total_bytes(&root), 500, "一个都不该删");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 目录不存在时不 panic（还没下过任何附件）。
    #[test]
    fn prune_is_safe_when_directory_is_missing() {
        let config = AttachmentConfig {
            dir: PathBuf::from("/tmp/acb-no-such-dir-xyz"),
            keep_bytes: 1,
            ..AttachmentConfig::default()
        };
        let report = prune(&config);
        assert_eq!(report.removed_files, 0);
    }

    /// 清完之后空目录要收掉，不留空壳。
    #[test]
    fn prune_removes_empty_directories() {
        let root = fixture("empty", &[("r", "m", 10)]);
        let config = AttachmentConfig {
            dir: root.clone(),
            keep_bytes: 1,
            keep_days: 0,
            per_room_bytes: 0,
            ..AttachmentConfig::default()
        };
        prune(&config);
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .map(|it| it.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "不该留空目录，实际 {leftovers:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
