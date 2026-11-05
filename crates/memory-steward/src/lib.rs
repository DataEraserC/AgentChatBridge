//! 记忆管家：把「会话 worktree 的 git 同步」从对话轮次里拆出去。
//!
//! 边界：
//! - **不在 core 轮次路径里**：独立线程/服务按调度器触发，不阻塞 agent 的对话轮。
//! - **不做拓扑发现、CRDT、跨仓库引用**：远端就是一个 git remote。
//! - **冲突不静默丢数据**：默认停下并列出冲突文件；自动取一侧必须显式配置。
//!
//! 本地提交发生在 `state/worktrees/<memory>/<session>` 的会话分支上；管家对每个
//! 已存在的 worktree 做 `fetch → merge/rebase → push`，状态写到
//! `state_dir/status.json`。

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acb_proto::RoomId;
use serde::{Deserialize, Serialize};

pub const DEFAULT_INTERVAL_SECS: u64 = 300;
pub const DEFAULT_POLL_SECS: u64 = 30;
pub const DEFAULT_GIT_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_STATE_DIR: &str = "state/memory-steward";

/// 合并策略。`merge` 保留分叉历史；`rebase` 把本地提交重放到远端之上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    #[default]
    Merge,
    Rebase,
}

/// 冲突自动策略。默认 `none`：停下、列文件、通知会话，绝不静默取一侧。
///
/// `local` / `remote` **不是**内容级自动合并，而是按文件整体取一侧：另一侧在该文件
/// 里的改动会被丢弃。默认关闭就是因为这会丢数据；要开必须显式写进配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoResolve {
    /// 不自动解决，冲突即停。
    #[default]
    None,
    /// 冲突文件整体取本地一侧；远端在这些文件里的改动被丢弃。
    Local,
    /// 冲突文件整体取远端一侧；本地在这些文件里的改动被丢弃。
    Remote,
}

/// `[memory_steward]` 配置。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StewardConfig {
    pub enabled: bool,
    /// 时间触发间隔；0 = 关闭时间触发。
    pub interval_secs: u64,
    /// 本地新增提交达到多少次触发；0 = 关闭提交量触发。
    pub commit_threshold: u64,
    /// 后台轮询间隔。
    pub poll_secs: u64,
    pub merge_strategy: MergeStrategy,
    pub auto_resolve: AutoResolve,
    pub state_dir: String,
    /// 单次 git 命令超时；超时按失败记录，不阻塞轮次。
    pub git_timeout_secs: u64,
}

impl Default for StewardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: DEFAULT_INTERVAL_SECS,
            commit_threshold: 0,
            poll_secs: DEFAULT_POLL_SECS,
            merge_strategy: MergeStrategy::Merge,
            auto_resolve: AutoResolve::None,
            state_dir: DEFAULT_STATE_DIR.to_owned(),
            git_timeout_secs: DEFAULT_GIT_TIMEOUT_SECS,
        }
    }
}

impl StewardConfig {
    /// 从整份 config.toml 里读 `[memory_steward]`；没有这一段就用默认（关闭）。
    pub fn from_toml(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            memory_steward: Option<StewardConfig>,
        }
        let root: Root =
            toml::from_str(text).map_err(|error| format!("memory_steward config: {error}"))?;
        let config = root.memory_steward.unwrap_or_default();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.enabled && self.interval_secs == 0 && self.commit_threshold == 0 {
            return Err(
                "memory_steward: interval_secs 和 commit_threshold 至少要有一个非 0".to_owned(),
            );
        }
        if self.poll_secs == 0 {
            return Err("memory_steward: poll_secs must be > 0".to_owned());
        }
        if self.git_timeout_secs == 0 {
            return Err("memory_steward: git_timeout_secs must be > 0".to_owned());
        }
        Ok(())
    }

    pub fn scheduler(&self) -> Arc<dyn Scheduler> {
        Arc::new(CombinedSchedule {
            interval_secs: self.interval_secs,
            commit_threshold: self.commit_threshold,
        })
    }
}

