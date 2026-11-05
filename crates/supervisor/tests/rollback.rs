//! 监督循环的执行测试：断言回滚/重启/策略错误真的被执行，而不是只算出决策。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acb_supervisor::{
    BuiltinPolicy, Clock, CommandSpec, Config, ExitInfo, Parts, Policy, PolicySettings, Probe,
    Runner, Sleeper, Supervisor,
};
use acb_supervisor_policy::{Decision, DecisionKind, PolicyConfig, Snapshot};
use serde_json::Value;

struct SharedClock(Arc<AtomicI64>);

impl Clock for SharedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct RunnerState {
    running: bool,
    starts: Vec<CommandSpec>,
    stops: usize,
    fail_start: bool,
    last_exit: Option<ExitInfo>,
}

#[derive(Clone, Default)]
struct SharedRunner(Arc<Mutex<RunnerState>>);

impl SharedRunner {
    fn new() -> Self {
        Self::default()
    }

    fn set_exited(&self, code: i32) {
        let mut state = self.0.lock().expect("runner");
        state.running = false;
        state.last_exit = Some(ExitInfo {
            code: Some(code),
            signal: None,
        });
    }

    fn fail_next_start(&self) {
        self.0.lock().expect("runner").fail_start = true;
    }

    fn starts(&self) -> Vec<CommandSpec> {
        self.0.lock().expect("runner").starts.clone()
    }

    fn stops(&self) -> usize {
        self.0.lock().expect("runner").stops
    }
}

impl Runner for SharedRunner {
    fn start(&mut self, spec: &CommandSpec) -> Result<(), String> {
        let mut state = self.0.lock().expect("runner");
        if state.fail_start {
            state.fail_start = false;
            return Err("fake start failed".to_owned());
        }
        state.starts.push(spec.clone());
        state.running = true;
        state.last_exit = None;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        let mut state = self.0.lock().expect("runner");
        state.stops += 1;
        state.running = false;
        Ok(())
    }

    fn running(&mut self) -> bool {
        self.0.lock().expect("runner").running
    }

    fn last_exit(&mut self) -> Option<ExitInfo> {
        self.0.lock().expect("runner").last_exit.clone()
    }
}

#[derive(Clone)]
struct SharedProbe(Arc<Mutex<VecDeque<bool>>>);

impl SharedProbe {
    fn new(results: Vec<bool>) -> Self {
        Self(Arc::new(Mutex::new(results.into())))
    }
}

impl Probe for SharedProbe {
    fn healthy(&mut self) -> bool {
        self.0.lock().expect("probe").pop_front().unwrap_or(true)
    }
}

#[derive(Clone, Default)]
struct SharedSleeper(Arc<Mutex<Vec<u64>>>);

impl SharedSleeper {
    fn sleeps(&self) -> Vec<u64> {
        self.0.lock().expect("sleeper").clone()
    }
}

impl Sleeper for SharedSleeper {
    fn sleep_ms(&mut self, millis: u64) {
        self.0.lock().expect("sleeper").push(millis);
    }
}

enum FakeMode {
    Hold,
    Error,
    Sleep(u64),
}

struct FakePolicy {
    calls: AtomicUsize,
    mode: Mutex<FakeMode>,
}

impl FakePolicy {
    fn hold() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            mode: Mutex::new(FakeMode::Hold),
        }
    }
    fn error() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            mode: Mutex::new(FakeMode::Error),
        }
    }
    fn slow(millis: u64) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            mode: Mutex::new(FakeMode::Sleep(millis)),
        }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Policy for FakePolicy {
    fn id(&self) -> &str {
        "fake"
    }

    fn evaluate(&self, _snapshot: &Snapshot, _options: &PolicyConfig) -> Result<Decision, String> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        match *self.mode.lock().expect("mode") {
            FakeMode::Hold => Ok(Decision {
                decision: DecisionKind::Hold,
                reason_code: "fake_hold".to_owned(),
                reason: "fake hold".to_owned(),
                delay_ms: Some(0),
            }),
            FakeMode::Error => Err("fake policy error".to_owned()),
            FakeMode::Sleep(millis) => {
                std::thread::sleep(Duration::from_millis(millis));
                Ok(Decision {
                    decision: DecisionKind::Hold,
                    reason_code: "fake_slow".to_owned(),
                    reason: "fake slow hold".to_owned(),
                    delay_ms: Some(0),
                })
            }
        }
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "acb-supervisor-{name}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn config(dir: &std::path::Path, lkg: Option<&str>) -> Config {
    Config {
        program: "/current".to_owned(),
        args: Vec::new(),
        lkg: lkg.map(CommandSpec::new),
        health_url: None,
        state_dir: dir.to_string_lossy().into_owned(),
        probe_interval_secs: 1,
        stable_window_secs: 60,
        policy: PolicySettings::default(),
    }
}

