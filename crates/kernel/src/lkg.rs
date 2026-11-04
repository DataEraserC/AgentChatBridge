//! 插件 last-known-good（LKG）状态（ADR-0030 D3 的运行时兜底）。
//!
//! 这里只做 bridge 自己的状态，不依赖 Nix、不需要 root：
//! - 加载成功的可回滚插件先进入 `candidate`；
//! - 只有在无在途轮次的安全点、且自加载以来没有失败时，才提升为 `lkg`；
//! - 同一 provider 在窗口内连续失败达到阈值，就回退到上一条 LKG。
//!
//! 状态文件默认放在存储目录旁（`state/plugins-lkg.toml`），由桥进程原子写入。
//! 文件只记录**可回滚**的插件产物；常驻插件不写，避免制造“能回滚”的假象。

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::LkgConfig;
use crate::plugin::ProviderId;

/// 失败窗口：10 分钟。
pub const FAILURE_WINDOW_MS: i64 = 10 * 60 * 1000;
/// 同一窗口内连续失败达到这个数就触发回滚。
pub const FAILURE_THRESHOLD: usize = 3;
/// 每个插件保留的 LKG 历史条数。
pub const KEEP_ENTRIES: usize = 3;

/// 插件当前的 LKG 状态。`RolledBack` 表示刚回滚过，等待下一次安全点重新提升。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LkgState {
    Candidate,
    Lkg,
    RolledBack,
}

impl LkgState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Lkg => "lkg",
            Self::RolledBack => "rolledBack",
        }
    }
}

/// 一条成功提升记录。`path` 是回滚时要恢复的产物引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LkgEntry {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub path: String,
    /// Unix 毫秒。
    pub promoted_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LkgFile {
    #[serde(default)]
    entries: Vec<LkgEntry>,
}

#[derive(Debug)]
struct Tracked {
    state: LkgState,
    /// 回滚原因，catalog 展示用。
    reason: Option<String>,
    /// 自本次加载以来的失败时间（Unix 毫秒）。
    failures: VecDeque<i64>,
    /// catalog 上一次观察到的健康状况；只在 Healthy→Unhealthy 转换时记一次失败。
    observed_unhealthy: bool,
    /// 当前候选产物。
    version: String,
    digest: String,
    path: String,
    /// 达到阈值、等待安全点执行的回滚请求。
    rollback_pending: Option<String>,
}

/// 一个插件在 catalog 里的 LKG 视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LkgInfo {
    pub state: LkgState,
    pub reason: Option<String>,
    pub version: Option<String>,
    pub digest: Option<String>,
    pub promoted_at: Option<i64>,
}

/// 桥持有的 LKG 状态机。`path = None` 表示禁用（测试/嵌入式不写文件）。
pub struct PluginLkg {
    path: Option<PathBuf>,
    entries: Vec<LkgEntry>,
    tracked: HashMap<ProviderId, Tracked>,
    max_failures: u64,
    window_ms: i64,
}

impl PluginLkg {
    /// 默认阈值（3 次 / 10 分钟）。
    pub fn new(path: PathBuf) -> Self {
        Self::new_with_config(path, LkgConfig::default())
    }

    /// 从文件恢复历史；文件不存在/损坏时降级为空历史。
    pub fn new_with_config(path: PathBuf, config: LkgConfig) -> Self {
        let entries = match fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<LkgFile>(&text) {
                Ok(mut file) => {
                    trim_all(&mut file.entries);
                    file.entries
                }
                Err(error) => {
                    eprintln!(
                        "plugin-lkg: cannot parse {}: {error}; starting with empty history",
                        path.display()
                    );
                    Vec::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                eprintln!(
                    "plugin-lkg: cannot read {}: {error}; starting with empty history",
                    path.display()
                );
                Vec::new()
            }
        };
        Self {
            path: Some(path),
            entries,
            tracked: HashMap::new(),
            max_failures: config.max_failures,
            window_ms: config.window_secs.saturating_mul(1000) as i64,
        }
    }