/// 调度器接口：core 不硬编码周期，触发方式由实现决定。
pub trait Scheduler: Send + Sync {
    fn due(&self, state: &SyncState, now_ms: i64, commits_since_sync: u64) -> bool;
}

/// 时间 + 提交量组合：两者都配置时先到者触发。
#[derive(Debug, Clone)]
pub struct CombinedSchedule {
    pub interval_secs: u64,
    pub commit_threshold: u64,
}

impl Scheduler for CombinedSchedule {
    fn due(&self, state: &SyncState, now_ms: i64, commits_since_sync: u64) -> bool {
        let by_time = self.interval_secs > 0
            && state.last_attempt_ms.is_none_or(|last| {
                now_ms.saturating_sub(last) >= (self.interval_secs as i64).saturating_mul(1000)
            });
        let by_commits = self.commit_threshold > 0 && commits_since_sync >= self.commit_threshold;
        by_time || by_commits
    }
}

/// 一个「会话 × 记忆」的同步目标。
#[derive(Debug, Clone)]
pub struct Target {
    pub memory: String,
    pub session: String,
    /// 通知写回的房间（该会话的第一条订阅）；没有订阅时只记状态。
    pub room: Option<RoomId>,
    pub worktree: PathBuf,
}

impl Target {
    pub fn key(&self) -> String {
        format!("{}/{}", self.memory, self.session)
    }
}

/// 一次同步的持久状态。字段就是可观测面的最小集合。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    pub last_attempt_ms: Option<i64>,
    pub last_success_ms: Option<i64>,
    /// ok / pushed / merged / rebased / conflict / dirty / no_remote /
    /// remote_unreachable / concurrent_write / landing_failed / error。
    pub last_result: Option<String>,
    pub conflict_files: Vec<String>,
    pub error: Option<String>,
    /// 上次成功同步时的 HEAD；用于计算「之后又提交了几次」。
    pub last_head: Option<String>,
}

impl SyncState {
    pub fn conflict_count(&self) -> usize {
        self.conflict_files.len()
    }

    fn is_failure(&self) -> bool {
        matches!(
            self.last_result.as_deref(),
            Some("conflict")
                | Some("remote_unreachable")
                | Some("concurrent_write")
                | Some("landing_failed")
                | Some("error")
        )
    }
}