fn supervisor(
    config: Config,
    clock: Arc<AtomicI64>,
    runner: SharedRunner,
    probe: SharedProbe,
    sleeper: SharedSleeper,
    policy: Arc<dyn Policy>,
) -> Supervisor {
    Supervisor::new(
        config,
        Parts {
            clock: Box::new(SharedClock(clock)),
            runner: Box::new(runner),
            probe: Box::new(probe),
            sleeper: Box::new(sleeper),
            policy,
        },
    )
    .expect("supervisor starts")
}

fn audit_events(supervisor: &Supervisor) -> Vec<Value> {
    std::fs::read_to_string(supervisor.audit_path())
        .expect("audit file")
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn find_event<'a>(events: &'a [Value], name: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["event"] == name)
        .collect()
}

#[test]
fn rollback_is_executed_and_switches_to_lkg() {
    let dir = temp_dir("rollback-ok");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true]);
    let sleeper = SharedSleeper::default();
    let policy: Arc<dyn Policy> = Arc::new(FakePolicy::hold());
    let mut config = config(&dir, Some("/lkg"));
    config.policy.options.consecutive_failures = 1;
    config.policy.options.startup_grace_secs = 0;
    config.policy.options.window_failures = 0;
    config.policy.options.unhealthy_secs = 0;
    config.policy.options.unhealthy_probes = 0;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        Arc::clone(&policy),
    );

    assert_eq!(runner.starts().len(), 1, "启动当前版本");
    runner.set_exited(1);
    supervisor.tick();

    assert_eq!(supervisor.current().program, "/lkg");
    assert_eq!(runner.starts().len(), 2, "回滚必须真的启动 LKG");
    assert_eq!(runner.starts()[1].program, "/lkg");
    let events = audit_events(&supervisor);
    assert!(
        find_event(&events, "execute")
            .iter()
            .any(|event| event["decision"] == "rollback"
                && event["result"] == "rolled_back"
                && event["target"]["program"] == "/lkg"),
        "审计里要有回滚执行结果：{events:?}"
    );
}

#[test]
fn rollback_start_failure_is_visible_and_stops_recovery() {
    let dir = temp_dir("rollback-fail");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true]);
    let sleeper = SharedSleeper::default();
    let policy: Arc<dyn Policy> = Arc::new(FakePolicy::hold());
    let mut config = config(&dir, Some("/lkg"));
    config.policy.options.consecutive_failures = 1;
    config.policy.options.startup_grace_secs = 0;
    config.policy.options.window_failures = 0;
    config.policy.options.unhealthy_secs = 0;
    config.policy.options.unhealthy_probes = 0;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        Arc::clone(&policy),
    );
    runner.set_exited(1);
    runner.fail_next_start();
    supervisor.tick();

    assert!(supervisor.failed().is_some(), "回滚启动失败必须标记 failed");
    let events = audit_events(&supervisor);
    assert!(
        find_event(&events, "execute")
            .iter()
            .any(|event| event["result"] == "rollback_failed"),
        "审计里要有失败原因：{events:?}"
    );
    assert_eq!(runner.starts().len(), 1, "失败后不再自动重启");
}

#[test]
fn policy_timeout_holds_then_three_errors_fail() {
    let dir = temp_dir("policy-timeout");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true, true, true]);
    let sleeper = SharedSleeper::default();
    let fake: Arc<FakePolicy> = Arc::new(FakePolicy::slow(200));
    let policy: Arc<dyn Policy> = fake.clone();
    let mut config = config(&dir, None);
    config.health_url = Some("http://127.0.0.1:1/healthz".to_owned());
    config.policy.decision_timeout_ms = 100;
    config.policy.policy_error_fail_after = 3;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        Arc::clone(&policy),
    );

    for step in 0..3 {
        clock.store(step as i64 * 1000, Ordering::Relaxed);
        supervisor.tick();
    }
    assert!(supervisor.failed().is_some(), "连续 3 次超时转 fail");
    assert_eq!(fake.calls(), 3, "超时也要真的调用过策略");
    let events = audit_events(&supervisor);
    assert_eq!(find_event(&events, "policy_error").len(), 3, "{events:?}");
    assert!(
        find_event(&events, "execute")
            .iter()
            .any(|event| event["decision"] == "fail"),
        "最后一次要执行 fail：{events:?}"
    );
}

#[test]
fn consecutive_policy_errors_hold_then_fail() {
    let dir = temp_dir("policy-error");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true, true, true]);
    let sleeper = SharedSleeper::default();
    let fake: Arc<FakePolicy> = Arc::new(FakePolicy::error());
    let policy: Arc<dyn Policy> = fake.clone();
    let mut config = config(&dir, None);
    config.health_url = Some("http://127.0.0.1:1/healthz".to_owned());
    config.policy.policy_error_fail_after = 3;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        policy,
    );

    for step in 1..=2 {
        clock.store(step * 1000, Ordering::Relaxed);
        supervisor.tick();
        assert!(supervisor.failed().is_none(), "前两次错误只 hold");
    }
    clock.store(3000, Ordering::Relaxed);
    supervisor.tick();
    assert!(supervisor.failed().is_some(), "第 3 次错误转 fail");

    let events = audit_events(&supervisor);
    assert_eq!(find_event(&events, "policy_error").len(), 3, "{events:?}");
    assert!(
        find_event(&events, "execute")
            .iter()
            .any(|event| event["decision"] == "hold"),
        "单次错误要先执行 hold：{events:?}"
    );
    assert!(
        find_event(&events, "execute")
            .iter()
            .any(|event| event["decision"] == "fail"),
        "达到阈值要真的停止自动恢复：{events:?}"
    );
}

