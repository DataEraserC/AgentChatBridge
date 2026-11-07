//! RecordObserver：每条入库记录都被观察一次；观察者不能影响调度。
//!
//! 直接注册 root observer（不经过 manager），覆盖 append 之后的调用点。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, Record, RecordDraft,
    RecordObserver, RoomId, Sender, Sent, Transcript,
};

fn config() -> String {
    r#"
    [[connections]]
    id = "qq"
    platform = "qq"
    kind = "onebot11"
    url = "ws://127.0.0.1:3001"
    account = "bot"

    [[personas]]
    id = "p"
    name = "人格"
    text = "你是桥。"

    [[harnesses]]
    id = "dsh"
    name = "DSH"

    [[sessions]]
    id = "main"
    harness = "dsh"
    persona = "p"
    token = { secret = "t-main" }

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/1"
    "#
    .to_owned()
}

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: 1,
        })
    }
}

struct CountingObserver {
    seen: AtomicUsize,
    texts: Mutex<Vec<String>>,
}

impl RecordObserver for CountingObserver {
    fn observe(&self, record: &Record) {
        self.seen.fetch_add(1, Ordering::SeqCst);
        self.texts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(
                record
                    .elements
                    .iter()
                    .filter_map(|element| match element {
                        Element::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            );
    }
}

fn bridge() -> (Arc<Mutex<Bridge<MemoryStore>>>, Arc<CountingObserver>) {
    let observer = Arc::new(CountingObserver {
        seen: AtomicUsize::new(0),
        texts: Mutex::new(Vec::new()),
    });
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_observer(
            ProviderDescriptor::new(
                ProviderId::parse("test.observer").expect("合法"),
                ExtensionPoint::Observer,
                PLUGIN_API_VERSION,
                vec![Capability::ObservedRecord],
            ),
            observer.clone(),
        )
        .expect("注册观察者");
    let bridge = Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse_fixture(&config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )));
    (bridge, observer)
}

fn message(text: &str) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse("qq:group/1").expect("房间"),
        Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        },
        1,
        vec![Element::text(text)],
        false,
    )
}

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn every_stored_record_is_observed_once() {
    let (bridge, observer) = bridge();
    lock(&bridge).ingest(message("一")).expect("入库");
    lock(&bridge).ingest(message("二")).expect("入库");

    assert_eq!(observer.seen.load(Ordering::SeqCst), 2);
    assert_eq!(
        observer
            .texts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_slice(),
        ["一", "二"]
    );
}

#[test]
fn observer_does_not_change_scheduling() {
    let (bridge, observer) = bridge();
    lock(&bridge).ingest(message("在吗")).expect("入库");
    assert_eq!(observer.seen.load(Ordering::SeqCst), 1);
    // 没有路由、没有唤醒策略命中，观察者也不能替 core 开轮。
    assert!(lock(&bridge).poll_turn().expect("能查询").is_none());
    assert_eq!(
        lock(&bridge)
            .store()
            .latest(&RoomId::parse("qq:group/1").expect("房间"))
            .expect("读最新编号"),
        Some(1)
    );
}