/// 通知钩子。桥把它实现成「写入房间的事件」，测试用记录器。
pub trait Notifier: Send + Sync {
    fn notify(&self, target: &Target, notice: &SyncNotice);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncNotice {
    Conflict { files: Vec<String> },
    Failure { reason: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StatusFile {
    version: u32,
    updated_at_ms: i64,
    targets: BTreeMap<String, SyncState>,
}

/// 一次 run_once 的可读结果，方便测试和日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOutcome {
    pub target: String,
    pub result: &'static str,
    pub conflict_files: Vec<String>,
    pub error: Option<String>,
}

pub struct Steward {
    config: StewardConfig,
    scheduler: Arc<dyn Scheduler>,
    targets: Vec<Target>,
    notifier: Arc<dyn Notifier>,
    status_path: PathBuf,
    state: StatusFile,
}

impl Steward {
    pub fn new(
        config: StewardConfig,
        targets: Vec<Target>,
        notifier: Arc<dyn Notifier>,
    ) -> Result<Self, String> {
        config.validate()?;
        let state_dir = PathBuf::from(&config.state_dir);
        std::fs::create_dir_all(&state_dir)
            .map_err(|error| format!("cannot create {}: {error}", state_dir.display()))?;
        let status_path = state_dir.join("status.json");
        let state = load_status(&status_path).unwrap_or(StatusFile {
            version: 1,
            updated_at_ms: 0,
            targets: BTreeMap::new(),
        });
        // 上次进程留下的演练 worktree 可能是半死的；先清掉再开始。
        cleanup_stale_merge_worktrees(&config, &targets);
        let scheduler = config.scheduler();
        Ok(Self {
            config,
            scheduler,
            targets,
            notifier,
            status_path,
            state,
        })
    }

    pub fn status_path(&self) -> &Path {
        &self.status_path
    }

    pub fn state(&self, target: &Target) -> SyncState {
        self.state
            .targets
            .get(&target.key())
            .cloned()
            .unwrap_or_default()
    }

    /// 一次调度检查 + 所有到期目标的同步。测试直接调用它。
    pub fn run_once(&mut self, now_ms: i64) -> Vec<SyncOutcome> {
        let mut outcomes = Vec::new();
        for target in &self.targets {
            if !target.worktree.exists() {
                continue;
            }
            let key = target.key();
            let mut old = self.state.targets.get(&key).cloned().unwrap_or_default();
            if old.last_head.is_none() {
                if let Some(head) = head_of(
                    &target.worktree,
                    Duration::from_secs(self.config.git_timeout_secs),
                ) {
                    old.last_head = Some(head);
                    // 基线要先落盘：否则下一次进程启动又会把「当前 HEAD」当作 0 次提交。
                    self.state.targets.insert(key.clone(), old.clone());
                }
            }
            let commits = commits_since(
                &target.worktree,
                old.last_head.as_deref(),
                self.config.git_timeout_secs,
            );
            if !self.scheduler.due(&old, now_ms, commits) {
                continue;
            }
            // 失败/冲突后按时间间隔重试，避免远端不可达时每个 poll 都打一次。
            let retry_ms = self
                .config
                .interval_secs
                .max(self.config.poll_secs)
                .saturating_mul(1000) as i64;
            if old.is_failure()
                && old
                    .last_attempt_ms
                    .is_some_and(|last| now_ms.saturating_sub(last) < retry_ms)
            {
                continue;
            }

            let (mut state, outcome) = sync_target(target, &self.config, now_ms);
            state.last_head = state.last_head.clone().or(old.last_head.clone());
            state.last_success_ms = state.last_success_ms.or(old.last_success_ms);
            self.state.targets.insert(key.clone(), state.clone());
            if let Some(notice) = &outcome.notice {
                self.notifier.notify(target, notice);
            }
            outcomes.push(SyncOutcome {
                target: key,
                result: outcome.result,
                conflict_files: state.conflict_files.clone(),
                error: state.error.clone(),
            });
        }
        self.state.updated_at_ms = now_ms;
        if let Err(error) = save_status(&self.status_path, &self.state) {
            eprintln!("memory-steward: cannot write status: {error}");
        }
        outcomes
    }

    /// 后台循环。`shutdown` 为真时退出；单次 git 操作有超时，不阻塞轮次。
    pub fn run_forever(mut self, shutdown: &AtomicBool) {
        let poll = Duration::from_secs(self.config.poll_secs);
        while !shutdown.load(Ordering::Relaxed) {
            self.run_once(now_ms());
            let mut slept = Duration::ZERO;
            while slept < poll && !shutdown.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                slept += Duration::from_millis(50);
            }
        }
    }
}

struct TargetOutcome {
    result: &'static str,
    notice: Option<SyncNotice>,
}

impl TargetOutcome {
    fn new(result: &'static str) -> Self {
        Self {
            result,
            notice: None,
        }
    }
}

/// 临时演练 worktree 的根目录。`state_dir` 是相对路径时按**桥的启动目录**解析，
/// 再传给 `git`（git 的 `current_dir` 是主 worktree，不能拿相对路径直接拼）。
fn merge_root(config: &StewardConfig) -> PathBuf {
    let state_dir = Path::new(&config.state_dir);
    let base = if state_dir.is_absolute() {
        state_dir.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(state_dir)
    };
    base.join("memory-merge")
}

/// 临时演练 worktree 的路径：`state_dir/memory-merge/<记忆>/<会话>`。
fn merge_worktree_path(config: &StewardConfig, target: &Target) -> PathBuf {
    merge_root(config)
        .join(&target.memory)
        .join(&target.session)
}

/// 演练用的 linked worktree。Drop 时无论成功失败都尝试 `git worktree remove`。
struct TempWorktree {
    main: PathBuf,
    path: PathBuf,
    timeout: Duration,
}

impl TempWorktree {
    fn create(main: &Path, path: &Path, head: &str, timeout: Duration) -> Result<Self, String> {
        if path.exists() {
            remove_merge_worktree(main, path, timeout);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let path_text = path.to_string_lossy().into_owned();
        let add = git(
            main,
            &["worktree", "add", "--detach", &path_text, head],
            timeout,
        );
        if !add.success {
            return Err(format!("git worktree add failed: {}", add.detail()));
        }
        Ok(Self {
            main: main.to_path_buf(),
            path: path.to_path_buf(),
            timeout,
        })
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        remove_merge_worktree(&self.main, &self.path, self.timeout);
    }
}

fn remove_merge_worktree(main: &Path, path: &Path, timeout: Duration) {
    let path_text = path.to_string_lossy().into_owned();
    let _ = git(
        main,
        &["worktree", "remove", "--force", &path_text],
        timeout,
    );
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = git(main, &["worktree", "prune"], timeout);
}

/// 启动时清理 `state_dir/memory-merge/*` 残留。配置里已经不存在的会话也会删目录，
/// 然后对现有目标跑一次 `git worktree prune`，避免 admin 元数据越积越多。
fn cleanup_stale_merge_worktrees(config: &StewardConfig, targets: &[Target]) {
    let timeout = Duration::from_secs(config.git_timeout_secs);
    for target in targets {
        let path = merge_worktree_path(config, target);
        if path.exists() {
            remove_merge_worktree(&target.worktree, &path, timeout);
        }
    }

    let root = merge_root(config);
    let known: std::collections::BTreeSet<PathBuf> = targets
        .iter()
        .map(|target| merge_worktree_path(config, target))
        .collect();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for memory_entry in entries.flatten() {
            let memory_path = memory_entry.path();
            if !memory_path.is_dir() {
                continue;
            }
            if let Ok(sessions) = std::fs::read_dir(&memory_path) {
                for session_entry in sessions.flatten() {
                    let path = session_entry.path();
                    if path.is_dir() && !known.contains(&path) {
                        let _ = std::fs::remove_dir_all(&path);
                    }
                }
            }
        }
    }

