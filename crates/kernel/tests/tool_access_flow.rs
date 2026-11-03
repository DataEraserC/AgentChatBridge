//! ToolAccess：插件只能否决工具调用，不能放行 core 的判定。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ErrorCode, ExtensionPoint, MemoryStore, Outbound,
    OutboundError, PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft,
    RoomId, Sender, Sent, ToolAccessContext, ToolAccessProvider, ToolAccessVerdict,
};
use serde_json::json;

fn config() -> String {
    r#"
    [[connections]]
    id = "qq"
    platform = "qq"
    kind = "onebot11"
    url = "ws://127.0.0.1:3001"
    account = "bot"
    admin_qqs = ["10001"]

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

#[derive(Debug, Clone, PartialEq)]
struct Seen {
    tool: String,
    session: String,
    room: String,
    platform: String,
    origin_sender: Option<String>,
    is_admin: bool,
    live: bool,
}

struct DenyHistory {
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl ToolAccessProvider for DenyHistory {
    fn check(&self, context: &ToolAccessContext<'_>) -> Result<ToolAccessVerdict, String> {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        seen.push(Seen {
            tool: context.tool.to_owned(),
            session: context.session.to_owned(),
            room: context.room.to_string(),
            platform: context.platform.to_owned(),
            origin_sender: context.origin_sender.map(str::to_owned),
            is_admin: context.is_admin,
            live: context.live,
        });
        if context.tool == "get_history" {
            return Ok(ToolAccessVerdict::Deny {
                reason: "历史工具被策略禁用".into(),
            });
        }
        Ok(ToolAccessVerdict::Allow)
    }
}

struct AllowEverything;

impl ToolAccessProvider for AllowEverything {
    fn check(&self, _context: &ToolAccessContext<'_>) -> Result<ToolAccessVerdict, String> {
        Ok(ToolAccessVerdict::Allow)
    }
}

struct BrokenAccess;

impl ToolAccessProvider for BrokenAccess {
    fn check(&self, _context: &ToolAccessContext<'_>) -> Result<ToolAccessVerdict, String> {
        Err("provider boom".into())
    }
}

fn bridge(provider: Arc<dyn ToolAccessProvider>) -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_tool_access(
            ProviderDescriptor::new(
                ProviderId::parse("test.access").expect("合法"),
                ExtensionPoint::ToolAccess,
                PLUGIN_API_VERSION,
                vec![Capability::ToolAccessDecision],
            ),
            provider,
        )
        .expect("注册工具访问 provider");
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

fn ticket(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) -> acb_kernel::Ticket {
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

#[test]
fn provider_can_deny_one_tool_and_sees_the_call_context() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let bridge = bridge(Arc::new(DenyHistory { seen: seen.clone() }));
    let ticket = ticket(&bridge);

    let denied = lock(&bridge).tool_call(&ticket, &["main".to_owned()], "get_history", json!({}));
    let error = denied.expect_err("策略禁用 get_history");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
    assert!(
        error.message.contains("历史工具被策略禁用"),
        "错误要带策略原因：{}",
        error.message
    );

    let allowed = lock(&bridge).tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}));
    allowed.expect("没被策略禁用的工具照常可用");

    let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(seen.len(), 2, "每次调用只问一次，且只问被调用的工具");
    assert_eq!(seen[0].tool, "get_history");
    assert_eq!(seen[0].session, "main");
    assert_eq!(seen[0].room, "qq:group/1");
    assert_eq!(seen[0].platform, "qq");
    assert_eq!(seen[0].origin_sender.as_deref(), Some("10001"));
    assert!(seen[0].is_admin, "admin_qqs 里的发送者应当是管理员");
    assert!(seen[0].live);
}

#[test]
fn allow_cannot_override_the_core_access_boundary() {
    let bridge = bridge(Arc::new(AllowEverything));
    let ticket = ticket(&bridge);

    // get_history 查未订阅房间：插件说 Allow，core 仍然拒绝。
    let error = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "get_history",
            json!({"room": "qq:group/2"}),
        )
        .expect_err("未订阅房间必须被 core 拒绝");
    assert!(
        !error.message.contains("tool_denied"),
        "这是 core 的访问边界，不是插件否决：{}",
        error.message
    );
}

#[test]
fn provider_error_fails_closed() {
    let bridge = bridge(Arc::new(BrokenAccess));
    let ticket = ticket(&bridge);

    let error = lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "list_rooms", json!({}))
        .expect_err("provider 出错时不能放行");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
    assert!(
        error.message.contains("provider boom"),
        "错误要能定位到 provider：{}",
        error.message
    );
}
