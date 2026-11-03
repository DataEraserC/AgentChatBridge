//! 进程内 ToolProvider 最小路径。
//!
//! 运行：
//!   cargo run --manifest-path examples/tool-provider/Cargo.toml
//!
//! 它不连平台、不起服务，只证明一个宿主程序需要做哪几步：
//! 1. 实现 `ToolProvider::tools` / `call`；
//! 2. 用 `PluginManager::register_tools` 注册（provider id + 能力）；
//! 3. 用 `Bridge::with_plugins` 把插件装进桥；
//! 4. agent 侧通过 `plugin_tools` 发现，`plugin_tool_call` 调用。
//!
//! 当前限制：动态库 ABI / `ACB_DYNAMIC_DIR` 还没有 tool 载体，所以 ToolProvider
//! 必须和宿主一起编译（或由进程内 Manager 提供），不能像 Lua / .so 那样热装。

use std::sync::Arc;

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId,
    Sender, Sent, ToolCallContext, ToolError, ToolProvider, ToolSpec,
};
use serde_json::{Value, json};

const CONFIG: &str = r#"
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
"#;

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "example".to_owned(),
            at: 1,
        })
    }
}

struct EchoTool;

impl ToolProvider for EchoTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "example_echo".into(),
            summary: "回显调用方给的参数".into(),
            schema: json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "additionalProperties": true,
            }),
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
            "room": context.room.to_string(),
            "args": args,
        }))
    }
}

fn main() {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_tools(
            ProviderDescriptor::new(
                ProviderId::parse("example.tools").expect("合法 provider id"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            Arc::new(EchoTool),
        )
        .expect("注册 ToolProvider");

    let mut bridge = Bridge::with_plugins(
        Config::parse(CONFIG).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    );

    bridge
        .ingest(RecordDraft::message(
            RoomId::parse("qq:group/1").expect("房间"),
            Sender {
                id: "10001".into(),
                name: "用户".into(),
                role: None,
            },
            1_793_000_000_000,
            vec![Element::text("在吗")],
            true,
        ))
        .expect("入库");
    let delivery = bridge
        .poll_turn()
        .expect("能开轮")
        .expect("有活");
    let ticket = delivery.ticket.clone();

    let index = bridge
        .tool_call(&ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("列插件工具");
    println!("plugin_tools: {index}");

    let called = bridge
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({"tool": "example_echo", "arguments": {"text": "hello"}}),
        )
        .expect("调用插件工具");
    println!("plugin_tool_call: {called}");

    bridge.end_turn(&ticket, None).expect("收轮");
}
