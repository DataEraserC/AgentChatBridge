//! manager.instantiate panic：单个 provider 失败，bridge 继续，lastError 可见。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, ManagerManifest, ManagerPlugin,
    MemoryStore, Outbound, OutboundError, PLUGIN_API_VERSION, PluginError, PluginManager,
    ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, RecordDraft, RoomId,
    RoutePolicy, Sender, Sent, ToolCallContext, ToolError, ToolProvider, ToolSpec, WakePolicy,
};
use serde_json::{Value, json};

fn config(wake_provider: Option<&str>) -> String {
    let wake = wake_provider
        .map(|provider| format!("\n[sessions.subscriptions.wake]\nprovider = {provider:?}\n"))
        .unwrap_or_default();
    format!(
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
    token = "t-main"

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/1"
    {wake}
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

struct GoodTool;

impl ToolProvider for GoodTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "good_tool".into(),
            summary: "好插件提供的工具".into(),
            schema: json!({"type": "object"}),
        }]
    }

    fn call(
        &self,
        context: &ToolCallContext<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        Ok(json!({"tool": tool, "session": context.session, "args": args}))
    }
}

struct PanicManager;

impl ManagerPlugin for PanicManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            "panic",
            PLUGIN_API_VERSION,
            vec![
                Capability::WakeDecision,
                Capability::ToolCall,
                Capability::RouteDecision,
            ],
        )
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                ProviderId::parse("panic.wake").expect("合法"),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            ),
            ProviderDescriptor::new(
                ProviderId::parse("panic.bad_tool").expect("合法"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            ProviderDescriptor::new(
                ProviderId::parse("panic.good_tool").expect("合法"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            ProviderDescriptor::new(
                ProviderId::parse("panic.route").expect("合法"),
                ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision],
            ),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        match (id.name(), options) {
            ("wake", ProviderOptions::Wake(_)) => panic!("wake instantiate panic"),
            ("bad_tool", ProviderOptions::Tool) => panic!("tool instantiate panic"),
            ("good_tool", ProviderOptions::Tool) => Ok(ProviderInstance::Tool(Arc::new(GoodTool))),
            ("route", ProviderOptions::Route(_)) => panic!("route instantiate panic"),
            _ => Err(PluginError::UnknownProvider(id.to_string())),
        }
    }
}

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn catalog_error(bridge: &Arc<Mutex<Bridge<MemoryStore>>>, id: &str) -> Option<String> {
    lock(bridge)
        .plugin_catalog()
        .into_iter()
        .find(|entry| entry["id"].as_str() == Some(id))
        .and_then(|entry| entry["lastError"].as_str().map(str::to_owned))
}

#[test]
fn one_manager_panic_does_not_take_down_the_bridge() {
    let mut plugins = PluginManager::with_native_builtins();
    let before = plugins.descriptors().len();
    plugins
        .install_manager(Arc::new(PanicManager))
        .expect("能装 manager");
    let after_install = plugins.descriptors().len();
    assert!(after_install > before);

    let bridge = Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse(&config(None)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )));

    lock(&bridge)
        .ingest(RecordDraft::message(
            RoomId::parse("qq:group/1").expect("房间"),
            Sender {
                id: "10001".into(),
                name: "u".into(),
                role: None,
            },
            1,
            vec![Element::text("在吗")],
            true,
        ))
        .expect("入库");
    let ticket = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;

    // 索引会实例化两个工具 provider：坏的那个 panic，好的那个正常出现。
    let index = lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("索引仍可用");
    assert_eq!(index["total"], 1, "坏 provider 不能留下工具条目");
    assert_eq!(index["tools"][0]["name"], "good_tool");
    assert!(
        !index["tools"]
            .as_array()
            .expect("数组")
            .iter()
            .any(|tool| tool["name"] == "bad_tool"),
        "panic 的 provider 不能被当成已注册工具"
    );

    // 好工具照常调用；再次列索引也不会因为锁中毒而卡住。
    lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({"tool": "good_tool", "arguments": {"x": 1}}),
        )
        .expect("好插件不受影响");
    lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}))
        .expect("内置工具照常");
    lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("重复列索引不能卡死");

    // lastError 可见，且明确是实例化 panic。
    let error = catalog_error(&bridge, "panic.bad_tool").expect("坏 provider 要有 lastError");
    assert!(
        error.contains("tool instantiate panic"),
        "lastError 要带 panic 原因：{error}"
    );
    let good_error = catalog_error(&bridge, "panic.good_tool");
    assert!(good_error.is_none(), "好 provider 不应有 lastError");

    // 没有因为失败而多注册任何条目（只多了 manager 的四个 descriptor）。
    let descriptor_count = lock(&bridge)
        .plugin_catalog()
        .into_iter()
        .filter(|entry| {
            entry["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("panic."))
        })
        .count();
    assert_eq!(descriptor_count, 4, "descriptor 数量保持不变");
}

#[test]
fn referenced_provider_panic_fails_startup_with_actionable_error() {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(Arc::new(PanicManager))
        .expect("能装 manager");
    let before = plugins.descriptors().len();

    let config = Config::parse(&config(Some("panic.wake"))).expect("配置能解析");
    let error = plugins
        .validate_config(&config)
        .expect_err("被引用的坏 provider 必须在启动时拒绝");
    let text = error.to_string();
    assert!(
        text.contains("panic.wake"),
        "启动错误要指出被引用的 provider：{text}"
    );
    assert!(
        text.contains("wake instantiate panic"),
        "启动错误要带 panic 原因：{text}"
    );
    assert_eq!(plugins.descriptors().len(), before, "失败不能留下额外注册");
    assert!(
        plugins
            .provider_error(&ProviderId::parse("panic.wake").expect("合法"))
            .is_some(),
        "启动失败要写 lastError"
    );

    // 另一个 provider 仍能解析，说明失败只影响它自己。
    let bad = ProviderId::parse("panic.wake").expect("合法");
    let error_again = match plugins.resolve_wake(&bad, &WakePolicy::default()) {
        Err(error) => error,
        Ok(_) => panic!("同一个坏 provider 再次调用必须仍报错"),
    };
    assert!(error_again.to_string().contains("wake instantiate panic"));

    // 路由路径的 panic 也只影响它自己，不会被当成可用 route。
    let route = ProviderId::parse("panic.route").expect("合法");
    let route_policy = RoutePolicy {
        provider: route.clone(),
        options: json!({}),
    };
    let route_error = match plugins.resolve_route(&route, &route_policy) {
        Err(error) => error,
        Ok(_) => panic!("route instantiate panic 不能被当成成功"),
    };
    assert!(
        route_error.to_string().contains("route instantiate panic"),
        "路由 panic 要有可定位错误：{route_error}"
    );
    assert_eq!(
        plugins.descriptors().len(),
        before,
        "路由失败也不能留下额外注册"
    );
}
