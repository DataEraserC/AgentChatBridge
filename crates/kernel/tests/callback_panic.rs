//! 回调 panic 后的不变量：跳过/Pass/工具错误，锁不中毒，后续调用可用。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ErrorCode, ExtensionPoint, Ingest, MemoryStore, Outbound,
    OutboundError, PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, Record,
    RecordDraft, RecordObserver, RoomId, RouteContext, RouteDecision, RoutePolicy, RouteProvider,
    Sender, Sent, ToolCallContext, ToolError, ToolProvider, ToolSpec,
};
use serde_json::{Value, json};

fn config(route_provider: Option<&str>) -> String {
    let routes = route_provider
        .map(|provider| {
            format!(
                "[[connections.routes]]\nprovider = {provider:?}\n\n[[connections.routes]]\nprovider = \"test.route_ok\"\n"
            )
        })
        .unwrap_or_default();
    format!(
        r#"
    [[connections]]
    id = "qq"
    platform = "qq"
    kind = "onebot11"
    url = "ws://127.0.0.1:3001"
    account = "bot"

    {routes}
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
    token = {{ secret = "t-main" }}

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/1"
    "#
    )
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

fn draft() -> RecordDraft {
    RecordDraft::message(
        RoomId::parse("qq:group/1").expect("房间"),
        Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        },
        1,
        vec![Element::text("在吗")],
        true,
    )
}

fn bridge(plugins: PluginManager, route_provider: Option<&str>) -> Arc<Mutex<Bridge<MemoryStore>>> {
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse_fixture(&config(route_provider)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )))
}

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct PanicObserver {
    calls: Arc<AtomicUsize>,
}

impl RecordObserver for PanicObserver {
    fn observe(&self, _record: &Record) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("observer boom");
    }
}

struct CountObserver {
    calls: Arc<AtomicUsize>,
}

impl RecordObserver for CountObserver {
    fn observe(&self, _record: &Record) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

struct PanicRoute {
    calls: Arc<AtomicUsize>,
}

impl RouteProvider for PanicRoute {
    fn decide(&self, _options: &RoutePolicy, _context: &RouteContext<'_>) -> RouteDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("route boom");
    }
}

struct HandleRoute;

impl RouteProvider for HandleRoute {
    fn decide(&self, _options: &RoutePolicy, _context: &RouteContext<'_>) -> RouteDecision {
        RouteDecision::Handle {
            store: true,
            wake: true,
            actions: Vec::new(),
        }
    }
}

struct PanicTool;

impl ToolProvider for PanicTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "panic_tool".into(),
            summary: "总是 panic".into(),
            schema: json!({"type": "object"}),
        }]
    }

    fn call(
        &self,
        _context: &ToolCallContext<'_>,
        _tool: &str,
        _args: Value,
    ) -> Result<Value, ToolError> {
        panic!("tool boom");
    }
}

#[test]
fn observer_panic_is_skipped_and_wake_continues() {
    let panics = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(AtomicUsize::new(0));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_observer(
            ProviderDescriptor::new(
                ProviderId::parse("test.a_panic").expect("合法"),
                ExtensionPoint::Observer,
                PLUGIN_API_VERSION,
                vec![Capability::ObservedRecord],
            ),
            Arc::new(PanicObserver {
                calls: panics.clone(),
            }),
        )
        .expect("注册观察者");
    plugins
        .register_observer(
            ProviderDescriptor::new(
                ProviderId::parse("test.z_count").expect("合法"),
                ExtensionPoint::Observer,
                PLUGIN_API_VERSION,
                vec![Capability::ObservedRecord],
            ),
            Arc::new(CountObserver {
                calls: counts.clone(),
            }),
        )
        .expect("注册观察者");
    let bridge = bridge(plugins, None);

    let first = lock(&bridge).ingest(draft()).expect("panic 后仍要入库");
    assert!(
        matches!(&first, Ingest::Recorded { woke, .. } if !woke.is_empty()),
        "panic 不能挡住唤醒：{first:?}"
    );
    assert_eq!(panics.load(Ordering::SeqCst), 1);
    assert_eq!(counts.load(Ordering::SeqCst), 1, "其它观察者要继续执行");
    assert_eq!(lock(&bridge).drain_snapshot()["pending_route_actions"], 0);

    let ticket = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;
    lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}))
        .expect("锁未被毒化");
    lock(&bridge).end_turn(&ticket, None).expect("收轮");

    // 同一个坏观察者再次被调用：仍然只跳过它自己，不留下中毒状态。
    let second = lock(&bridge).ingest(draft()).expect("再次入库");
    assert!(matches!(second, Ingest::Recorded { .. }));
    assert_eq!(panics.load(Ordering::SeqCst), 2);
    assert_eq!(counts.load(Ordering::SeqCst), 2);
}