    for target in targets {
        let _ = git(&target.worktree, &["worktree", "prune"], timeout);
    }
}

/// 主工作区的指纹：HEAD + `git status` + 文件（相对路径、大小、mtime、内容哈希）。
///
/// 用于检测「演练期间有人直接改文件/提交」。mtime 单独不够（同一时间戳可被保留），
/// 所以再加上文件内容；大文件只算元数据，避免把一次同步拖成全盘扫描。
fn worktree_fingerprint(worktree: &Path, timeout: Duration) -> Result<String, String> {
    let head = head_of(worktree, timeout).ok_or_else(|| "cannot read HEAD".to_owned())?;
    let status = git(
        worktree,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
        timeout,
    );
    if !status.success {
        return Err(format!("git status failed: {}", status.detail()));
    }
    let mut files = Vec::new();
    collect_files(worktree, &mut files)?;
    files.sort();

    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    hash = fnv1a(head.as_bytes(), hash);
    hash = fnv1a(status.stdout.as_bytes(), hash);
    for file in files {
        let rel = file
            .strip_prefix(worktree)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        hash = fnv1a(rel.as_bytes(), hash);
        let Ok(meta) = std::fs::metadata(&file) else {
            continue;
        };
        hash = fnv1a(&meta.len().to_le_bytes(), hash);
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        hash = fnv1a(&mtime.to_le_bytes(), hash);
        if meta.len() <= 4 * 1024 * 1024 {
            if let Ok(content) = std::fs::read(&file) {
                hash = fnv1a(&content, hash);
            }
        }
    }
    Ok(format!("{hash:016x}"))
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir)
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read dir entry: {error}"))?;
        if entry.file_name() == ".git" {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_files(&path, out)?;
        } else if file_type.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// 把演练结果落到主工作区。失败时尝试 `reset --hard expected_head` 回滚，绝不把
/// 半落地的状态当成成功。
fn land_result(
    main: &Path,
    result_sha: &str,
    expected_head: &str,
    timeout: Duration,
) -> Result<(), String> {
    let reset = git(main, &["reset", "--hard", result_sha], timeout);
    // 只看「reset 是否成功 + HEAD 是否到位」。reset 之后再出现的外部写入属于
    // 落地之后的新改动，不能因为它反过来 reset 回旧 HEAD 把人家覆盖掉。
    let verified = reset.success && head_of(main, timeout).as_deref() == Some(result_sha);
    if verified {
        return Ok(());
    }

    let detail = if reset.success {
        "landing verification failed".to_owned()
    } else {
        format!("git reset --hard failed: {}", reset.detail())
    };
    let rollback = git(main, &["reset", "--hard", expected_head], timeout);
    if rollback.success {
        Err(format!("{detail}; rolled back to {expected_head}"))
    } else {
        Err(format!("{detail}; rollback failed: {}", rollback.detail()))
    }
}

/// 同步一个目标：主工作区只做「检查 + 收尾」，fetch/merge/rebase/冲突全在临时
/// worktree 里演练。演练期间主工作区保持干净不动，落地只持一小段写锁。
fn sync_target(target: &Target, config: &StewardConfig, now_ms: i64) -> (SyncState, TargetOutcome) {
    let mut state = SyncState {
        last_attempt_ms: Some(now_ms),
        ..SyncState::default()
    };
    let main = &target.worktree;
    let timeout = Duration::from_secs(config.git_timeout_secs);

    let status = git(main, &["status", "--porcelain"], timeout);
    if !status.success {
        state.error = Some(format!("git status failed: {}", status.detail()));
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }
    if !status.stdout.trim().is_empty() {
        state.error = Some("working tree has uncommitted changes; sync skipped".to_owned());
        state.last_result = Some("dirty".to_owned());
        return (state, TargetOutcome::new("dirty"));
    }

    let branch_out = git(main, &["rev-parse", "--abbrev-ref", "HEAD"], timeout);
    if !branch_out.success {
        state.error = Some(format!(
            "cannot determine current branch: {}",
            branch_out.detail()
        ));
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }
    let branch = branch_out.stdout.trim().to_owned();
    if branch.is_empty() || branch == "HEAD" {
        state.error = Some("cannot determine current branch".to_owned());
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }

    if !git(main, &["remote", "get-url", "origin"], timeout).success {
        state.error = Some("no origin remote configured".to_owned());
        state.last_result = Some("no_remote".to_owned());
        return (state, TargetOutcome::new("no_remote"));
    }

    let Some(start_head) = head_of(main, timeout) else {
        state.error = Some("cannot read main HEAD".to_owned());
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    };
    let start_fp = match worktree_fingerprint(main, timeout) {
        Ok(fingerprint) => fingerprint,
        Err(error) => {
            state.error = Some(error);
            state.last_result = Some("error".to_owned());
            let error = state.error.clone();
            return (state, failed_notice(error, "error"));
        }
    };

    let temp_path = merge_worktree_path(config, target);
    let temp = match TempWorktree::create(main, &temp_path, &start_head, timeout) {
        Ok(temp) => temp,
        Err(error) => {
            state.error = Some(error);
            state.last_result = Some("error".to_owned());
            let error = state.error.clone();
            return (state, failed_notice(error, "error"));
        }
    };
    let rehearsal = &temp.path;

    let fetch = git(rehearsal, &["fetch", "--prune", "origin"], timeout);
    if !fetch.success {
        state.error = Some(format!("fetch failed: {}", fetch.detail()));
        state.last_result = Some("remote_unreachable".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "remote_unreachable"));
    }

