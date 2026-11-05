//! 记忆管家的行为测试：调度触发、冲突停下、自动策略默认关闭、远端不可达不阻塞。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use acb_memory_steward::{
    AutoResolve, CombinedSchedule, MergeStrategy, Notifier, Scheduler, Steward, StewardConfig,
    SyncNotice, SyncState, Target,
};
use acb_proto::RoomId;

fn temp(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "acb-memory-steward-{name}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|error| panic!("git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn init_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("repo dir");
    git(path, &["init", "-b", "main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
}

fn init_bare(path: &Path) {
    std::fs::create_dir_all(path).expect("bare dir");
    git(path, &["init", "--bare", "-b", "main"]);
}

fn write_commit(repo: &Path, file: &str, content: &str, message: &str) {
    std::fs::write(repo.join(file), content).expect("write file");
    git(repo, &["add", file]);
    git(repo, &["commit", "-m", message]);
}

fn setup_remote_and_local(root: &Path) -> (PathBuf, PathBuf) {
    let remote = root.join("remote.git");
    init_bare(&remote);
    let local = root.join("local");
    std::fs::create_dir_all(&local).expect("local dir");
    git(
        root,
        &["clone", remote.to_str().unwrap(), local.to_str().unwrap()],
    );
    git(&local, &["config", "user.email", "test@example.com"]);
    git(&local, &["config", "user.name", "Test"]);
    write_commit(&local, "shared.md", "base", "base");
    git(&local, &["push", "origin", "main"]);
    (remote, local)
}

fn target(worktree: &Path) -> Target {
    Target {
        memory: "m1".to_owned(),
        session: "s1".to_owned(),
        room: Some(RoomId::parse("qq:group/1").expect("room")),
        worktree: worktree.to_path_buf(),
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<(String, SyncNotice)>>);

impl Notifier for Recorder {
    fn notify(&self, target: &Target, notice: &SyncNotice) {
        self.0
            .lock()
            .expect("recorder")
            .push((target.key(), notice.clone()));
    }
}

fn config(root: &Path, interval: u64, threshold: u64) -> StewardConfig {
    StewardConfig {
        enabled: true,
        interval_secs: interval,
        commit_threshold: threshold,
        state_dir: root.join("state").to_string_lossy().into_owned(),
        ..StewardConfig::default()
    }
}

#[test]
fn schedule_combines_time_and_commit_thresholds() {
    let state = SyncState::default();
    let schedule = CombinedSchedule {
        interval_secs: 60,
        commit_threshold: 2,
    };
    assert!(schedule.due(&state, 1000, 0), "首次没有尝试过应到期");
    let mut state = SyncState {
        last_attempt_ms: Some(1000),
        ..SyncState::default()
    };
    assert!(!schedule.due(&state, 1500, 0), "未到时间/提交量");
    assert!(schedule.due(&state, 1500, 2), "提交量先到触发");
    assert!(schedule.due(&state, 61_500, 0), "时间先到触发");
    state.last_attempt_ms = Some(0);
    assert!(schedule.due(&state, 61_000, 0));
}

#[test]
fn commit_threshold_triggers_a_sync_and_push() {
    let root = temp("commits");
    let (remote, local) = setup_remote_and_local(&root);
    let target = target(&local);
    let recorder = Arc::new(Recorder::default());
    let mut steward =
        Steward::new(config(&root, 0, 2), vec![target.clone()], recorder.clone()).expect("steward");

    // 第一次只建立基线，不因为历史提交触发。
    assert!(steward.run_once(1000).is_empty());
    write_commit(&local, "a.md", "a", "a");
    write_commit(&local, "b.md", "b", "b");
    let outcomes = steward.run_once(2000);
    assert_eq!(outcomes.len(), 1, "两次提交应触发一次同步");
    assert_eq!(outcomes[0].result, "pushed");
    assert_eq!(
        steward.state(&target).last_result.as_deref(),
        Some("pushed")
    );
    let remote_log = git(&remote, &["log", "--oneline", "main"]);
    assert!(remote_log.contains('b'), "本地提交应已推送：{remote_log}");
}

#[test]
fn time_interval_triggers_even_without_new_commits() {
    let root = temp("time");
    let (_remote, local) = setup_remote_and_local(&root);
    let target = target(&local);
    let recorder = Arc::new(Recorder::default());
    let mut steward =
        Steward::new(config(&root, 60, 0), vec![target.clone()], recorder).expect("steward");

    assert_eq!(steward.run_once(1000).len(), 1, "首次按时间到期");
    assert!(steward.run_once(1000).is_empty(), "同时间不重复");
    assert_eq!(steward.run_once(61_000).len(), 1, "满 60 秒后应再次同步");
    assert_eq!(
        steward.state(&target).last_attempt_ms,
        Some(61_000),
        "最近一次同步时间要写进状态"
    );
}

#[test]
fn conflict_stops_without_losing_the_local_side() {
    let root = temp("conflict");
    let (remote, local) = setup_remote_and_local(&root);
    let other = root.join("other");
    git(
        &root,
        &["clone", remote.to_str().unwrap(), other.to_str().unwrap()],
    );
    git(&other, &["config", "user.email", "other@example.com"]);
    git(&other, &["config", "user.name", "Other"]);

    let target = target(&local);
    let recorder = Arc::new(Recorder::default());
    let mut steward =
        Steward::new(config(&root, 0, 1), vec![target.clone()], recorder.clone()).expect("steward");
    assert!(steward.run_once(1000).is_empty(), "先建基线");

    write_commit(&local, "shared.md", "local", "local");
    write_commit(&other, "shared.md", "remote", "remote");
    git(&other, &["push", "origin", "main"]);

    let outcomes = steward.run_once(2000);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result, "conflict");
    assert_eq!(
        outcomes[0].conflict_files,
        vec!["shared.md".to_owned()],
        "冲突文件要列出来"
    );
    let local_content = std::fs::read_to_string(local.join("shared.md")).expect("local file");
    assert_eq!(local_content, "local", "默认策略不能静默取远端一侧");
    assert_eq!(
        git(&local, &["show", "HEAD:shared.md"]).trim(),
        "local",
        "本地提交必须保持原样"
    );
    let notices = recorder.0.lock().expect("recorder");
    assert!(
        notices.iter().any(|(_, notice)| matches!(
            notice,
            SyncNotice::Conflict { files } if files == &["shared.md".to_owned()]
        )),
        "冲突要通知会话：{notices:?}"
    );
    assert_eq!(
        steward.state(&target).conflict_count(),
        1,
        "状态里要暴露冲突文件数"
    );
}

#[test]
fn auto_resolve_is_opt_in() {
    assert_eq!(StewardConfig::default().auto_resolve, AutoResolve::None);
    let root = temp("auto-off");
    let (_remote, local) = setup_remote_and_local(&root);
    let target = target(&local);
    let mut steward = Steward::new(
        config(&root, 0, 1),
        vec![target],
        Arc::new(Recorder::default()),
    )
    .expect("steward");
    assert!(steward.run_once(1000).is_empty());
    assert_eq!(
        steward
            .state(&Target {
                memory: "m1".into(),
                session: "s1".into(),
                room: Some(RoomId::parse("qq:group/1").expect("room")),
                worktree: local.clone(),
            })
            .last_result,
        None,
        "默认配置不会自动解决任何东西"
    );
}

#[test]
fn remote_unreachable_only_records_status() {
    let root = temp("unreachable");
    let local = root.join("local");
    init_repo(&local);
    write_commit(&local, "a.md", "a", "a");
    git(
        &local,
        &["remote", "add", "origin", "/nonexistent/acb-remote.git"],
    );
    let target = target(&local);
    let recorder = Arc::new(Recorder::default());
    let mut steward =
        Steward::new(config(&root, 0, 1), vec![target.clone()], recorder.clone()).expect("steward");

    assert!(steward.run_once(1000).is_empty(), "先建基线");
    write_commit(&local, "b.md", "b", "b");
    let before = git(&local, &["rev-parse", "HEAD"]);
    let started = Instant::now();
    let outcomes = steward.run_once(2000);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "远端不可达不能把后台线程拖死"
    );
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result, "remote_unreachable");
    let state = steward.state(&target);
    assert_eq!(state.last_result.as_deref(), Some("remote_unreachable"));
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|error| error.contains("fetch")),
        "失败原因可查：{state:?}"
    );
    assert_eq!(
        git(&local, &["rev-parse", "HEAD"]),
        before,
        "本地提交不能丢"
    );
    let notices = recorder.0.lock().expect("recorder");
    assert!(
        notices.iter().any(|(_, notice)| matches!(
            notice,
            SyncNotice::Failure { reason } if reason.contains("fetch")
        )),
        "失败要通知：{notices:?}"
    );
}

#[test]
fn merge_strategy_defaults_to_merge_and_auto_resolve_is_explicit() {
    let config = StewardConfig::default();
    assert_eq!(config.merge_strategy, MergeStrategy::Merge);
    assert_eq!(config.auto_resolve, AutoResolve::None);
    let parsed = StewardConfig::from_toml(
        r#"
        [memory_steward]
        enabled = true
        interval_secs = 3600
        commit_threshold = 5
        merge_strategy = "rebase"
        auto_resolve = "local"
        state_dir = "/tmp/acb-memory-steward"
        "#,
    )
    .expect("config parses");
    assert_eq!(parsed.merge_strategy, MergeStrategy::Rebase);
    assert_eq!(parsed.auto_resolve, AutoResolve::Local);
    let invalid = StewardConfig::from_toml(
        r#"
        [memory_steward]
        enabled = true
        interval_secs = 0
        commit_threshold = 0
        "#,
    )
    .expect_err("两个触发器都为 0 必须报错");
    assert!(invalid.contains("interval_secs"), "{invalid}");
}
