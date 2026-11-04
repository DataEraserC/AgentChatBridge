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

/// 启动宽限期。配置已登记、但在此期间没有任何观测的条目，才从 `unknown`
/// 迁到 `down(not_started)`；避免桥先于 harness 启动时误报。
const STARTUP_GRACE_MS: i64 = 30_000;

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

/// 平台的三层版本信息。三层不压成一个字符串：协议决定语义，实现决定排障对象，
/// 适配器版本决定「我们自己的哪一版在说话」。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlatformVersionInfo {
    /// 协议种类，如 `onebot11`。
    pub protocol_kind: String,
    /// 协议族名，如 `OneBot`。
    pub protocol_name: String,
    /// 平台自报的协议版本；取不到为 `None`。
    pub protocol_version: Option<String>,
    /// 平台实现名，如 `snowluma`；取不到为 `None`。
    pub implementation_name: Option<String>,
    /// 平台实现版本；取不到为 `None`。
    pub implementation_version: Option<String>,
    /// 适配器 crate 版本。
    pub adapter_version: String,
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
    /// 当前打开的连接数。harness 允许同一组会话开多条连接；只有最后一条关闭才 down。
    open_connections: u64,
    last_error: Option<String>,
    last_class: Option<String>,
    /// 账号是否在线。`None` = 没观测到，不猜。
    account_online: Option<bool>,
    /// `get_status.good`。`None` = 没观测到。
    account_good: Option<bool>,
    /// 三层版本信息。`None` = 还没采集。
    version: Option<PlatformVersionInfo>,
    version_observed_at: Option<i64>,
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
                open_connections: 0,
                last_error: None,
                last_class: None,
                account_online: None,
                account_good: None,
                version: None,
                version_observed_at: None,
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
                open_connections: 0,
                last_error: None,
                last_class: None,
                account_online: None,
                account_good: None,
                version: None,
                version_observed_at: None,
            });
        }
        // model 不在启动时预登记：没有轮次就没有观测，预先挂一堆 unknown 只会
        // 把 /healthz 的 unknown 计数撑大。首次 model_down/model_up 再 upsert。
        // `models` 集合仍用于裁剪已经存在、但新配置不再引用的 model 条目。
    }

    /// harness 建立一条连接：attempts +1，连接计数 +1；第一条连接把状态置 up。
    ///
    /// 同一 harness 允许同时开多条连接（协议允许多条连接代表同一组会话）。
    /// 只要还有连接，状态就保持 up；最后一条关闭才 down。
    pub fn harness_connection_opened(&self, id: &str, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, DependencyKind::Harness, id, now);
        let key = Key {
            kind: DependencyKind::Harness,
            id: id.to_owned(),
        };
        {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            entry.attempts = entry.attempts.saturating_add(1);
            entry.last_attempt_at = Some(now);
            entry.open_connections = entry.open_connections.saturating_add(1);
        }
        let notice = mark_up_locked(&mut inner, &key, now);
        drop(inner);
        if let Some(event) = notice {
            self.emit(event);
        }
    }

    /// harness 关闭一条连接：只有最后一条关闭才 down；否则仍保持 up。
    pub fn harness_connection_closed(&self, id: &str, detail: &str, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, DependencyKind::Harness, id, now);
        let key = Key {
            kind: DependencyKind::Harness,
            id: id.to_owned(),
        };
        let last = {
            let entry = inner.entries.get_mut(&key).expect("刚 upsert 过");
            entry.open_connections = entry.open_connections.saturating_sub(1);
            entry.open_connections == 0
        };
        let notice = if last {
            mark_down_locked(&mut inner, &key, "closed", detail, now)
        } else {
            None
        };
        drop(inner);
        if let Some(event) = notice {
            self.emit(event);
        }
    }

    /// 一轮成功：harness 可用。不改变连接计数（这条轮次可能来自已在统计中的连接）。
    pub fn harness_turn_ok(&self, id: &str, now: i64) {
        self.mark_up(DependencyKind::Harness, id, now);
    }

    /// 记一次平台连接尝试（不改变状态）。
    pub fn platform_attempt(&self, id: &str, now: i64) {
        self.mark_attempt(DependencyKind::Platform, id, now);
    }

    /// 平台适配器上报连接成功（第②步使用）。
    pub fn platform_connected(&self, id: &str, now: i64) {
        self.clear_account_observation(DependencyKind::Platform, id);
        self.mark_up(DependencyKind::Platform, id, now);
    }

    /// 平台适配器上报断开（第②步使用）。
    pub fn platform_disconnected(&self, id: &str, class: &str, detail: &str, now: i64) {
        self.clear_account_observation(DependencyKind::Platform, id);
        self.mark_down(DependencyKind::Platform, id, class, detail, now);
    }

    /// 账号在线状态。连接建立不等于账号在线：SnowLuma 能在 WS 还通着的时候
    /// 推送 `bot_status offline`，所以这条信号单独上报。
    pub fn platform_account(
        &self,
        id: &str,
        online: bool,
        good: Option<bool>,
        detail: &str,
        now: i64,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, DependencyKind::Platform, id, now);
        let key = Key {
            kind: DependencyKind::Platform,
            id: id.to_owned(),
        };
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.account_online = Some(online);
            entry.account_good = good;
        }
        let notice = if online {
            mark_up_locked(&mut inner, &key, now)
        } else {
            mark_down_locked(&mut inner, &key, "account_offline", detail, now)
        };
        drop(inner);
        if let Some(event) = notice {
            self.emit(event);
        }
    }

    /// 记录三层版本信息。取不到的值保持 `None`，序列化时统一显示 `unknown`。
    pub fn platform_version(&self, id: &str, version: PlatformVersionInfo, now: i64) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        upsert(&mut inner, DependencyKind::Platform, id, now);
        let key = Key {
            kind: DependencyKind::Platform,
            id: id.to_owned(),
        };
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.version = Some(version);
            entry.version_observed_at = Some(now);
        }
    }

    fn clear_account_observation(&self, kind: DependencyKind, id: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = Key {
            kind,
            id: id.to_owned(),
        };
        if let Some(entry) = inner.entries.get_mut(&key) {
            entry.account_online = None;
            entry.account_good = None;
        }
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

    /// 当前是否 down。turn 失败抑制用它判断「同一依赖故障是否已经发生过」。
    pub fn is_down(&self, kind: DependencyKind, id: &str) -> bool {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .entries
            .get(&Key {
                kind,
                id: id.to_owned(),
            })
            .is_some_and(|entry| entry.state == State::Down)
    }

    /// 启动宽限期检查：把从未出现过的 harness 从 `unknown` 迁到 `down(not_started)`。
    ///
    /// 只处理 harness：平台是否启动只有适配器知道，core 不能凭「没观测到」就判
    /// 平台 down，否则不经过 `AdapterStatus` 的载体（如旧 in-process 路径）会被误判。
    /// 返回本次产生的迁移数。
    pub fn tick(&self, now: i64) -> usize {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let candidates: Vec<Key> = inner
            .entries
            .iter()
            .filter(|(key, entry)| {
                key.kind == DependencyKind::Harness
                    && entry.state == State::Unknown
                    && now.saturating_sub(entry.since) >= STARTUP_GRACE_MS
            })
            .map(|(key, _)| key.clone())
            .collect();
        let mut notices = Vec::new();
        let mut changed = 0;
        for key in candidates {
            let (class, detail) = {
                let entry = inner.entries.get(&key).expect("候选来自 entries");
                if entry.attempts == 0 {
                    (
                        "not_started",
                        "no connection attempt within startup grace period",
                    )
                } else {
                    (
                        "unreachable",
                        "connection attempt did not complete within startup grace period",
                    )
                }
            };
            if let Some(notice) = mark_down_locked(&mut inner, &key, class, detail, now) {
                notices.push(notice);
                changed += 1;
            }
        }
        drop(inner);
        for notice in notices {
            self.emit(notice);
        }
        changed
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
        let notice = mark_up_locked(&mut inner, &key, now);
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
        let notice = mark_down_locked(&mut inner, &key, class, detail, now);
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

    /// `/healthz` 的逐依赖视图。
    ///
    /// 计数、`status`、`seq` 与完整条目一起返回；条目数量由配置决定（harness +
    /// connection + 已观测 model），不随消息增长。读快照只锁依赖表，不碰适配器。
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
        let status = if down > 0 { "degraded" } else { "ok" };
        json!({
            "status": status,
            "up": up,
            "down": down,
            "unknown": unknown,
            "seq": inner.seq,
            "items": inner.entries.values().map(entry_json).collect::<Vec<_>>(),
        })
    }

    /// 只取 platform 类的逐连接状态（`/api/plugins` 的补充视图）。
    pub fn platform_items(&self) -> Vec<Value> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .entries
            .values()
            .filter(|entry| entry.key.kind == DependencyKind::Platform)
            .map(entry_json)
            .collect()
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
        open_connections: 0,
        last_error: None,
        last_class: None,
        account_online: None,
        account_good: None,
        version: None,
        version_observed_at: None,
    });
}