    let upstream = format!("refs/remotes/origin/{branch}");
    if !git(
        rehearsal,
        &["rev-parse", "--verify", "--quiet", &upstream],
        timeout,
    )
    .success
    {
        // 远端还没有这个分支：第一次同步就是把本地分支推上去。
        drop(temp);
        return finish_push(main, &branch, config, state, "pushed", now_ms);
    }

    let counts = git(
        rehearsal,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...origin/{branch}"),
        ],
        timeout,
    );
    if !counts.success {
        state.error = Some(format!("cannot compare with upstream: {}", counts.detail()));
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }
    let (ahead, behind) = parse_counts(&counts.stdout).unwrap_or((0, 0));

    if behind == 0 {
        drop(temp);
        if ahead > 0 {
            return finish_push(main, &branch, config, state, "pushed", now_ms);
        }
        state.last_result = Some("ok".to_owned());
        state.error = None;
        state.last_success_ms = Some(now_ms);
        state.last_head = head_of(main, timeout);
        return (state, TargetOutcome::new("ok"));
    }

    let remote_ref = format!("origin/{branch}");
    let integrate = match config.merge_strategy {
        MergeStrategy::Merge => git(
            rehearsal,
            &["-c", "core.editor=true", "merge", "--no-edit", &remote_ref],
            timeout,
        ),
        MergeStrategy::Rebase => git(rehearsal, &["rebase", &remote_ref], timeout),
    };

    if !integrate.success {
        let conflicts = conflict_files(rehearsal, timeout);
        if conflicts.is_empty() {
            abort_integration(rehearsal, config.merge_strategy, timeout);
            state.error = Some(format!("git integration failed: {}", integrate.detail()));
            state.last_result = Some("error".to_owned());
            let error = state.error.clone();
            return (state, failed_notice(error, "error"));
        }

        if config.auto_resolve == AutoResolve::None {
            abort_integration(rehearsal, config.merge_strategy, timeout);
            state.error = Some(
                "merge conflict; stopped without choosing a side (auto_resolve is none)".to_owned(),
            );
            state.last_result = Some("conflict".to_owned());
            state.conflict_files = conflicts.clone();
            let notice = SyncNotice::Conflict {
                files: conflicts.clone(),
            };
            return (
                state,
                TargetOutcome {
                    result: "conflict",
                    notice: Some(notice),
                },
            );
        }

        match resolve_conflicts(rehearsal, config, &conflicts, timeout) {
            Ok(()) => {}
            Err(error) => {
                abort_integration(rehearsal, config.merge_strategy, timeout);
                state.error = Some(format!(
                    "auto_resolve failed: {error}; conflict files: {}",
                    conflicts.join(", ")
                ));
                state.last_result = Some("error".to_owned());
                state.conflict_files = conflicts.clone();
                return (
                    state,
                    TargetOutcome {
                        result: "error",
                        notice: Some(SyncNotice::Failure {
                            reason: format!("auto_resolve failed: {error}"),
                        }),
                    },
                );
            }
        }
    }

    let Some(result_sha) = head_of(rehearsal, timeout) else {
        state.error = Some("cannot read rehearsal HEAD".to_owned());
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    };

    // 演练结束、落地之前：主工作区必须仍是同一 HEAD、同一指纹、仍然干净。
    let end_head = head_of(main, timeout);
    let end_fp = worktree_fingerprint(main, timeout).ok();
    let end_status = git(main, &["status", "--porcelain"], timeout);
    if end_head.as_deref() != Some(start_head.as_str())
        || end_fp.as_deref() != Some(start_fp.as_str())
        || !end_status.success
        || !end_status.stdout.trim().is_empty()
    {
        state.error =
            Some("concurrent write detected during merge rehearsal; landing skipped".to_owned());
        state.last_result = Some("concurrent_write".to_owned());
        return (
            state,
            TargetOutcome {
                result: "concurrent_write",
                notice: Some(SyncNotice::Failure {
                    reason: "concurrent_write: main worktree changed during merge rehearsal"
                        .to_owned(),
                }),
            },
        );
    }

    // 只在这一小段持锁：重新检查 + 落地。演练全程不持锁。
    let land = {
        let lock = acb_kernel::memory::worktree_write_lock(main);
        let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let locked_head = head_of(main, timeout);
        let locked_fp = worktree_fingerprint(main, timeout).ok();
        let locked_status = git(main, &["status", "--porcelain"], timeout);
        let changed = locked_head.as_deref() != Some(start_head.as_str())
            || locked_fp.as_deref() != Some(start_fp.as_str())
            || !locked_status.success
            || !locked_status.stdout.trim().is_empty();
        if changed {
            Err("concurrent_write: main worktree changed while acquiring lock".to_owned())
        } else {
            land_result(main, &result_sha, &start_head, timeout)
        }
    };

    drop(temp);

    if let Err(error) = land {
        state.error = Some(error.clone());
        state.last_result = Some("landing_failed".to_owned());
        return (state, failed_notice(Some(error), "landing_failed"));
    }

    let result = match config.merge_strategy {
        MergeStrategy::Merge => "merged",
        MergeStrategy::Rebase => "rebased",
    };
    finish_push(main, &branch, config, state, result, now_ms)
}

