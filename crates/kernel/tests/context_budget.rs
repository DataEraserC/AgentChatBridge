//! 成本测量：插件目录、task_list。数字通过 `-- --nocapture` 打印，供文档/进度记录。
//!
//! 不做接口优化断言，只保证测量路径可重复；阈值行为由各自的流程测试覆盖。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, DefaultWakeProvider, Element, ExtensionPoint, MemoryStore,
    Outbound, OutboundError, PLUGIN_API_VERSION, PluginManager, PluginMetadata, ProviderDescriptor,
    ProviderId, RecordDraft, RoomId, RouteAction, RouteContext, RouteDecision, RoutePolicy,
    RouteProvider, Sender, Sent,
};
use serde_json::{Value, json};

fn bytes(value: &Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

#[test]
fn measure_plugin_catalog() {
    let builtins = PluginManager::with_native_builtins().catalog_json();
    let mut plugins = PluginManager::with_native_builtins();
    for index in 0..50 {
        let metadata = PluginMetadata {
            title: Some(format!("测试插件 {index}").into()),
            summary: Some("用于测量插件目录随生态规模增长的成本".into()),
            description: Some(
                "描述由插件作者声明，长度随作者写法变化；这里给一段中等长度文本。".into(),
            ),
            tags: vec!["measure".into()].into(),
            effects: Vec::new().into(),
            docs_url: None,
            options_schema: None,
        };
        plugins
            .register_wake(
                ProviderDescriptor::new(
                    ProviderId::parse(&format!("bench.plugin{index}")).expect("合法"),
                    ExtensionPoint::Wake,
                    PLUGIN_API_VERSION,
                    vec![Capability::WakeDecision],
                )
                .with_metadata(metadata),
                Arc::new(DefaultWakeProvider),
            )
            .expect("注册测量 provider");
    }
    let catalog = plugins.catalog_json();
    println!(
        "MEASURE plugin_catalog typical_bytes={} typical_count={} worst_bytes={} worst_count={}",
        bytes(&json!(builtins)),
        builtins.len(),
        bytes(&json!(catalog)),
        catalog.len()
    );
    assert!(bytes(&json!(catalog)) > bytes(&json!(builtins)));
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

struct BudgetRoute;

impl RouteProvider for BudgetRoute {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        if context.text.starts_with("/task") {
            return RouteDecision::handle(RouteAction::Task {
                handler: "exec".into(),
                params: json!({
                    "command": ["sh", "-c", "printf ok"],
                    "send": "text",
                }),
            });
        }
        if context.text.starts_with("/wake") {
            return RouteDecision::handle(RouteAction::Wake {
                session: Some("main".into()),
                room: None,
                reason: Some("budget".into()),
            });
        }
        RouteDecision::Pass
    }
}

fn config() -> String {
    r#"
    [[connections]]
    id = "qq"
    platform = "qq"
    kind = "onebot11"
    url = "ws://127.0.0.1:3001"
    account = "bot"

    [[connections.routes]]
    provider = "bench.route"

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
    token = "t-main"

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/1"

    [sessions.subscriptions.wake]
    provider = "acb.wake.default"
    at_mention = false
    reply = false
    name_mention = false
    accumulate = 1000
    "#
    .to_owned()
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

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("bench.route").expect("合法"),
                ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision, Capability::WakeRequest],
            ),
            Arc::new(BudgetRoute),
        )
        .expect("注册路由");
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse(&config()).expect("配置合法"),
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

#[test]
fn measure_task_list() {
    for count in [5usize, 20] {
        let bridge = bridge();
        lock(&bridge).ingest(message("/wake")).expect("入库");
        let ticket = lock(&bridge)
            .poll_turn()
            .expect("能开轮")
            .expect("直接唤醒开轮")
            .ticket;
        for index in 0..count {
            lock(&bridge)
                .ingest(message(&format!("/task {index}")))
                .expect("入库");
        }
        let listed = lock(&bridge)
            .tool_call(
                &ticket,
                &["main".to_owned()],
                "tasks",
                json!({"op": "list", "limit": count}),
            )
            .expect("列任务");
        println!(
            "MEASURE tasks count={count} bytes={} indexed={} returned={}",
            bytes(&listed),
            listed["indexed"],
            listed["returned"]
        );
    }
}