#[test]
fn route_panic_is_treated_as_pass_without_partial_pending_state() {
    let panics = Arc::new(AtomicUsize::new(0));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.route_panic").expect("合法"),
                ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision],
            ),
            Arc::new(PanicRoute {
                calls: panics.clone(),
            }),
        )
        .expect("注册路由");
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.route_ok").expect("合法"),
                ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision],
            ),
            Arc::new(HandleRoute),
        )
        .expect("注册路由");
    let bridge = bridge(plugins, Some("test.route_panic"));

    let first = lock(&bridge)
        .ingest(draft())
        .expect("路由 panic 不能丢消息");
    assert!(
        matches!(&first, Ingest::Handled { stored: Some(_), woke } if !woke.is_empty()),
        "panic 按 Pass 后，下一个路由/唤醒要继续：{first:?}"
    );
    assert_eq!(panics.load(Ordering::SeqCst), 1);
    assert_eq!(
        lock(&bridge).drain_snapshot()["pending_route_actions"],
        0,
        "panic 不能留下半完成的 pending 动作"
    );

    let ticket = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;
    lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}))
        .expect("锁未被毒化");
    lock(&bridge).end_turn(&ticket, None).expect("收轮");

    let second = lock(&bridge).ingest(draft()).expect("再次入库");
    assert!(matches!(
        second,
        Ingest::Handled {
            stored: Some(_),
            ..
        }
    ));
    assert_eq!(panics.load(Ordering::SeqCst), 2);
}

#[test]
fn tool_panic_returns_error_and_keeps_turn_and_index_usable() {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_tools(
            ProviderDescriptor::new(
                ProviderId::parse("test.tools").expect("合法"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            Arc::new(PanicTool),
        )
        .expect("注册工具");
    let bridge = bridge(plugins, None);

    lock(&bridge).ingest(draft()).expect("入库");
    let ticket = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;

    let index = lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("工具索引仍可用");
    assert_eq!(index["total"], 1);

    let error = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "panic_tool",
                "arguments": {},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect_err("panic 要变成工具错误");
    assert_eq!(error.code, ErrorCode::Internal);
    assert!(
        error.message.contains("[tool_panic]") && error.message.contains("tool boom"),
        "错误要能区分插件 panic：{}",
        error.message
    );

    // 同一轮里继续用其它工具，然后同一个坏工具再调用一次；都不能卡死。
    lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}))
        .expect("其它工具照常");
    let again = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "panic_tool",
                "arguments": {},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect_err("再次 panic 仍按错误返回");
    assert_eq!(again.code, ErrorCode::Internal);
    assert_eq!(lock(&bridge).drain_snapshot()["pending_route_actions"], 0);

    lock(&bridge).end_turn(&ticket, None).expect("收轮");
    // 收轮后的下一轮仍然正常，说明 panic 没有留下坏状态。
    lock(&bridge).ingest(draft()).expect("下一轮入库");
    let next = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;
    lock(&bridge)
        .tool_call(&next, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("下一轮工具索引仍可用");
}
