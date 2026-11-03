//! T7b-2：高信任模块直接唤醒。
//!
//! 验证 trust / 订阅 / 频率检查，以及审计视图。直接唤醒必须绕过一个
//! “正常消息不会叫醒”的 wake 策略，才能证明它真的不是普通事件路径。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, MemoryStore, Outbound, OutboundError, PLUGIN_API_VERSION,
    PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId, RouteAction, RouteContext,
    RouteDecision, RoutePolicy, RouteProvider, Sender, Sent, TurnReason,
};
use serde_json::Value;

fn config() -> String {
    r#"
    [[connections]]
    id = "qq"
    platform = "qq"
    kind = "onebot11"
    url = "ws://127.0.0.1:3001"
    account = "bot"

    [[connections.routes]]
    provider = "test.plain"

    [[connections.routes]]
    provider = "test.wake"

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

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: 1,
        })
    }
}

/// 高信任路线：声明 `WakeRequest` 能力。
struct TrustedRoute;

impl RouteProvider for TrustedRoute {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        match context.text {
            "/direct ok" => RouteDecision::handle(RouteAction::Wake {
                session: Some("main".into()),
                room: None,
                reason: Some("manual".into()),
            }),
            "/direct all" => RouteDecision::handle(RouteAction::Wake {
                session: None,
                room: None,
                reason: None,
            }),
            "/direct bad-session" => RouteDecision::handle(RouteAction::Wake {
                session: Some("other".into()),
                room: None,
                reason: None,
            }),
            "/direct bad-room" => RouteDecision::handle(RouteAction::Wake {
                session: None,
                room: Some(RoomId::parse("qq:group/2").expect("房间")),
                reason: None,
            }),
            _ => RouteDecision::Pass,
        }
    }
}

/// 普通路线：没有 `WakeRequest` 能力，直接唤醒应被拒绝。
struct PlainRoute;

impl RouteProvider for PlainRoute {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        if context.text == "/plain" {
            RouteDecision::handle(RouteAction::Wake {
                session: Some("main".into()),
                room: None,
                reason: Some("plain".into()),
            })
        } else {
            RouteDecision::Pass
        }
    }
}

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.wake").expect("合法"),
                acb_kernel::ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision, Capability::WakeRequest],
            ),
            Arc::new(TrustedRoute),
        )
        .expect("注册高信任路由");
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.plain").expect("合法"),
                acb_kernel::ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision],
            ),
            Arc::new(PlainRoute),
        )
        .expect("注册普通路由");
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse(&config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )))
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

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn audits(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) -> Value {
    lock(bridge).wake_audit()
}

fn outcomes(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) -> Vec<String> {
    audits(bridge)["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["outcome"].as_str().expect("outcome").to_owned())
        .collect()
}

#[test]
fn trusted_route_can_open_a_turn_without_wake_policy() {
    let bridge = bridge();
    // 正常消息本身不叫醒（策略被关掉），证明下面的开轮只可能来自直接唤醒。
    let ingest = lock(&bridge).ingest(message("/direct ok")).expect("入库");
    assert!(
        matches!(&ingest, acb_kernel::Ingest::Handled { woke, .. } if woke.is_empty()),
        "普通 wake 策略不应叫醒：{ingest:?}"
    );

    let delivery = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("直接唤醒应排进队列");
    assert_eq!(delivery.session, "main");
    assert_eq!(delivery.reason, TurnReason::Wake);
    let reason = delivery.wake_reason.expect("有唤醒原因");
    assert!(reason.contains("test.wake"), "原因带来源：{reason}");
    assert!(reason.contains("manual"), "原因带请求方说明：{reason}");
    assert!(delivery.records.is_empty(), "直接唤醒不伪造消息");

    let audit = audits(&bridge);
    assert_eq!(audit["total"], 1);
    assert_eq!(audit["entries"][0]["source"], "test.wake");
    assert_eq!(audit["entries"][0]["outcome"], "accepted");
    assert_eq!(audit["entries"][0]["session"], "main");
    assert_eq!(audit["entries"][0]["reason"], "manual");
}

#[test]
fn untrusted_route_wakes_nobody_and_is_audited() {
    let bridge = bridge();
    lock(&bridge).ingest(message("/plain")).expect("入库");
    assert!(lock(&bridge).poll_turn().expect("能开轮").is_none());

    let audit = audits(&bridge);
    assert_eq!(audit["total"], 1);
    assert_eq!(audit["entries"][0]["source"], "test.plain");
    assert_eq!(audit["entries"][0]["outcome"], "untrusted");
}

#[test]
fn direct_wake_respects_subscriptions() {
    let bridge = bridge();
    lock(&bridge)
        .ingest(message("/direct bad-session"))
        .expect("入库");
    lock(&bridge)
        .ingest(message("/direct bad-room"))
        .expect("入库");
    assert!(lock(&bridge).poll_turn().expect("能开轮").is_none());

    let mut outcomes = outcomes(&bridge);
    outcomes.sort();
    assert_eq!(outcomes, vec!["no_subscribers", "not_subscribed"]);
}

#[test]
fn direct_wake_is_rate_limited_per_source() {
    let bridge = bridge();
    for _ in 0..7 {
        lock(&bridge).ingest(message("/direct ok")).expect("入库");
    }
    let outcomes = outcomes(&bridge);
    let accepted = outcomes
        .iter()
        .filter(|outcome| *outcome == "accepted")
        .count();
    let limited = outcomes
        .iter()
        .filter(|outcome| *outcome == "rate_limited")
        .count();
    assert_eq!(accepted, 6, "前 6 次放行：{outcomes:?}");
    assert_eq!(limited, 1, "第 7 次被限流：{outcomes:?}");
}

#[test]
fn direct_wake_all_subscribers_opens_each_session() {
    let bridge = bridge();
    lock(&bridge).ingest(message("/direct all")).expect("入库");

    let audit = audits(&bridge);
    assert_eq!(audit["total"], 1);
    assert_eq!(audit["entries"][0]["session"], "main");
    assert_eq!(audit["entries"][0]["outcome"], "accepted");

    let delivery = lock(&bridge).poll_turn().expect("能开轮").expect("有唤醒");
    assert_eq!(delivery.session, "main");
    assert!(delivery.wake_reason.expect("原因").contains("test.wake"));
}
