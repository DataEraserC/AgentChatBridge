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
use std::time::{Duration, Instant, SystemTime};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutoResolve {
    /// 不自动解决，冲突即停。
    #[default]
    None,
    /// 冲突文件保留本地一侧。
    Local,
    /// 冲突文件保留远端一侧。
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
    /// remote_unreachable / error。
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
            Some("conflict") | Some("remote_unreachable") | Some("error")
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

/// 同步一个 worktree：脏工作区跳过；fetch → merge/rebase → push。
fn sync_target(target: &Target, config: &StewardConfig, now_ms: i64) -> (SyncState, TargetOutcome) {
    let mut state = SyncState {
        last_attempt_ms: Some(now_ms),
        ..SyncState::default()
    };
    let worktree = &target.worktree;
    let timeout = Duration::from_secs(config.git_timeout_secs);

    let status = git(worktree, &["status", "--porcelain"], timeout);
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

    let branch_out = git(worktree, &["rev-parse", "--abbrev-ref", "HEAD"], timeout);
    if !branch_out.success {
        state.error = Some(format!(
            "cannot determine current branch: {}",
            branch_out.detail()
        ));
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }
    let branch = branch_out.stdout.trim();
    if branch.is_empty() || branch == "HEAD" {
        state.error = Some("cannot determine current branch".to_owned());
        state.last_result = Some("error".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }

    if !git(worktree, &["remote", "get-url", "origin"], timeout).success {
        state.error = Some("no origin remote configured".to_owned());
        state.last_result = Some("no_remote".to_owned());
        return (state, TargetOutcome::new("no_remote"));
    }

    let fetch = git(worktree, &["fetch", "--prune", "origin"], timeout);
    if !fetch.success {
        state.error = Some(format!("fetch failed: {}", fetch.detail()));
        state.last_result = Some("remote_unreachable".to_owned());
        let error = state.error.clone();
        return (state, failed_notice(error, "remote_unreachable"));
    }

    let upstream = format!("refs/remotes/origin/{branch}");
    if !git(
        worktree,
        &["rev-parse", "--verify", "--quiet", &upstream],
        timeout,
    )
    .success
    {
        // 远端还没有这个分支：第一次同步就是把本地分支推上去。
        return finish_push(worktree, branch, config, state, "pushed", now_ms);
    }

    let counts = git(
        worktree,
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
        if ahead > 0 {
            return finish_push(worktree, branch, config, state, "pushed", now_ms);
        }
        state.last_result = Some("ok".to_owned());
        state.error = None;
        state.last_success_ms = Some(now_ms);
        state.last_head = head_of(worktree, timeout);
        return (state, TargetOutcome::new("ok"));
    }

    let remote_ref = format!("origin/{branch}");
    let integrate = match config.merge_strategy {
        MergeStrategy::Merge => git(
            worktree,
            &["-c", "core.editor=true", "merge", "--no-edit", &remote_ref],
            timeout,
        ),
        MergeStrategy::Rebase => git(worktree, &["rebase", &remote_ref], timeout),
    };

    if integrate.success {
        let result = match config.merge_strategy {
            MergeStrategy::Merge => "merged",
            MergeStrategy::Rebase => "rebased",
        };
        return finish_push(worktree, branch, config, state, result, now_ms);
    }

    let conflicts = conflict_files(worktree, timeout);
    if conflicts.is_empty() {
        state.error = Some(format!("git integration failed: {}", integrate.detail()));
        state.last_result = Some("error".to_owned());
        abort_integration(worktree, config.merge_strategy, timeout);
        let error = state.error.clone();
        return (state, failed_notice(error, "error"));
    }

    if config.auto_resolve == AutoResolve::None {
        abort_integration(worktree, config.merge_strategy, timeout);
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

    match resolve_conflicts(worktree, config, &conflicts, timeout) {
        Ok(()) => {
            let result = match config.merge_strategy {
                MergeStrategy::Merge => "merged",
                MergeStrategy::Rebase => "rebased",
            };
            finish_push(worktree, branch, config, state, result, now_ms)
        }
        Err(error) => {
            abort_integration(worktree, config.merge_strategy, timeout);
            state.error = Some(format!(
                "auto_resolve failed: {error}; conflict files: {}",
                conflicts.join(", ")
            ));
            state.last_result = Some("error".to_owned());
            state.conflict_files = conflicts.clone();
            (
                state,
                TargetOutcome {
                    result: "error",
                    notice: Some(SyncNotice::Failure {
                        reason: format!("auto_resolve failed: {error}"),
                    }),
                },
            )
        }
    }
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
