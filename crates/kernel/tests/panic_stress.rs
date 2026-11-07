//! 连续回调 panic 的压力验证：pending 不增长、内存不泄漏、状态仍可用。
//!
//! 这是独立测试二进制，避免其它测试并行分配干扰 RSS 基线。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, Ingest, MemoryStore, Outbound,
    OutboundError, PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, Record,
    RecordDraft, RecordObserver, RoomId, RouteContext, RouteDecision, RoutePolicy, RouteProvider,
    Sender, Sent, ToolCallContext, ToolError, ToolProvider, ToolSpec,
};
use serde_json::{Value, json};

const ITERATIONS: usize = 200;
const MAX_RSS_GROWTH_KB: u64 = 16 * 1024;

const CONFIG: &str = r#"
[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "bot"

[[connections.routes]]
provider = "test.route_panic"

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
"#;

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "stress".to_owned(),
            at: 1,
        })
    }
}

struct PanicObserver;

impl RecordObserver for PanicObserver {
    fn observe(&self, _record: &Record) {
        panic!("observer stress panic");
    }
}

struct PanicRoute;

impl RouteProvider for PanicRoute {
    fn decide(&self, _options: &RoutePolicy, _context: &RouteContext<'_>) -> RouteDecision {
        panic!("route stress panic");
    }
}

struct PanicTool;

impl ToolProvider for PanicTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "stress_panic".into(),
            summary: "压力测试用：总是 panic".into(),
            schema: json!({"type": "object"}),
        }]
    }

    fn call(
        &self,
        _context: &ToolCallContext<'_>,
        _tool: &str,
        _args: Value,
    ) -> Result<Value, ToolError> {
        panic!("tool stress panic");
    }
}

fn rss_kb() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
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

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn one_cycle(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) {
    let ingest = lock(bridge).ingest(draft()).expect("panic 后仍要入库");
    assert!(
        matches!(ingest, Ingest::Recorded { .. }),
        "route panic 按 Pass 后应当继续入库"
    );
    assert_eq!(lock(bridge).drain_snapshot()["pending_route_actions"], 0);

    let ticket = lock(bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;
    let error = lock(bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "stress_panic",
                "arguments": {},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect_err("工具 panic 要返回错误");
    assert!(
        error.message.contains("[tool_panic]"),
        "错误要能区分插件 panic：{}",
        error.message
    );
    lock(bridge)
        .tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}))
        .expect("锁未被毒化，其它工具可用");
    lock(bridge).end_turn(&ticket, None).expect("收轮");
}

#[test]
fn repeated_panics_do_not_leak_pending_or_state() {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_observer(
            ProviderDescriptor::new(
                ProviderId::parse("test.a_panic").expect("合法"),
                ExtensionPoint::Observer,
                PLUGIN_API_VERSION,
                vec![Capability::ObservedRecord],
            ),
            Arc::new(PanicObserver),
        )
        .expect("注册观察者");
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.route_panic").expect("合法"),
                ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision],
            ),
            Arc::new(PanicRoute),
        )
        .expect("注册路由");
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
    let bridge = Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse_fixture(CONFIG).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )));

    // 预热，让分配器/日志缓冲稳定。
    for _ in 0..5 {
        one_cycle(&bridge);
    }
    let baseline = rss_kb();

    for _ in 0..ITERATIONS {
        one_cycle(&bridge);
    }

    let after = rss_kb();
    let growth = after.saturating_sub(baseline);
    assert!(
        growth <= MAX_RSS_GROWTH_KB,
        "连续 {ITERATIONS} 次 panic 后 RSS 增长 {growth} KiB，超过 {MAX_RSS_GROWTH_KB} KiB"
    );

    let snapshot = lock(&bridge).drain_snapshot();
    assert_eq!(snapshot["queued_turns"], 0, "不应残留排队轮次");
    assert_eq!(snapshot["pending_route_actions"], 0, "不应残留路由动作");

    let catalog = lock(&bridge).plugin_catalog();
    let tool = catalog
        .iter()
        .find(|entry| entry["id"].as_str() == Some("test.tools"))
        .expect("工具 provider 仍在目录里");
    assert_eq!(
        tool["lastError"],
        serde_json::Value::Null,
        "panic 不是加载错误"
    );
    assert_eq!(
        tool["state"], "resident",
        "直接注册的 provider 仍是常驻；panic 不改变生命周期"
    );

    // 压力结束后再走一轮，确认还能继续服务。
    one_cycle(&bridge);

    println!(
        "panic stress: iterations={ITERATIONS} rss_baseline_kb={baseline} rss_after_kb={after} growth_kb={growth}"
    );
}