fn failed_notice(reason: Option<String>, result: &'static str) -> TargetOutcome {
    let reason = reason.unwrap_or_else(|| result.to_owned());
    TargetOutcome {
        result,
        notice: Some(SyncNotice::Failure { reason }),
    }
}

fn finish_push(
    worktree: &Path,
    branch: &str,
    config: &StewardConfig,
    mut state: SyncState,
    result: &'static str,
    now_ms: i64,
) -> (SyncState, TargetOutcome) {
    let timeout = Duration::from_secs(config.git_timeout_secs);
    let push = git(
        worktree,
        &["push", "origin", &format!("HEAD:refs/heads/{branch}")],
        timeout,
    );
    if !push.success {
        state.error = Some(format!("push failed: {}", push.detail()));
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }
    state.error = None;
    state.last_result = Some(result.to_owned());
    state.last_success_ms = Some(now_ms);
    state.last_head = head_of(worktree, timeout);
    let outcome = TargetOutcome {
        result,
        notice: None,
    };
    (state, outcome)
}

/// rebase/merge 冲突解决：按策略选一侧，然后继续/提交。
fn resolve_conflicts(
    worktree: &Path,
    config: &StewardConfig,
    conflicts: &[String],
    timeout: Duration,
) -> Result<(), String> {
    let side = match config.auto_resolve {
        AutoResolve::Local => match config.merge_strategy {
            MergeStrategy::Merge => "--ours",
            MergeStrategy::Rebase => "--theirs",
        },
        AutoResolve::Remote => match config.merge_strategy {
            MergeStrategy::Merge => "--theirs",
            MergeStrategy::Rebase => "--ours",
        },
        AutoResolve::None => return Err("auto_resolve is none".to_owned()),
    };

    for file in conflicts {
        let checkout = git(worktree, &["checkout", side, "--", file], timeout);
        if !checkout.success {
            return Err(format!(
                "cannot checkout {side} for {file}: {}",
                checkout.detail()
            ));
        }
        if !git(worktree, &["add", "--", file], timeout).success {
            return Err(format!("cannot stage resolved file {file}"));
        }
    }

    match config.merge_strategy {
        MergeStrategy::Merge => {
            let commit = git(
                worktree,
                &["-c", "core.editor=true", "commit", "--no-edit"],
                timeout,
            );
            if commit.success {
                Ok(())
            } else {
                Err(format!("merge commit failed: {}", commit.detail()))
            }
        }
        MergeStrategy::Rebase => {
            for _ in 0..10 {
                let cont = git(
                    worktree,
                    &["-c", "core.editor=true", "rebase", "--continue"],
                    timeout,
                );
                if cont.success {
                    return Ok(());
                }
                let next = conflict_files(worktree, timeout);
                if next.is_empty() {
                    return Err(format!("rebase continue failed: {}", cont.detail()));
                }
                for file in &next {
                    if !git(worktree, &["checkout", side, "--", file], timeout).success
                        || !git(worktree, &["add", "--", file], timeout).success
                    {
                        return Err(format!("cannot resolve {file} during rebase"));
                    }
                }
            }
            Err("rebase conflict did not converge".to_owned())
        }
    }
}

