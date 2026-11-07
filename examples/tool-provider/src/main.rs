//! 进程内 ToolProvider 最小路径 + 轮次边界热卸载/重载。
//!
//! 运行：
//!   cargo run --manifest-path examples/tool-provider/Cargo.toml
//!
//! 它不连平台、不起服务，只证明一个宿主程序需要做哪几步：
//! 1. 实现 `ToolProvider::tools` / `call`；
//! 2. 用 `ToolLoader` 注册（provider id + 能力 + 工具声明）；
//! 3. 用 `Bridge::with_plugins` 把插件装进桥；
//! 4. agent 侧通过 `plugin_tools` 发现，`plugin_tool_call` 调用；
//! 5. `queue_unload` / `queue_reload` 在轮次边界生效，索引随之消失/出现。
//!
//! 当前限制：工具代码仍要跟宿主一起编译（动态库 ABI 还没有 tool 载体）；
//! 但生命周期已经走现有 pending_unload / refs / lastError 状态机，不需要重启进程。

use std::sync::Arc;

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId,
    Sender, Sent, Ticket, ToolCallContext, ToolError, ToolLoader, ToolProvider, ToolSpec,
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
token = { secret = "t-main" }

[[sessions.subscriptions]]
connection = "qq"
room = "group/1"
"#;

const PROVIDER: &str = "example.tools";

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

fn open_turn(bridge: &mut Bridge<MemoryStore>) -> Ticket {
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
    bridge
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket
}

fn list_tools(bridge: &mut Bridge<MemoryStore>, ticket: &Ticket) -> Value {
    bridge
        .tool_call(ticket, &["main".to_owned()], "plugin_tools", json!({}))
        .expect("列插件工具")
}

fn state(bridge: &Bridge<MemoryStore>, id: &str) -> Value {
    bridge
        .plugin_catalog()
        .into_iter()
        .find(|entry| entry["id"].as_str() == Some(id))
        .map(|entry| json!({"state": entry["state"], "pending": entry["pending"], "lastError": entry["lastError"]}))
        .unwrap_or(Value::Null)
}

fn main() {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(Arc::new(ToolLoader::new(vec![(
            ProviderDescriptor::new(
                ProviderId::parse(PROVIDER).expect("合法 provider id"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            Arc::new(EchoTool),
        )])))
        .expect("注册 ToolLoader");

    let mut bridge = Bridge::with_plugins(
        Config::parse_fixture(CONFIG).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    );

    let ticket = open_turn(&mut bridge);
    println!("initial: {}", list_tools(&mut bridge, &ticket));

    bridge.queue_unload(PROVIDER).expect("排队卸载");
    println!("after queue_unload: {}", state(&bridge, PROVIDER));
    bridge.end_turn(&ticket, None).expect("收轮");

    let ticket = open_turn(&mut bridge);
    println!("after unload: {}", list_tools(&mut bridge, &ticket));
    let called = bridge.tool_call(
        &ticket,
        &["main".to_owned()],
        "plugin_tool_call",
        json!({"tool": "example_echo", "arguments": {}}),
    );
    println!("call while unloaded: {called:?}");
    bridge.end_turn(&ticket, None).expect("收轮");

    bridge.queue_reload(PROVIDER).expect("排队重载");
    println!("after queue_reload: {}", state(&bridge, PROVIDER));

    let ticket = open_turn(&mut bridge);
    println!("after reload: {}", list_tools(&mut bridge, &ticket));
    let called = bridge
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({"tool": "example_echo", "arguments": {"text": "hello"}}),
        )
        .expect("重载后调用");
    println!("plugin_tool_call: {called}");
    bridge.end_turn(&ticket, None).expect("收轮");
}
