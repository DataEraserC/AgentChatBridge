//! 全局依赖状态表（connectivity 第①②步）。
//!
//! 只记录「这个组件现在可不可用」以及最后一次状态迁移，不做通知、不探测远端。
//! harness 由接入点上报；平台适配器通过 `AdapterStatus` 上报。
//! `attempts` 记每一次连接尝试，与是否连上无关；`neverConnected` 只看 `lastOkAt`。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{Value, json};

use crate::config::Config;
use crate::notice::{NoticeEvent, NoticeSink};

const TRANSITION_KEEP: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DependencyKind {
    Harness,
    Platform,
    Model,
}

impl DependencyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::Platform => "platform",
            Self::Model => "model",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Key {
    kind: DependencyKind,
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Unknown,
    Up,
    Down,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    key: Key,
    configured: bool,
    expected: bool,
    state: State,
    since: i64,
    last_ok_at: Option<i64>,
    last_attempt_at: Option<i64>,
    attempts: u64,
    last_error: Option<String>,
    last_class: Option<String>,
}

#[derive(Debug, Clone)]
struct Transition {
    seq: u64,
    at: i64,
    kind: DependencyKind,
    id: String,
    from: State,
    to: State,
    class: Option<String>,
    detail: Option<String>,
    since: i64,
    last_ok_at: Option<i64>,
}

#[derive(Debug)]
struct Inner {
    entries: BTreeMap<Key, Entry>,
    transitions: VecDeque<Transition>,
    seq: u64,
}

/// 可克隆的依赖注册表。锁独立于 `Bridge` 的大锁，读快照不碰任何 I/O。
#[derive(Clone)]
pub struct DependencyRegistry {
    inner: Arc<Mutex<Inner>>,
    /// 带外通知接收端。未配置时保持 None，整条路径零开销。
    sink: Arc<RwLock<Option<Arc<dyn NoticeSink>>>>,
}

impl Default for DependencyRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: BTreeMap::new(),
                transitions: VecDeque::new(),
                seq: 0,
            })),
            sink: Arc::new(RwLock::new(None)),
        }
    }
}

