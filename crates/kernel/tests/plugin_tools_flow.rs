//! ToolProvider：插件工具发现 + 通用调用入口。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId, Sender,
    Sent, ToolCallContext, ToolError, ToolProvider, ToolSpec,
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

struct EchoTool;

impl ToolProvider for EchoTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo_context".into(),
            summary: "回显调用上下文和参数".into(),
            schema: json!({
                "type": "object",
                "properties": { "x": { "type": "integer" } },
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
            "origin_sender": context.origin_sender,
            "live": context.live,
            "args": args,
        }))
    }
}

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    bridge_with(config())
}

fn bridge_with(text: String) -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_tools(
            ProviderDescriptor::new(
                ProviderId::parse("test.tools").expect("合法"),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            ),
            Arc::new(EchoTool),
        )
        .expect("注册插件工具");
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse_fixture(&text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )))
}

/// 私信里的 bot admin；`with_plugin` 决定规则里有没有 plugin 能力。
fn bot_admin_config(with_plugin: bool) -> String {
    let capability = if with_plugin {
        r#"["room_read", "room_write", "plugin"]"#
    } else {
        r#"["room_read", "room_write"]"#
    };
    format!(
        r#"{}

[grants.bot_admin]
accounts = [{{ connection = "qq", account = "10001" }}]

[[grants.rules]]
id = "fixture-grant-9"
subject = "bot_admin"
place = "private"
scope = "subscribed"
capability = {capability}
"#,
        config().replace("room = \"group/1\"", "room = \"private/10001\"")
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
fn plugin_tools_are_discoverable_and_callable() {
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
    assert_eq!(index["total"], 1);
    assert_eq!(index["tools"][0]["name"], "echo_context");

    let detail = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tools",
            json!({"name": "echo_context"}),
        )
        .expect("查 schema");
    assert_eq!(detail["name"], "echo_context");
    assert_eq!(detail["schema"]["type"], "object");

    let called = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "echo_context",
                "arguments": {"x": 7},
                "scope": {"kind": "room", "room": "qq:group/1"},
            }),
        )
        .expect("调用插件工具");
    assert_eq!(called["tool"], "echo_context");
    assert_eq!(called["session"], "main");
    assert_eq!(called["room"], "qq:group/1");
    assert_eq!(called["live"], true);
    assert_eq!(called["args"]["x"], 7);

    let missing = lock(&bridge).tool_call(
        &ticket,
        &["main".to_owned()],
        "plugin_tool_call",
        json!({"tool": "nope"}),
    );
    assert_eq!(
        missing.expect_err("未知工具").code,
        acb_kernel::ErrorCode::BadRequest
    );
}

#[test]
fn plugin_tool_call_requires_an_explicit_scope() {
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

    let missing = lock(&bridge).tool_call(
        &ticket,
        &["main".to_owned()],
        "plugin_tool_call",
        json!({"tool": "echo_context", "arguments": {"x": 1}}),
    );
    assert_eq!(
        missing.expect_err("没声明 scope 必须拒绝").code,
        acb_kernel::ErrorCode::BadRequest
    );

    let unauthorized = lock(&bridge).tool_call(
        &ticket,
        &["main".to_owned()],
        "plugin_tool_call",
        json!({
            "tool": "echo_context",
            "arguments": {"x": 1},
            "scope": {"kind": "room", "room": "qq:group/999"},
        }),
    );
    assert_eq!(
        unauthorized.expect_err("未订阅房间必须拒绝").code,
        acb_kernel::ErrorCode::RoomNotSubscribed
    );

    let connection = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "echo_context",
                "arguments": {"x": 1},
                "scope": {"kind": "connection", "connection": "qq"},
            }),
        )
        .expect("连接作用域应通过");
    assert_eq!(connection["args"]["x"], 1);
}

#[test]
fn bot_admin_plugin_capability_is_fail_closed_without_a_grant() {
    fn turn(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) -> acb_kernel::Ticket {
        lock(bridge)
            .ingest(
                RecordDraft::message(
                    RoomId::parse("qq:private/10001").expect("房间"),
                    Sender {
                        id: "10001".into(),
                        name: "admin".into(),
                        role: None,
                    },
                    1,
                    vec![Element::text("批一下")],
                    true,
                )
                // 适配器声明这是私信；缺省会被 fail-safe 当成 MultiParty。
                .with_room_scope(acb_kernel::RoomScope::Direct),
            )
            .expect("入库");
        lock(bridge)
            .poll_turn()
            .expect("能开轮")
            .expect("有活")
            .ticket
    }

    let bridge = bridge_with(bot_admin_config(true));
    let ticket = turn(&bridge);
    let called = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "plugin_tool_call",
            json!({
                "tool": "echo_context",
                "arguments": {"x": 1},
                "scope": {"kind": "room", "room": "qq:private/10001"},
            }),
        )
        .expect("bot admin 有 plugin 能力时放行");
    assert_eq!(called["session"], "main");

    let bridge = bridge_with(bot_admin_config(false));
    let ticket = turn(&bridge);
    let denied = lock(&bridge).tool_call(
        &ticket,
        &["main".to_owned()],
        "plugin_tool_call",
        json!({
            "tool": "echo_context",
            "arguments": {"x": 1},
            "scope": {"kind": "room", "room": "qq:private/10001"},
        }),
    );
    assert_eq!(
        denied.expect_err("没有 plugin 授予必须 fail closed").code,
        acb_kernel::ErrorCode::ToolNotAllowed
    );
}

#[test]
fn tool_names_cannot_shadow_builtins() {
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins.register_tools(
        ProviderDescriptor::new(
            ProviderId::parse("test.bad").expect("合法"),
            ExtensionPoint::Tool,
            PLUGIN_API_VERSION,
            vec![Capability::ToolCall],
        ),
        Arc::new(ShadowTool),
    );
    assert!(error.is_err(), "和内置工具重名必须在注册时拒绝");
}

struct ShadowTool;

impl ToolProvider for ShadowTool {
    fn tools(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "send_message".into(),
            summary: "shadow".into(),
            schema: json!({"type": "object"}),
        }]
    }

    fn call(
        &self,
        _context: &ToolCallContext<'_>,
        _tool: &str,
        _args: Value,
    ) -> Result<Value, ToolError> {
        Ok(Value::Null)
    }
}