#[test]
fn startup_grace_keeps_restarting_without_rollback() {
    let dir = temp_dir("grace");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true]);
    let sleeper = SharedSleeper::default();
    let policy: Arc<dyn Policy> = Arc::new(FakePolicy::hold());
    let mut config = config(&dir, Some("/lkg"));
    config.policy.options.consecutive_failures = 1;
    config.policy.options.startup_grace_secs = 30;
    config.policy.options.window_failures = 0;
    config.policy.options.unhealthy_secs = 0;
    config.policy.options.unhealthy_probes = 0;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        Arc::clone(&policy),
    );

    for step in 1..=3 {
        clock.store(step * 1000, Ordering::Relaxed);
        runner.set_exited(1);
        supervisor.tick();
        assert_eq!(supervisor.current().program, "/current", "宽限期内不切换");
    }
    assert_eq!(runner.starts().len(), 4, "初始 + 3 次宽限期重启");
    let events = audit_events(&supervisor);
    assert!(
        find_event(&events, "decision")
            .iter()
            .any(|event| event["reason_code"] == "startup_grace"),
        "{events:?}"
    );
}

#[test]
fn unhealthy_secs_waits_for_the_window_before_rollback() {
    let dir = temp_dir("unhealthy");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![false, false, false]);
    let sleeper = SharedSleeper::default();
    let policy: Arc<dyn Policy> = Arc::new(BuiltinPolicy);
    let mut config = config(&dir, Some("/lkg"));
    config.health_url = Some("http://127.0.0.1:1/healthz".to_owned());
    config.policy.options.startup_grace_secs = 0;
    config.policy.options.consecutive_failures = 0;
    config.policy.options.window_failures = 0;
    config.policy.options.unhealthy_secs = 60;
    config.policy.options.unhealthy_probes = 0;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        policy,
    );

    supervisor.tick();
    assert_eq!(supervisor.current().program, "/current");
    clock.store(59_000, Ordering::Relaxed);
    supervisor.tick();
    assert_eq!(supervisor.current().program, "/current");
    clock.store(61_000, Ordering::Relaxed);
    supervisor.tick();
    assert_eq!(supervisor.current().program, "/lkg", "到 60s 才回滚");
    assert_eq!(runner.stops(), 1, "运行中的进程必须先停再切 LKG");
}

#[test]
fn hold_restart_uses_the_backoff_ladder() {
    let dir = temp_dir("backoff");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true]);
    let sleeper = SharedSleeper::default();
    let policy: Arc<dyn Policy> = Arc::new(FakePolicy::hold());
    let mut config = config(&dir, None);
    config.policy.options.startup_grace_secs = 0;
    config.policy.options.consecutive_failures = 99;
    config.policy.options.window_failures = 0;
    config.policy.options.unhealthy_secs = 0;
    config.policy.options.unhealthy_probes = 0;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper.clone(),
        Arc::clone(&policy),
    );

    for step in 0..5 {
        clock.store(step as i64 * 1000, Ordering::Relaxed);
        runner.set_exited(1);
        supervisor.tick();
    }
    assert_eq!(
        sleeper.sleeps(),
        vec![1000, 2000, 4000, 8000, 30_000],
        "退避阶梯要按 hold 次数增长"
    );
}

#[test]
fn stable_window_promotes_current_to_lkg() {
    let dir = temp_dir("promote");
    let clock = Arc::new(AtomicI64::new(0));
    let runner = SharedRunner::new();
    let probe = SharedProbe::new(vec![true, true]);
    let sleeper = SharedSleeper::default();
    let policy: Arc<dyn Policy> = Arc::new(FakePolicy::hold());
    let mut config = config(&dir, None);
    config.health_url = Some("http://127.0.0.1:1/healthz".to_owned());
    config.stable_window_secs = 60;
    let mut supervisor = supervisor(
        config,
        Arc::clone(&clock),
        runner.clone(),
        probe,
        sleeper,
        Arc::clone(&policy),
    );

    supervisor.tick();
    assert!(supervisor.lkg().is_none());
    clock.store(61_000, Ordering::Relaxed);
    supervisor.tick();
    assert_eq!(
        supervisor.lkg().map(|spec| spec.program.as_str()),
        Some("/current")
    );
    assert!(
        audit_events(&supervisor)
            .iter()
            .any(|event| event["event"] == "lkg_promoted"),
        "LKG 提升要留审计"
    );
}