    /// 不写文件的 LKG（测试、嵌入式）。
    pub fn disabled() -> Self {
        let config = LkgConfig::default();
        Self {
            path: None,
            entries: Vec::new(),
            tracked: HashMap::new(),
            max_failures: config.max_failures,
            window_ms: config.window_secs.saturating_mul(1000) as i64,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.path.is_some()
    }

    /// 登记一个刚加载成功的候选。失败窗口、健康观察标记一并清空。
    pub fn register_candidate(
        &mut self,
        id: ProviderId,
        version: String,
        digest: String,
        path: String,
    ) {
        self.tracked.insert(
            id,
            Tracked {
                state: LkgState::Candidate,
                reason: None,
                failures: VecDeque::new(),
                observed_unhealthy: false,
                version,
                digest,
                path,
                rollback_pending: None,
            },
        );
    }

    /// 当前是否在跟踪这个 provider。
    pub fn is_tracked(&self, id: &ProviderId) -> bool {
        self.tracked.contains_key(id)
    }

    /// 已登记失败（自本次加载以来）。
    pub fn has_failures(&self, id: &ProviderId) -> bool {
        self.tracked
            .get(id)
            .map(|tracked| !tracked.failures.is_empty())
            .unwrap_or(false)
    }

    /// 当前状态。
    pub fn state(&self, id: &ProviderId) -> Option<LkgState> {
        self.tracked.get(id).map(|tracked| tracked.state)
    }

    /// 所有需要尝试提升的 provider（Candidate / RolledBack）。
    pub fn promotable(&self) -> Vec<ProviderId> {
        self.tracked
            .iter()
            .filter(|(_, tracked)| {
                matches!(tracked.state, LkgState::Candidate | LkgState::RolledBack)
                    && tracked.failures.is_empty()
                    && tracked.rollback_pending.is_none()
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 记录一次失败。返回窗口内累计次数；未跟踪/已回滚的 provider 返回 `None`。
    ///
    /// 达到阈值时把回滚请求挂起，等安全点执行；不会在这里直接改状态。
    pub fn note_failure(&mut self, id: &ProviderId, reason: &str, now: i64) -> Option<usize> {
        let tracked = self.tracked.get_mut(id)?;
        if tracked.state == LkgState::RolledBack {
            return None;
        }
        tracked.failures.push_back(now);
        while tracked
            .failures
            .front()
            .map(|at| now.saturating_sub(*at) > self.window_ms)
            .unwrap_or(false)
        {
            tracked.failures.pop_front();
        }
        let count = tracked.failures.len();
        if count as u64 >= self.max_failures && tracked.rollback_pending.is_none() {
            tracked.rollback_pending = Some(reason.to_owned());
        }
        Some(count)
    }

    /// catalog 健康状况转换：返回 `true` 表示这次从 Healthy 变成了 Unhealthy。
    pub fn observe_health(&mut self, id: &ProviderId, unhealthy: bool) -> bool {
        let Some(tracked) = self.tracked.get_mut(id) else {
            return false;
        };
        if unhealthy {
            if tracked.observed_unhealthy {
                return false;
            }
            tracked.observed_unhealthy = true;
            true
        } else {
            tracked.observed_unhealthy = false;
            false
        }
    }

    /// 取走一个达到阈值的回滚请求；同一时刻只执行一个。
    pub fn take_rollback_request(&mut self) -> Option<(ProviderId, String)> {
        for (id, tracked) in &mut self.tracked {
            if let Some(reason) = tracked.rollback_pending.take() {
                return Some((id.clone(), reason));
            }
        }
        None
    }

    /// 成功提升：追加记录、裁剪到最近 `KEEP_ENTRIES` 条、原子写文件。
    pub fn promote(&mut self, id: &ProviderId, now: i64) -> bool {
        let Some(tracked) = self.tracked.get(id) else {
            return false;
        };
        let entry = LkgEntry {
            id: id.to_string(),
            version: tracked.version.clone(),
            digest: tracked.digest.clone(),
            path: tracked.path.clone(),
            promoted_at: now,
        };
        self.entries.push(entry);
        trim_for(&mut self.entries, &id.to_string());
        if let Some(tracked) = self.tracked.get_mut(id) {
            tracked.state = LkgState::Lkg;
            tracked.reason = None;
            tracked.failures.clear();
            tracked.rollback_pending = None;
        }
        self.save();
        true
    }

    /// 找上一条 LKG：最新的、产物与当前候选不同的记录。
    ///
    /// 候选还没提升时，最新记录就是上一条 LKG；当前 LKG 自己失败时，跳过与当前
    /// 产物相同的记录，取再往前一条。
    pub fn previous_lkg(
        &self,
        id: &ProviderId,
        current_digest: &str,
        current_path: &str,
    ) -> Option<LkgEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.id == id.to_string())
            .find(|entry| entry.digest != current_digest || entry.path != current_path)
            .cloned()
    }

    /// 回滚成功：把当前候选换成上一条 LKG，状态显示为 `RolledBack`，等下次安全点提升。
    pub fn record_rollback(&mut self, id: &ProviderId, entry: &LkgEntry, reason: String) {
        if let Some(tracked) = self.tracked.get_mut(id) {
            tracked.state = LkgState::RolledBack;
            tracked.reason = Some(reason);
            tracked.failures.clear();
            tracked.observed_unhealthy = false;
            tracked.rollback_pending = None;
            tracked.version = entry.version.clone();
            tracked.digest = entry.digest.clone();
            tracked.path = entry.path.clone();
        }
    }

    /// 回滚失败（包括没有上一条 LKG）：保留原因，不再递归回滚。
    pub fn mark_rollback_failed(&mut self, id: &ProviderId, reason: String) {
        if let Some(tracked) = self.tracked.get_mut(id) {
            tracked.state = LkgState::RolledBack;
            tracked.reason = Some(reason);
            tracked.rollback_pending = None;
        }
    }

    /// 当前候选产物（回滚时与历史比较用）。
    pub fn current_artifact(&self, id: &ProviderId) -> Option<(String, String, String)> {
        self.tracked.get(id).map(|tracked| {
            (
                tracked.version.clone(),
                tracked.digest.clone(),
                tracked.path.clone(),
            )
        })
    }

    /// 最新一条已持久化的 LKG 记录（重启后没有 tracked 时也能查历史）。
    pub fn latest_lkg(&self, id: &ProviderId) -> Option<&LkgEntry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.id == id.to_string())
    }

