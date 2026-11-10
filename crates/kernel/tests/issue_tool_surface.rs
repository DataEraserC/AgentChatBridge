//! issue 工具走插件面：`plugin_tools` 能看到 8 个，core 的 surface 仍是 29 个。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId, Sender,
    Sent, TOOL_NAMES, ToolCallContext, ToolError, ToolProvider, ToolSpec,
};
use serde_json::{Value, json};

const ISSUE_TOOLS: [&str; 8] = [
    "issue_create",
    "issue_get",
    "issue_search",
    "issue_update",
    "issue_subscribe",
    "issue_unsubscribe",
    "issue_report_result",
    "issue_my_work",
];

struct IssueTools;

impl ToolProvider for IssueTools {
    fn tools(&self) -> Vec<ToolSpec> {
        ISSUE_TOOLS
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_owned(),
                summary: format!("issue tool {name}"),
                schema: json!({"type": "object", "additionalProperties": true}),
            })
            .collect()
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

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: 1,
        })
    }
}

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_tools(
            ProviderDescriptor::new(
                ProviderId::parse("issue.tools").expect("合法 id"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            Arc::new(IssueTools),
        )
        .expect("注册 issue 工具");
    let config = Config::parse_fixture(
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
        "#,
    )
    .expect("配置合法");
    Arc::new(Mutex::new(Bridge::with_plugins(
        config,
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
fn issue_tools_are_plugin_only_and_do_not_take_core_slots() {
    let bridge = bridge();
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

    let index = lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("列插件工具");
    assert_eq!(index["total"], 8, "plugin_tools 必须看到 8 个 issue 工具");
    let listed: Vec<String> = index["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
        .collect();
    for name in ISSUE_TOOLS {
        assert!(
            listed.iter().any(|item| item == name),
            "{name} 不在插件索引里"
        );
    }

    let surface = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "capabilities",
            json!({"scope": "surface"}),
        )
        .expect("算 surface");
    assert_eq!(
        surface["counts"]["tools_total"], 33,
        "core 工具数含管理面 4 个"
    );
    assert_eq!(surface["counts"]["resident"], 8, "常驻层仍是 8");
    assert_eq!(surface["counts"]["catalog"], 25, "目录层含管理面 4 个");

    let resident: Vec<&str> = surface["resident"]
        .as_array()
        .expect("resident")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let catalog: Vec<&str> = surface["catalog"]
        .as_array()
        .expect("catalog")
        .iter()
        .filter_map(|entry| entry["tool"].as_str())
        .collect();
    for name in ISSUE_TOOLS {
        assert!(!resident.contains(&name), "{name} 不能出现在 resident");
        assert!(!catalog.contains(&name), "{name} 不能出现在 catalog");
        assert!(!TOOL_NAMES.contains(&name), "{name} 不能进 core TOOL_NAMES");
    }

    // 插件调用入口仍在，且不需要把 issue 工具塞进 core 工具表。
    let called = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "issue_my_work",
                "arguments": {"subscriber": "session:main", "channel": "acb_session"},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect("调 issue 工具");
    assert_eq!(called["tool"], "issue_my_work");
    assert_eq!(called["session"], "main");
}