fn abort_integration(worktree: &Path, strategy: MergeStrategy, timeout: Duration) {
    let abort = match strategy {
        MergeStrategy::Merge => "merge",
        MergeStrategy::Rebase => "rebase",
    };
    let _ = git(worktree, &[abort, "--abort"], timeout);
}

fn conflict_files(worktree: &Path, timeout: Duration) -> Vec<String> {
    let result = git(
        worktree,
        &["diff", "--name-only", "--diff-filter=U"],
        timeout,
    );
    result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn commits_since(worktree: &Path, base: Option<&str>, timeout_secs: u64) -> u64 {
    let Some(base) = base else {
        return 0;
    };
    let timeout = Duration::from_secs(timeout_secs);
    let output = git(
        worktree,
        &["rev-list", "--count", &format!("{base}..HEAD")],
        timeout,
    );
    output.stdout.trim().parse::<u64>().unwrap_or(0)
}

fn head_of(worktree: &Path, timeout: Duration) -> Option<String> {
    let output = git(worktree, &["rev-parse", "HEAD"], timeout);
    if output.success {
        Some(output.stdout.trim().to_owned())
    } else {
        None
    }
}

fn parse_counts(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.split_whitespace();
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

struct GitResult {
    success: bool,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl GitResult {
    fn detail(&self) -> String {
        if self.timed_out {
            return "git command timed out".to_owned();
        }
        let detail = self.stderr.trim();
        if detail.is_empty() {
            "git command failed".to_owned()
        } else {
            detail.to_owned()
        }
    }
}

fn git(worktree: &Path, args: &[&str], timeout: Duration) -> GitResult {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(worktree)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return GitResult {
            success: false,
            stdout: String::new(),
            stderr: "cannot spawn git".to_owned(),
            timed_out: false,
        };
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                let mut err = String::new();
                if let Some(handle) = stdout.as_mut() {
                    let _ = handle.read_to_string(&mut out);
                }
                if let Some(handle) = stderr.as_mut() {
                    let _ = handle.read_to_string(&mut err);
                }
                return GitResult {
                    success: status.success(),
                    stdout: out,
                    stderr: err,
                    timed_out: false,
                };
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return GitResult {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("git timed out after {timeout:?}"),
                    timed_out: true,
                };
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                return GitResult {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("git wait failed: {error}"),
                    timed_out: false,
                };
            }
        }
    }
}