    /// catalog 视图：状态 + 原因 + 最新一条历史记录。
    pub fn info(&self, id: &ProviderId) -> Option<LkgInfo> {
        let tracked = self.tracked.get(id)?;
        let latest = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.id == id.to_string());
        Some(LkgInfo {
            state: tracked.state,
            reason: tracked.reason.clone(),
            version: latest.map(|entry| entry.version.clone()),
            digest: latest.map(|entry| entry.digest.clone()),
            promoted_at: latest.map(|entry| entry.promoted_at),
        })
    }

    fn save(&self) {
        let Some(path) = &self.path else {
            return;
        };
        let file = LkgFile {
            entries: self.entries.clone(),
        };
        let text = match toml::to_string_pretty(&file) {
            Ok(text) => text,
            Err(error) => {
                eprintln!("plugin-lkg: cannot encode state: {error}");
                return;
            }
        };
        if let Some(parent) = path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                eprintln!("plugin-lkg: cannot create {}: {error}", parent.display());
                return;
            }
        }
        let mut temp = path.as_os_str().to_owned();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        if let Err(error) = fs::write(&temp, text) {
            eprintln!("plugin-lkg: cannot write {}: {error}", temp.display());
            return;
        }
        if let Err(error) = fs::rename(&temp, path) {
            eprintln!("plugin-lkg: cannot replace {}: {error}", path.display());
            let _ = fs::remove_file(&temp);
        }
    }
}

/// 按插入顺序保留每个 id 最新的 `KEEP_ENTRIES` 条。
fn trim_for(entries: &mut Vec<LkgEntry>, id: &str) {
    let count = entries.iter().filter(|entry| entry.id == id).count();
    if count <= KEEP_ENTRIES {
        return;
    }
    let drop_count = count - KEEP_ENTRIES;
    let mut dropped = 0;
    entries.retain(|entry| {
        if entry.id == id && dropped < drop_count {
            dropped += 1;
            false
        } else {
            true
        }
    });
}

fn trim_all(entries: &mut Vec<LkgEntry>) {
    let ids: Vec<String> = entries.iter().map(|entry| entry.id.clone()).collect();
    for id in ids {
        trim_for(entries, &id);
    }
}

