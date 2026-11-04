//! 全局依赖状态表（connectivity 第①步）。
//!
//! 只记录「这个组件现在可不可用」以及最后一次状态迁移，不做通知、不探测远端。
//! 当前只接 harness 连接存在性；平台适配器在下一步通过 `mark_platform` 上报。

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::config::Config;

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
}

impl Default for DependencyRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: BTreeMap::new(),
                transitions: VecDeque::new(),
                seq: 0,
            })),
        }
    }
}

impl DependencyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按当前配置登记 harness。平台/模型在后续步骤再加。
    pub fn from_config(config: &Config) -> Self {
        let registry = Self::default();
        let now = now_millis();
        let mut inner = registry
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for harness in &config.harnesses {
            inner
                .entries
                .entry(Key {
                    kind: DependencyKind::Harness,
                    id: harness.id.clone(),
                })
                .or_insert(Entry {
                    key: Key {
                        kind: DependencyKind::Harness,
                        id: harness.id.clone(),
                    },
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
            inner
                .entries
                .entry(Key {
                    kind: DependencyKind::Platform,
                    id: connection.id.clone(),
                })
                .or_insert(Entry {
                    key: Key {
                        kind: DependencyKind::Platform,
                        id: connection.id.clone(),
                    },
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
        drop(inner);
        registry
    }

    /// harness 握手成功：up，记录 lastOkAt / attempts。
    pub fn harness_connected(&self, id: &str, now: i64) {
        self.mark_up(DependencyKind::Harness, id, now);
    }

    /// harness 连接断开：down，class=closed。
    pub fn harness_disconnected(&self, id: &str, detail: &str, now: i64) {
        self.mark_down(DependencyKind::Harness, id, "closed", detail, now);
    }

    /// 平台适配器上报连接成功（第②步使用）。
    pub fn platform_connected(&self, id: &str, now: i64) {
        self.mark_up(DependencyKind::Platform, id, now);
    }

    /// 平台适配器上报断开（第②步使用）。
    pub fn platform_disconnected(&self, id: &str, class: &str, detail: &str, now: i64) {
        self.mark_down(DependencyKind::Platform, id, class, detail, now);
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
        let from = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            let from = entry.state;
            entry.last_error = Some(detail.to_owned());
            entry.last_class = Some(class.to_owned());
            if from != State::Unknown {
                entry.state = State::Unknown;
                entry.since = now;
            }
            from
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
        let from = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            let from = entry.state;
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_attempt_at = Some(now);
            entry.last_ok_at = Some(now);
            entry.last_error = None;
            entry.last_class = None;
            if from != State::Up {
                entry.state = State::Up;
                entry.since = now;
            }
            from
        };
        if from != State::Up {
            push_transition(&mut inner, key, from, State::Up, None, None, now);
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
        let from = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            let from = entry.state;
            entry.last_error = Some(detail.to_owned());
            entry.last_class = Some(class.to_owned());
            if from != State::Down {
                entry.state = State::Down;
                entry.since = now;
            }
            from
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

        status.connected();
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["state"], "up");
        assert_eq!(qq["attempts"], 2);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn connect_disconnect_and_reconnect_make_edges() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_connected("dsh", 100);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "up");
        assert_eq!(dsh["lastOkAt"], 100);
        assert_eq!(dsh["neverConnected"], false);
        assert_eq!(dsh["attempts"], 1);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);

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

        registry.harness_connected("dsh", 103);
        let snapshot = registry.snapshot(Some(2));
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);
        assert_eq!(entry(&snapshot, "dsh")["state"], "up");
        assert_eq!(entry(&snapshot, "dsh")["attempts"], 3);
    }
}