fn load_status(path: &Path) -> Option<StatusFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_status(path: &Path, status: &StatusFile) -> Result<(), String> {
    let temp = path.with_extension("json.tmp");
    std::fs::write(
        &temp,
        serde_json::to_vec_pretty(status).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    std::fs::rename(&temp, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("acb-steward-{name}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[test]
    fn landing_failure_rolls_back_to_old_head() {
        let root = test_dir("landing");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        let timeout = Duration::from_secs(10);
        assert!(git(&repo, &["init", "-b", "main"], timeout).success);
        assert!(
            git(
                &repo,
                &["config", "user.email", "test@example.com"],
                timeout
            )
            .success
        );
        assert!(git(&repo, &["config", "user.name", "Test"], timeout).success);
        std::fs::write(repo.join("a.md"), "a").expect("file");
        assert!(git(&repo, &["add", "a.md"], timeout).success);
        assert!(git(&repo, &["commit", "-m", "a"], timeout).success);
        let head = head_of(&repo, timeout).expect("head");

        let error = land_result(
            &repo,
            "0000000000000000000000000000000000000000",
            &head,
            timeout,
        )
        .expect_err("不存在的 result 必须落地失败");
        assert!(error.contains("reset"), "{error}");
        assert_eq!(
            head_of(&repo, timeout).as_deref(),
            Some(head.as_str()),
            "失败后必须回到旧 HEAD"
        );
        let status = git(&repo, &["status", "--porcelain"], timeout);
        assert!(
            status.success && status.stdout.trim().is_empty(),
            "回滚后主工作区必须干净"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