impl DependencyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入带外通知接收端。生产 main 只在 `[notifications]` 完整配置时调用。
    pub fn set_notice_sink(&self, sink: Arc<dyn NoticeSink>) {
        *self
            .sink
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }

    fn emit(&self, event: NoticeEvent) {
        let sink = self
            .sink
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink.notify(event);
        }
    }

    /// turn 失败通知被 in-band 抑制时，把这件事交给带外层。
    pub fn notice_turn_suppressed(
        &self,
        session: &str,
        room: &str,
        component: Option<(&str, Option<&str>, &str)>,
        reason: &str,
        fingerprint: &str,
    ) {
        if self
            .sink
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
        {
            return;
        }
        self.emit(NoticeEvent::TurnSuppressed {
            session: session.to_owned(),
            room: room.to_owned(),
            component: component.map(|(component, _, _)| component.to_owned()),
            component_id: component.and_then(|(_, id, _)| id).map(ToOwned::to_owned),
            class: component.map(|(_, _, class)| class.to_owned()),
            reason: reason.to_owned(),
            fingerprint: fingerprint.to_owned(),
        });
    }

    /// 按当前配置登记 harness 与平台连接。
    pub fn from_config(config: &Config) -> Self {
        let registry = Self::default();
        registry.retain_config(config);
        registry
    }

    /// 按新配置重组依赖表（启动与配置热重载共用）。
    ///
    /// - 配置里新增的依赖以 `unknown` 登记；平台带 `not_configured`。
    /// - 配置里仍在的条目原样保留：连接状态不因重载被重置。
    /// - 配置里被移除的条目**直接删除**，不留 `down` 幽灵，也不写迁移环。
    ///   迁移环只描述运行时状态变化；配置增删用 `[deps] removed` 日志表达。
    pub fn retain_config(&self, config: &Config) {
        let now = now_millis();
        let harnesses: BTreeSet<&str> = config
            .harnesses
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let platforms: BTreeSet<&str> = config
            .connections
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let models: BTreeSet<String> = config
            .sessions
            .iter()
            .map(|session| {
                session
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("model:{}/default", session.harness))
            })
            .collect();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let stale: Vec<Key> = inner
            .entries
            .keys()
            .filter(|key| match key.kind {
                DependencyKind::Harness => !harnesses.contains(key.id.as_str()),
                DependencyKind::Platform => !platforms.contains(key.id.as_str()),
                DependencyKind::Model => !models.contains(&key.id),
            })
            .cloned()
            .collect();
        for key in stale {
            inner.entries.remove(&key);
            eprintln!("[deps] removed kind={} id={}", key.kind.as_str(), key.id);
        }

        for harness in &config.harnesses {
            let key = Key {
                kind: DependencyKind::Harness,
                id: harness.id.clone(),
            };
            inner.entries.entry(key.clone()).or_insert(Entry {
                key,
                configured: true,
                expected: true,
                state: State::Unknown,
                since: now,
                last_ok_at: None,
                last_attempt_at: None,
                attempts: 0,
                last_error: None,
                last_class: None,
            });
        }
        for connection in &config.connections {
            let key = Key {
                kind: DependencyKind::Platform,
                id: connection.id.clone(),
            };
            inner.entries.entry(key.clone()).or_insert(Entry {
                key,
                configured: true,
                expected: true,
                state: State::Unknown,
                since: now,
                last_ok_at: None,
                last_attempt_at: None,
                attempts: 0,
                last_error: Some("not configured".to_owned()),
                last_class: Some("not_configured".to_owned()),
            });
        }
        for id in models {
            let key = Key {
                kind: DependencyKind::Model,
                id,
            };
            inner.entries.entry(key.clone()).or_insert(Entry {
                key,
                configured: true,
                expected: true,
                state: State::Unknown,
                since: now,
                last_ok_at: None,
                last_attempt_at: None,
                attempts: 0,
                last_error: None,
                last_class: None,
            });
        }
    }

    /// 记一次连接尝试（不改变状态）。成功也是尝试，由 connected 前的 attempt 记一次。
    pub fn harness_attempt(&self, id: &str, now: i64) {
        self.mark_attempt(DependencyKind::Harness, id, now);
    }

    /// harness 握手成功：up，记录 lastOkAt。
    pub fn harness_connected(&self, id: &str, now: i64) {
        self.mark_up(DependencyKind::Harness, id, now);
    }

    /// harness 连接断开：down，class=closed。
    pub fn harness_disconnected(&self, id: &str, detail: &str, now: i64) {
        self.mark_down(DependencyKind::Harness, id, "closed", detail, now);
    }

    /// 记一次平台连接尝试（不改变状态）。
    pub fn platform_attempt(&self, id: &str, now: i64) {
        self.mark_attempt(DependencyKind::Platform, id, now);
    }

    /// 平台适配器上报连接成功（第②步使用）。
    pub fn platform_connected(&self, id: &str, now: i64) {
        self.mark_up(DependencyKind::Platform, id, now);
    }

    /// 平台适配器上报断开（第②步使用）。
    pub fn platform_disconnected(&self, id: &str, class: &str, detail: &str, now: i64) {
        self.mark_down(DependencyKind::Platform, id, class, detail, now);
    }

    /// harness 明确报告后端失败：把它记成 down，class 用 harness 给的分类。
    /// 连接状态仍由 endpoint 维护；这是「harness 连着但后端不可用」的叠加信号。
    pub fn harness_turn_down(&self, id: &str, class: &str, detail: &str, now: i64) {
        self.mark_down(DependencyKind::Harness, id, class, detail, now);
    }

    /// harness 报告模型失败：更新模型条目。
    pub fn model_down(&self, id: &str, class: &str, detail: &str, now: i64) {
        self.mark_down(DependencyKind::Model, id, class, detail, now);
    }

    /// 一轮成功：模型恢复可用。
    pub fn model_up(&self, id: &str, now: i64) {
        self.mark_up(DependencyKind::Model, id, now);
    }

    /// 平台连接尚未配置/尚未开始：保持 unknown，不报 down。
    pub fn platform_unknown(&self, id: &str, class: &str, detail: &str, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, DependencyKind::Platform, id, now);
        let key = Key {
            kind: DependencyKind::Platform,
            id: id.to_owned(),
        };
        let (from, notice) = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            let from = entry.state;
            entry.last_error = Some(detail.to_owned());
            entry.last_class = Some(class.to_owned());
            if from != State::Unknown {
                entry.state = State::Unknown;
                entry.since = now;
            }
            let notice = (from != State::Unknown).then(|| NoticeEvent::DependencyEdge {
                kind: key.kind.as_str().to_owned(),
                id: key.id.clone(),
                from: from.as_str().to_owned(),
                to: State::Unknown.as_str().to_owned(),
                since: entry.since,
                last_ok_at: entry.last_ok_at,
                class: Some(class.to_owned()),
                detail: Some(detail.to_owned()),
                attempts: entry.attempts,
            });
            (from, notice)
        };
        if from != State::Unknown {
            push_transition(
                &mut inner,
                key,
                from,
                State::Unknown,
                Some(class.to_owned()),
                Some(detail.to_owned()),
                now,
            );
        }
        drop(inner);
        if let Some(event) = notice {
            self.emit(event);
        }
    }

    fn mark_attempt(&self, kind: DependencyKind, id: &str, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, kind, id, now);
        let key = Key {
            kind,
            id: id.to_owned(),
        };
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_attempt_at = Some(now);
        }
    }

    fn mark_up(&self, kind: DependencyKind, id: &str, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, kind, id, now);
        let key = Key {
            kind,
            id: id.to_owned(),
        };
        let (from, notice) = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            let from = entry.state;
            entry.last_ok_at = Some(now);
            entry.last_error = None;
            entry.last_class = None;
            if from != State::Up {
                entry.state = State::Up;
                entry.since = now;
            }
            let notice = (from != State::Up).then(|| NoticeEvent::DependencyEdge {
                kind: key.kind.as_str().to_owned(),
                id: key.id.clone(),
                from: from.as_str().to_owned(),
                to: State::Up.as_str().to_owned(),
                since: entry.since,
                last_ok_at: entry.last_ok_at,
                class: None,
                detail: None,
                attempts: entry.attempts,
            });
            (from, notice)
        };
        if from != State::Up {
            push_transition(&mut inner, key, from, State::Up, None, None, now);
        }
        drop(inner);
        if let Some(event) = notice {
            self.emit(event);
        }
    }

    fn mark_down(&self, kind: DependencyKind, id: &str, class: &str, detail: &str, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, kind, id, now);
        let key = Key {
            kind,
            id: id.to_owned(),
        };
        let (from, notice) = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            let from = entry.state;
            entry.last_error = Some(detail.to_owned());
            entry.last_class = Some(class.to_owned());
            if from != State::Down {
                entry.state = State::Down;
                entry.since = now;
            }
            let notice = (from != State::Down).then(|| NoticeEvent::DependencyEdge {
                kind: key.kind.as_str().to_owned(),
                id: key.id.clone(),
                from: from.as_str().to_owned(),
                to: State::Down.as_str().to_owned(),
                since: entry.since,
                last_ok_at: entry.last_ok_at,
                class: Some(class.to_owned()),
                detail: Some(detail.to_owned()),
                attempts: entry.attempts,
            });
            (from, notice)
        };
        if from != State::Down {
            push_transition(
                &mut inner,
                key,
                from,
                State::Down,
                Some(class.to_owned()),
                Some(detail.to_owned()),
                now,
            );
        }
        drop(inner);
        if let Some(event) = notice {
            self.emit(event);
        }
    }

    /// `/api/dependencies?since=<seq>` 的完整快照。
    pub fn snapshot(&self, since: Option<u64>) -> Value {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dependencies: Vec<Value> = inner.entries.values().map(entry_json).collect();
        let transitions: Vec<Value> = inner
            .transitions
            .iter()
            .filter(|item| since.is_none_or(|since| item.seq > since))
            .map(transition_json)
            .collect();
        json!({
            "seq": inner.seq,
            "dependencies": dependencies,
            "transitions": transitions,
        })
    }

    /// `/healthz` 的一句话汇总，不要塞完整对象。
    pub fn summary(&self) -> Value {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut up = 0usize;
        let mut down = 0usize;
        let mut unknown = 0usize;
        for entry in inner.entries.values() {
            match entry.state {
                State::Up => up += 1,
                State::Down => down += 1,
                State::Unknown => unknown += 1,
            }
        }
        json!({
            "up": up,
            "down": down,
            "unknown": unknown,
        })
    }
}

