//! ToolProvider 热装：loader 注册的工具随轮次边界出现/消失。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginError, PluginManager, ProviderDescriptor, ProviderId, RecordDraft,
    RoomId, Sender, Sent, ToolCallContext, ToolError, ToolLoader, ToolProvider, ToolSpec,
};
use serde_json::{Value, json};

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
    token = "t-main"

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

struct EchoTool {
    name: &'static str,
}

impl ToolProvider for EchoTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: self.name.into(),
            summary: "热装测试工具".into(),
            schema: json!({"type": "object"}),
        }]
    }

    fn call(
        &self,
        context: &ToolCallContext<'_>,
        tool: &str,
        args: Value,
    ) -> Result<Value, ToolError> {
        Ok(json!({
            "tool": tool,
            "session": context.session,
            "args": args,
        }))
    }
}

fn descriptor(id: &str) -> ProviderDescriptor {
    ProviderDescriptor::new(
        ProviderId::parse(id).expect("合法"),
        ExtensionPoint::Tool,
        PLUGIN_API_VERSION,
        vec![Capability::ToolCall],
    )
}

fn bridge_with(loader: Arc<ToolLoader>) -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(loader)
        .expect("能装 ToolLoader");
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

fn open(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) -> acb_kernel::Ticket {
    lock(bridge)
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
    lock(bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket
}

fn tool_names(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
    ticket: &acb_kernel::Ticket,
) -> Vec<String> {
    let index = lock(bridge)
        .tool_call(ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("列插件工具");
    index["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn state(bridge: &Arc<Mutex<Bridge<MemoryStore>>>, id: &str) -> String {
    lock(bridge)
        .plugin_catalog()
        .into_iter()
        .find(|entry| entry["id"].as_str() == Some(id))
        .and_then(|entry| entry["state"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "missing".to_owned())
}

#[test]
fn tool_provider_appears_and_disappears_at_turn_boundaries() {
    let bridge = bridge_with(Arc::new(ToolLoader::new(vec![(
        descriptor("hot.tools"),
        Arc::new(EchoTool { name: "hot_echo" }),
    )])));

    // 第一轮：工具在索引里，能调用。
    let first = open(&bridge);
    assert_eq!(tool_names(&bridge, &first), vec!["hot_echo"]);
    lock(&bridge)
        .tool_call(
            &first,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "hot_echo",
                "arguments": {"x": 1},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect("热装工具能调用");

    // 在途轮次里排队卸载：旧快照不受影响。
    lock(&bridge).queue_unload("hot.tools").expect("可排队卸载");
    assert!(state(&bridge, "hot.tools").contains("pending_unload"));
    assert_eq!(
        tool_names(&bridge, &first),
        vec!["hot_echo"],
        "旧会话在轮次结束前仍用旧快照"
    );

    // 收轮后的下一个安全点：卸载生效，索引消失。
    lock(&bridge).end_turn(&first, None).expect("收轮");
    let second = open(&bridge);
    assert_eq!(state(&bridge, "hot.tools"), "unloaded");
    assert!(
        tool_names(&bridge, &second).is_empty(),
        "卸载后索引必须消失"
    );
    let missing = lock(&bridge).tool_call(
        &second,
        &["main".to_owned()],
        "plugin_tool_call",
        json!({
            "tool": "hot_echo",
            "arguments": {},
            "scope": {"kind": "connection", "connection": "qq"},
        }),
    );
    assert!(missing.is_err(), "卸载后不能调用");
    // 其它内置工具不受影响。
    lock(&bridge)
        .tool_call(&second, &["main".to_owned()], "list_rooms", json!({}))
        .expect("旧会话的其它工具照常可用");

    // 排队重新加载，下一个边界恢复。
    lock(&bridge).queue_reload("hot.tools").expect("可排队重载");
    assert!(state(&bridge, "hot.tools").contains("pending_reload"));
    lock(&bridge).end_turn(&second, None).expect("收轮");
    let third = open(&bridge);
    assert_eq!(state(&bridge, "hot.tools"), "unloadable");
    assert_eq!(
        tool_names(&bridge, &third),
        vec!["hot_echo"],
        "重载后立即出现在索引里"
    );
    lock(&bridge)
        .tool_call(
            &third,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "hot_echo",
                "arguments": {"x": 2},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect("重载后能调用");
}

#[test]
fn tool_name_conflicts_are_rejected_with_actionable_error() {
    // 和内置工具重名。
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins
        .install_loader_owned(Arc::new(ToolLoader::new(vec![(
            descriptor("hot.shadow"),
            Arc::new(EchoTool {
                name: "send_message",
            }),
        )])))
        .expect_err("内置工具名不能被覆盖");
    assert!(matches!(
        error,
        PluginError::ToolNameConflict { ref name, .. } if name == "send_message"
    ));
    assert!(
        error.to_string().contains("send_message"),
        "错误要能指出冲突的工具名：{error}"
    );

    // 和另一个 provider 的工具重名。
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins
        .install_loader_owned(Arc::new(ToolLoader::new(vec![
            (
                descriptor("hot.one"),
                Arc::new(EchoTool { name: "same_name" }),
            ),
            (
                descriptor("hot.two"),
                Arc::new(EchoTool { name: "same_name" }),
            ),
        ])))
        .expect_err("跨 provider 重名必须拒绝");
    assert!(matches!(
        error,
        PluginError::ToolNameConflict { ref name, .. } if name == "same_name"
    ));
}