/// 把条目置为 `up`，必要时写迁移并返回带外事件。调用方持有 `inner` 锁。
fn mark_up_locked(inner: &mut Inner, key: &Key, now: i64) -> Option<NoticeEvent> {
    let (from, notice) = {
        let entry = inner.entries.get_mut(key).expect("调用前已 upsert");
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
        push_transition(inner, key.clone(), from, State::Up, None, None, now);
    }
    notice
}

/// 把条目置为 `down`，必要时写迁移并返回带外事件。调用方持有 `inner` 锁。
fn mark_down_locked(
    inner: &mut Inner,
    key: &Key,
    class: &str,
    detail: &str,
    now: i64,
) -> Option<NoticeEvent> {
    let (from, notice) = {
        let entry = inner.entries.get_mut(key).expect("调用前已 upsert");
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
            inner,
            key.clone(),
            from,
            State::Down,
            Some(class.to_owned()),
            Some(detail.to_owned()),
            now,
        );
    }
    notice
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
        "accountOnline": entry.account_online,
        "accountGood": entry.account_good,
        "version": entry.version.as_ref().map(version_json),
        "versionObservedAt": entry.version_observed_at,
    })
}

fn version_json(version: &PlatformVersionInfo) -> Value {
    let source = if version.protocol_version.is_some()
        || version.implementation_name.is_some()
        || version.implementation_version.is_some()
    {
        "get_version_info"
    } else {
        "unknown"
    };
    json!({
        "protocol": {
            "kind": version.protocol_kind,
            "name": version.protocol_name,
            "version": version.protocol_version.as_deref().unwrap_or("unknown"),
            "source": source,
        },
        "impl": {
            "name": version.implementation_name.as_deref().unwrap_or("unknown"),
            "version": version.implementation_version.as_deref().unwrap_or("unknown"),
            "source": source,
        },
        "adapter": {
            "version": version.adapter_version,
            "source": "crate",
        },
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
    fn platform_starts_unknown_then_connect_disconnect_reconnect() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["kind"], "platform");
        assert_eq!(qq["state"], "unknown", "配置已登记，但还没有观测到连接");
        assert_eq!(qq["neverConnected"], true);
        assert_eq!(qq["lastError"], Value::Null, "未连上不等于有错误");
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
    fn platform_never_connected_then_failure_is_down_not_unknown() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let status = crate::adapter::AdapterStatus::new("qq", registry.clone());
        status.attempt();
        status.disconnected("unreachable", "connect ECONNREFUSED");

        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["state"], "down");
        assert_eq!(
            qq["neverConnected"], true,
            "从未连上仍由 lastOkAt=null 表达"
        );
        assert_eq!(qq["lastOkAt"], Value::Null);
        assert_eq!(qq["lastErrorClass"], "unreachable");
        assert_eq!(qq["attempts"], 1);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);

        // 平台条目不由 core 的宽限计时器判 down：适配器没上报时保持 unknown。
        assert_eq!(registry.tick(now_millis() + 60_000), 0);
    }

    #[test]
    fn carrier_status_contract_covers_up_down_and_unknown() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["state"], "unknown", "未上报的载体保持 unknown");

        let status = crate::adapter::AdapterStatus::new("qq", registry.clone());
        status.loaded();
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["state"], "up", "载体加载/初始化成功 -> up");
        assert!(!qq["lastOkAt"].is_null(), "up 要带 lastOkAt");

        status.failed("unavailable", "dlopen failed");
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["state"], "down", "初始化/运行失败 -> down");
        assert_eq!(qq["lastErrorClass"], "unavailable");
        assert_eq!(qq["lastError"], "dlopen failed");
    }

    #[test]
    fn harness_unknown_becomes_not_started_after_grace() {
        let registry = DependencyRegistry::from_config(&harness_config());
        let snapshot = registry.snapshot(None);
        let since = snapshot["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "dsh")
            .and_then(|item| item["since"].as_i64())
            .expect("有 since");

        assert_eq!(registry.tick(since + 29_000), 0, "宽限期内保持 unknown");
        assert_eq!(entry(&registry.snapshot(None), "dsh")["state"], "unknown");

        assert_eq!(registry.tick(since + 30_001), 1, "宽限期后转 down");
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "down");
        assert_eq!(dsh["lastErrorClass"], "not_started");
        assert_eq!(dsh["neverConnected"], true);
        assert_eq!(dsh["lastOkAt"], Value::Null);
        assert_eq!(dsh["attempts"], 0);
        assert_eq!(registry.tick(since + 60_000), 0, "已经 down 不重复迁移");
        assert_eq!(registry.summary()["status"], "degraded");
    }

    #[test]
    fn retain_config_keeps_existing_and_adds_new_entries() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_connection_opened("dsh", 100);
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
        assert_eq!(entry(&snapshot, "qq")["lastError"], Value::Null);
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
        registry.harness_connection_opened("dsh", 100);

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

        registry.harness_connection_opened("dsh", 100);
        registry.harness_connection_opened("dsh", 101);
        registry.harness_connection_closed("dsh", "socket closed", 102);
        registry.harness_connection_closed("dsh", "socket closed", 103);

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
                assert_eq!(*since, 100);
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
                attempts,
                ..
            } => {
                assert_eq!(from, "up");
                assert_eq!(to, "down");
                assert_eq!(class.as_deref(), Some("closed"));
                assert_eq!(detail.as_deref(), Some("socket closed"));
                assert_eq!(*attempts, 2, "两条连接都计入 attempts");
            }
            other => panic!("应当是依赖边沿，收到 {other:?}"),
        }
    }

    #[test]
    fn multiple_harness_connections_only_go_down_after_the_last_closes() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_connection_opened("dsh", 100);
        registry.harness_connection_opened("dsh", 101);

        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "up");
        assert_eq!(dsh["attempts"], 2);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);

        registry.harness_connection_closed("dsh", "one of two closed", 102);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "up", "还有一条连接在，不能判 down");
        assert_eq!(dsh["lastOkAt"], 101);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);

        registry.harness_connection_closed("dsh", "last closed", 103);
        let snapshot = registry.snapshot(None);
        assert_eq!(entry(&snapshot, "dsh")["state"], "down");
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn failed_attempts_count_without_connecting() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let status = crate::adapter::AdapterStatus::new("qq", registry.clone());
        status.attempt();
        status.attempt();
        status.attempt();
        let snapshot = registry.snapshot(None);
        let qq = entry(&snapshot, "qq");
        assert_eq!(qq["state"], "unknown", "只尝试不算连上或断开");
        assert_eq!(qq["neverConnected"], true);
        assert_eq!(qq["attempts"], 3, "每次尝试都计数");
        assert!(qq["lastAttemptAt"].as_i64().is_some());
        assert!(snapshot["transitions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn connect_disconnect_and_reconnect_make_edges() {
        let registry = DependencyRegistry::from_config(&harness_config());
        registry.harness_connection_opened("dsh", 99);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "up");
        assert_eq!(dsh["lastOkAt"], 99);
        assert_eq!(dsh["neverConnected"], false);
        assert_eq!(dsh["attempts"], 1);
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);

        registry.harness_connection_opened("dsh", 100);
        let snapshot = registry.snapshot(None);
        assert_eq!(
            snapshot["transitions"].as_array().unwrap().len(),
            1,
            "已经 up 时不重复打迁移日志"
        );

        // 两条连接，先关一条仍 up；关掉最后一条才 down。
        registry.harness_connection_closed("dsh", "one of two closed", 101);
        assert_eq!(
            entry(&registry.snapshot(None), "dsh")["state"],
            "up",
            "还有连接时不能判 down"
        );
        registry.harness_connection_closed("dsh", "socket closed", 102);
        let snapshot = registry.snapshot(None);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "down");
        assert_eq!(dsh["lastOkAt"], 100, "断开不抹掉 lastOkAt");
        assert_eq!(dsh["lastError"], "socket closed");
        assert_eq!(dsh["lastErrorClass"], "closed");
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 2);

        registry.harness_connection_opened("dsh", 103);
        let snapshot = registry.snapshot(Some(2));
        assert_eq!(snapshot["transitions"].as_array().unwrap().len(), 1);
        let dsh = entry(&snapshot, "dsh");
        assert_eq!(dsh["state"], "up");
        assert_eq!(dsh["since"], 103, "重新连上后 since 重置");
        assert_eq!(dsh["lastOkAt"], 103);
        assert_eq!(dsh["attempts"], 3);
    }

    #[test]
    fn models_are_registered_only_after_observation() {
        let config = Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[personas]]
            id = "p"
            name = "P"
            text = "x"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"
            model = "test/model"
            "#,
        )
        .expect("配置合法");
        let registry = DependencyRegistry::from_config(&config);
        let snapshot = registry.snapshot(None);
        assert!(
            snapshot["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["kind"] != "model"),
            "没有轮次就没有模型观测：{snapshot}"
        );
        assert_eq!(registry.summary()["unknown"], 1, "只有 harness 是 unknown");

        registry.model_down("test/model", "model_unavailable", "gateway refused", 100);
        let snapshot = registry.snapshot(None);
        let model = entry(&snapshot, "test/model");
        assert_eq!(model["kind"], "model");
        assert_eq!(model["state"], "down");
        assert_eq!(model["lastErrorClass"], "model_unavailable");

        registry.model_up("test/model", 101);
        let snapshot = registry.snapshot(None);
        assert_eq!(entry(&snapshot, "test/model")["state"], "up");
    }

    #[test]
    fn retain_config_prunes_observed_model_no_longer_referenced() {
        let config = Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[personas]]
            id = "p"
            name = "P"
            text = "x"

            [[sessions]]
            id = "main"
            harness = "dsh"
            persona = "p"
            token = "t-main"
            model = "test/model"
            "#,
        )
        .expect("配置合法");
        let registry = DependencyRegistry::from_config(&config);
        registry.model_down("test/model", "model_unavailable", "gateway refused", 100);

        let next = Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"
            "#,
        )
        .expect("新配置合法");
        registry.retain_config(&next);
        let snapshot = registry.snapshot(None);
        assert!(
            snapshot["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["kind"] != "model"),
            "配置不再引用的 model 条目要删除：{snapshot}"
        );
    }

    #[test]
    fn ops_payload_shape_for_healthz_and_dependencies() {
        let config = Config::parse(
            r#"
            [[harnesses]]
            id = "dsh"
            name = "DSH"

            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:53001"
            account = "bot"
            "#,
        )
        .expect("配置合法");
        let registry = DependencyRegistry::from_config(&config);
        registry.harness_connection_opened("dsh", 1_793_000_000_000);
        registry.platform_attempt("qq", 1_793_000_001_000);
        registry.platform_disconnected(
            "qq",
            "unreachable",
            "connect ECONNREFUSED 127.0.0.1:53001",
            1_793_000_001_000,
        );
        registry.model_down(
            "new-api/1AgentRouter/gpt-5.6-sol",
            "model_unavailable",
            "connect ECONNREFUSED 127.0.0.1:3080",
            1_793_000_002_000,
        );

        let healthz_dependencies = registry.summary();
        let dependencies = registry.snapshot(None);
        println!("HEALTHZ dependencies = {healthz_dependencies}");
        println!("API /api/dependencies = {dependencies}");

        assert_eq!(healthz_dependencies["status"], "degraded");
        assert_eq!(healthz_dependencies["items"].as_array().unwrap().len(), 3);
        assert_eq!(dependencies["dependencies"].as_array().unwrap().len(), 3);
        assert_eq!(dependencies["transitions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn platform_account_is_separate_from_connection_state() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let status = crate::adapter::AdapterStatus::new("qq", registry.clone());
        status.attempt();
        status.connected();
        assert_eq!(
            entry(&registry.snapshot(None), "qq")["accountOnline"],
            Value::Null
        );

        status.account(true, Some(true), "get_status");
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["state"], "up");
        assert_eq!(qq["accountOnline"], true);
        assert_eq!(qq["accountGood"], true);

        status.account(false, None, "bot_status/offline");
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["accountOnline"], false);
        assert_eq!(qq["lastErrorClass"], "account_offline");
        assert_eq!(qq["lastError"], "bot_status/offline");
    }

    #[test]
    fn platform_version_records_three_layers_and_unknown_slots() {
        let registry = DependencyRegistry::from_config(&platform_config());
        registry.platform_version(
            "qq",
            PlatformVersionInfo {
                protocol_kind: "onebot11".to_owned(),
                protocol_name: "OneBot".to_owned(),
                protocol_version: Some("v11".to_owned()),
                implementation_name: Some("snowluma".to_owned()),
                implementation_version: Some("2.4.0".to_owned()),
                adapter_version: "0.1.0".to_owned(),
            },
            100,
        );
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["version"]["protocol"]["kind"], "onebot11");
        assert_eq!(qq["version"]["protocol"]["version"], "v11");
        assert_eq!(qq["version"]["protocol"]["source"], "get_version_info");
        assert_eq!(qq["version"]["impl"]["name"], "snowluma");
        assert_eq!(qq["version"]["impl"]["version"], "2.4.0");
        assert_eq!(qq["version"]["adapter"]["version"], "0.1.0");
        assert_eq!(qq["version"]["adapter"]["source"], "crate");
        assert_eq!(qq["versionObservedAt"], 100);

        registry.platform_version(
            "qq",
            PlatformVersionInfo {
                protocol_kind: "onebot11".to_owned(),
                protocol_name: "OneBot".to_owned(),
                protocol_version: None,
                implementation_name: None,
                implementation_version: None,
                adapter_version: "0.1.0".to_owned(),
            },
            200,
        );
        let qq = entry(&registry.snapshot(None), "qq");
        assert_eq!(qq["version"]["protocol"]["version"], "unknown");
        assert_eq!(qq["version"]["protocol"]["source"], "unknown");
        assert_eq!(qq["version"]["impl"]["name"], "unknown");
        assert_eq!(qq["version"]["impl"]["version"], "unknown");
    }

    #[test]
    fn summary_exposes_items_and_degrades_on_down() {
        let registry = DependencyRegistry::from_config(&platform_config());
        let summary = registry.summary();
        assert_eq!(summary["status"], "ok");
        assert_eq!(summary["unknown"], 1);
        assert_eq!(
            summary["items"].as_array().unwrap()[0]["id"],
            "qq",
            "summary 必须逐依赖，而不只是计数：{summary}"
        );

        let status = crate::adapter::AdapterStatus::new("qq", registry.clone());
        status.attempt();
        status.disconnected("unreachable", "connection refused");
        let summary = registry.summary();
        assert_eq!(summary["status"], "degraded");
        assert_eq!(summary["down"], 1);
        assert_eq!(
            summary["items"].as_array().unwrap()[0]["lastErrorClass"],
            "unreachable"
        );
    }
}