fn upsert(inner: &mut Inner, kind: DependencyKind, id: &str, now: i64) {
    let key = Key {
        kind,
        id: id.to_owned(),
    };
    inner.entries.entry(key.clone()).or_insert_with(|| Entry {
        key,
        configured: true,
        expected: true,
        state: State::Unknown,
        since: now,
        last_ok_at: None,
        last_attempt_at: None,
        attempts: 0,
        last_error: None,
        last_class: None,
    });
}

fn push_transition(
    inner: &mut Inner,
    key: Key,
    from: State,
    to: State,
    class: Option<String>,
    detail: Option<String>,
    now: i64,
) {
    let kind = key.kind;
    let id = key.id.clone();
    let Some(entry) = inner.entries.get(&key) else {
        return;
    };
    let since = entry.since;
    let last_ok_at = entry.last_ok_at;
    let attempts = entry.attempts;
    inner.seq = inner.seq.saturating_add(1);
    let item = Transition {
        seq: inner.seq,
        at: now,
        kind,
        id: id.to_owned(),
        from,
        to,
        class,
        detail,
        since,
        last_ok_at,
    };
    eprintln!(
        "[deps] transition={}->{} kind={} id={} since={} last_ok={} attempts={} class={} detail=\"{}\"",
        from.as_str(),
        to.as_str(),
        kind.as_str(),
        id,
        since,
        last_ok_at
            .map(|at| at.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        attempts,
        item.class.as_deref().unwrap_or("-"),
        item.detail.as_deref().unwrap_or("").replace('"', "'")
    );
    inner.transitions.push_back(item);
    while inner.transitions.len() > TRANSITION_KEEP {
        inner.transitions.pop_front();
    }
}

fn entry_json(entry: &Entry) -> Value {
    json!({
        "kind": entry.key.kind.as_str(),
        "id": entry.key.id,
        "configured": entry.configured,
        "expected": entry.expected,
        "state": entry.state.as_str(),
        "since": entry.since,
        "lastOkAt": entry.last_ok_at,
        "lastAttemptAt": entry.last_attempt_at,
        "neverConnected": entry.last_ok_at.is_none(),
        "lastError": entry.last_error,
        "lastErrorClass": entry.last_class,
        "attempts": entry.attempts,
    })
}

fn transition_json(item: &Transition) -> Value {
    json!({
        "seq": item.seq,
        "at": item.at,
        "kind": item.kind.as_str(),
        "id": item.id,
        "from": item.from.as_str(),
        "to": item.to.as_str(),
        "class": item.class,
        "detail": item.detail,
        "since": item.since,
        "lastOkAt": item.last_ok_at,
    })
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_config() -> Config {
        Config::parse(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "bot"
            "#,
        )
        .expect("配置合法")
    }

    fn harness_config() -> Config {
        Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"
            "#,
        )
        .expect("配置合法")
    }

    fn entry(snapshot: &Value, id: &str) -> Value {
        snapshot["dependencies"]
            .as_array()
            .expect("数组")
            .iter()
            .find(|item| item["id"] == id)
            .cloned()
            .expect("有条目")
    }

    #[test]
    fn harness_starts_unknown_and_never_connected() {
        let registry = DependencyRegistry::from_config(&harness_config());
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "unknown");
        assert_eq!(dsh["neverConnected"], true);
        assert_eq!(dsh["attempts"], 0);
        assert_eq!(registry.summary()["unknown"], 1);
    }

    #[test]
    fn platform_starts_not_configured_then_connect_disconnect_reconnect() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["kind"], "platform");
        assert_eq!(qq["state"], "unknown");
        assert_eq!(qq["neverConnected"], true);
        assert_eq!(qq["lastErrorClass"], "not_configured");
        assert_eq!(qq["attempts"], 0);

        let status = crate::adapter::AdapterStatus::new("qq", registry.clone());
        status.attempt();
        status.connected();
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["state"], "up");
        assert_eq!(qq["neverConnected"], false);

        status.disconnected("unreachable", "socket closed");
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["state"], "down");
        assert_eq!(qq["lastErrorClass"], "unreachable");
        assert_eq!(qq["lastError"], "socket closed");

        status.attempt();
        status.connected();
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["state"], "up");
        assert_eq!(qq["attempts"], 2);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn retain_config_keeps_existing_and_adds_new_entries() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_connected("dsh", 100);
        let config = Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[harnesses]]
            id = "opencode"
            name = "OpenCode"

            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "bot"
            "#,
        )
        .expect("配置合法");

        registry.retain_config(&config);
        let snapshot = registry.snapshot(None);
        assert_eq!(
            entry(&snapshot, "dsh")["state"],
            "up",
            "仍在配置里的状态保留"
        );
        assert_eq!(entry(&snapshot, "opencode")["state"], "unknown");
        assert_eq!(entry(&snapshot, "qq")["state"], "unknown");
        assert_eq!(entry(&snapshot, "qq")["lastErrorClass"], "not_configured");
        assert_eq!(
            snapshot["transitions"].as_array().unwrap().len(),
            1,
            "配置增删不追加迁移环：只有连接成功那一条"
        );
        assert_eq!(registry.summary()["up"], 1);
        assert_eq!(registry.summary()["unknown"], 2);
    }

    #[test]
    fn retain_config_removes_dependencies_dropped_from_config() {
        let config = Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "bot"
            "#,
        )
        .expect("配置合法");
        let registry = DependencyRegistry::from_config(&config);
        registry.platform_connected("qq", 100);
        registry.harness_connected("dsh", 100);

        let config = Config::parse(
            r#"
            [[harnesses]]
            id = "opencode"
            name = "OpenCode"
            "#,
        )
        .expect("配置合法");
        registry.retain_config(&config);
        let snapshot = registry.snapshot(None);
        assert!(
            snapshot["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["id"] != "qq"),
            "被移除的平台条目直接删除：{snapshot}"
        );
        assert!(
            snapshot["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["id"] != "dsh"),
            "被移除的 harness 条目直接删除：{snapshot}"
        );
        assert_eq!(
            snapshot["transitions"].as_array().unwrap().len(),
            2,
            "删除不写迁移环"
        );
    }

    struct RecordingSink(Arc<Mutex<Vec<NoticeEvent>>>);

    impl NoticeSink for RecordingSink {
        fn notify(&self, event: NoticeEvent) {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
        }
    }

    #[test]
    fn state_edges_feed_the_notice_sink_but_attempts_do_not() {
        let registry = DependencyRegistry::from_config(&harness_config());
        let events = Arc::new(Mutex::new(Vec::new()));
        registry.set_notice_sink(Arc::new(RecordingSink(Arc::clone(&events))));

        registry.harness_attempt("dsh", 100);
        registry.harness_connected("dsh", 101);
        registry.harness_connected("dsh", 102);
        registry.harness_disconnected("dsh", "socket closed", 103);
        registry.harness_attempt("dsh", 104);

        let events = events.lock().expect("没被毒死");
        assert_eq!(events.len(), 2, "只有 unknown→up 与 up→down 两条边");
        match &events[0] {
            NoticeEvent::DependencyEdge {
                kind,
                id,
                from,
                to,
                since,
                attempts,
                ..
            } => {
                assert_eq!(kind, "harness");
                assert_eq!(id, "dsh");
                assert_eq!(from, "unknown");
                assert_eq!(to, "up");
                assert_eq!(*since, 101);
                assert_eq!(*attempts, 1);
            }
            other => panic!("应当是依赖边沿，收到 {other:?}"),
        }
        match &events[1] {
            NoticeEvent::DependencyEdge {
                from,
                to,
                class,
                detail,
                ..
            } => {
                assert_eq!(from, "up");
                assert_eq!(to, "down");
                assert_eq!(class.as_deref(), Some("closed"));
                assert_eq!(detail.as_deref(), Some("socket closed"));
            }
            other => panic!("应当是依赖边沿，收到 {other:?}"),
        }
    }

    #[test]
    fn failed_attempts_count_without_connecting() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_attempt("dsh", 100);
        registry.harness_attempt("dsh", 101);
        registry.harness_attempt("dsh", 102);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "unknown", "只尝试不算连上或断开");
        assert_eq!(dsh["neverConnected"], true);
        assert_eq!(dsh["attempts"], 3, "每次尝试都计数");
        assert_eq!(dsh["lastAttemptAt"], 102);
        assert!(snapshot["transitions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn connect_disconnect_and_reconnect_make_edges() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_attempt("dsh", 99);
        registry.harness_connected("dsh", 100);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "up");
        assert_eq!(dsh["lastOkAt"], 100);
        assert_eq!(dsh["neverConnected"], false);
        assert_eq!(dsh["attempts"], 1);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);

        registry.harness_attempt("dsh", 101);
        registry.harness_connected("dsh", 101);
        let snapshot = registry.snapshot(None);
        assert_eq!(
            snapshot["transitions"].as_array().unwrap().len(),
            1,
            "已经 up 时不重复打迁移日志"
        );

        registry.harness_disconnected("dsh", "socket closed", 102);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "down");
        assert_eq!(dsh["lastOkAt"], 101, "断开不抹掉 lastOkAt");
        assert_eq!(dsh["lastError"], "socket closed");
        assert_eq!(dsh["lastErrorClass"], "closed");
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 2);

        registry.harness_attempt("dsh", 103);
        registry.harness_connected("dsh", 103);
        let snapshot = registry.snapshot(Some(2));
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);
        assert_eq!(entry(&snapshot, "dsh")["state"], "up");
        assert_eq!(entry(&snapshot, "dsh")["attempts"], 3);
    }
}