/// 文件内容指纹：64 位 FNV-1a。**不是签名/防篡改**，只用于识别内容变化。
pub fn digest_file(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        for byte in &buffer[..read] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    Some(format!("fnv1a64:{hash:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("acb-lkg-{name}-{nanos}.toml"))
    }

    fn id() -> ProviderId {
        ProviderId::parse("demo.wake").expect("合法 id")
    }

    #[test]
    fn promotes_and_persists_latest_three() {
        let path = temp_path("promote");
        let mut lkg = PluginLkg::new(path.clone());
        lkg.register_candidate(
            id(),
            "0.1.0".into(),
            "fnv1a64:01".into(),
            "/tmp/a.so".into(),
        );
        assert!(lkg.promote(&id(), 1));
        for index in 2..=4 {
            lkg.register_candidate(
                id(),
                format!("0.{index}.0"),
                format!("fnv1a64:{index:02}"),
                format!("/tmp/{index}.so"),
            );
            assert!(lkg.promote(&id(), index));
        }
        let restored = PluginLkg::new(path.clone());
        let entries: Vec<_> = restored
            .entries
            .iter()
            .filter(|entry| entry.id == id().to_string())
            .collect();
        assert_eq!(entries.len(), KEEP_ENTRIES);
        assert_eq!(entries.first().unwrap().version, "0.2.0");
        assert_eq!(entries.last().unwrap().version, "0.4.0");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn failure_window_prunes_old_entries_and_requests_rollback() {
        let mut lkg = PluginLkg::new(temp_path("failures"));
        lkg.register_candidate(id(), "1".into(), "digest".into(), "/tmp/a.so".into());
        assert_eq!(lkg.note_failure(&id(), "boom", 0), Some(1));
        assert_eq!(lkg.note_failure(&id(), "boom", 1), Some(2));
        assert!(lkg.take_rollback_request().is_none());
        assert_eq!(
            lkg.note_failure(&id(), "boom", FAILURE_WINDOW_MS + 2),
            Some(1),
            "超过窗口的旧失败应被裁掉"
        );
        assert_eq!(
            lkg.note_failure(&id(), "boom", FAILURE_WINDOW_MS + 3),
            Some(2)
        );
        assert_eq!(
            lkg.note_failure(&id(), "boom", FAILURE_WINDOW_MS + 4),
            Some(3)
        );
        let (found, reason) = lkg.take_rollback_request().expect("应触发回滚");
        assert_eq!(found, id());
        assert_eq!(reason, "boom");
    }

    #[test]
    fn configurable_threshold_and_window() {
        let mut lkg = PluginLkg::new_with_config(
            temp_path("configurable"),
            LkgConfig {
                max_failures: 2,
                window_secs: 1,
            },
        );
        lkg.register_candidate(id(), "1".into(), "digest".into(), "/tmp/a.so".into());
        assert_eq!(lkg.note_failure(&id(), "boom", 0), Some(1));
        assert_eq!(lkg.note_failure(&id(), "boom", 1), Some(2));
        let (found, reason) = lkg.take_rollback_request().expect("2 次就应触发");
        assert_eq!(found, id());
        assert_eq!(reason, "boom");
        // 1 秒窗口：旧失败被裁掉，不会继续累加。
        assert_eq!(lkg.note_failure(&id(), "boom", 1002), Some(1));
    }

    #[test]
    fn previous_lkg_skips_current_artifact() {
        let mut lkg = PluginLkg::new(temp_path("previous"));
        lkg.register_candidate(id(), "1".into(), "d1".into(), "/tmp/a.so".into());
        lkg.promote(&id(), 1);
        lkg.register_candidate(id(), "2".into(), "d2".into(), "/tmp/b.so".into());
        // 候选还没提升：上一条 LKG 就是最新记录。
        let previous = lkg
            .previous_lkg(&id(), "d2", "/tmp/b.so")
            .expect("有上一条");
        assert_eq!(previous.digest, "d1");
        // 当前 LKG 自己失败：跳过自己，取再往前一条。
        lkg.promote(&id(), 2);
        let previous = lkg
            .previous_lkg(&id(), "d2", "/tmp/b.so")
            .expect("有上一条");
        assert_eq!(previous.digest, "d1");
    }

    #[test]
    fn broken_file_degrades_to_empty_history() {
        let path = temp_path("broken");
        fs::write(&path, "not toml").unwrap();
        let lkg = PluginLkg::new(path.clone());
        assert!(lkg.entries.is_empty());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn digest_is_stable_and_changes_with_content() {
        let path = temp_path("digest");
        fs::write(&path, "hello").unwrap();
        let first = digest_file(&path).expect("能算摘要");
        let second = digest_file(&path).expect("能算摘要");
        assert_eq!(first, second);
        fs::write(&path, "hello2").unwrap();
        assert_ne!(first, digest_file(&path).expect("能算摘要"));
        let _ = fs::remove_file(path);
    }
}
