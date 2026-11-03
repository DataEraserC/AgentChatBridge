//! T7c：反馈查询、回复与唤醒闭环。
//!
//! 反馈由会话 A 写，维护方回复后必须作为合成事件回到 A；同一条回复不能
//! 让别的会话读到 A 的反馈，也不能让非管理员替 A 回复。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ErrorCode, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RecordKind,
    RoomId, RouteAction, RouteContext, RouteDecision, RoutePolicy, RouteProvider, Sender, Sent,
    TurnReason,
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
    admin_qqs = ["10002"]

    [[connections.routes]]
    provider = "test.hand"

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

    [[sessions]]
    id = "observer"
    harness = "dsh"
    persona = "p"
    token = "t-observer"

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/1"

    [sessions.subscriptions.wake]
    provider = "acb.wake.default"
    at_mention = true
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

/// `/open main` / `/open observer` 直接唤醒目标会话，给测试签票。
struct HandRoute;

impl RouteProvider for HandRoute {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        let session = match context.text {
            "/open main" => Some("main"),
            "/open observer" => Some("observer"),
            _ => None,
        };
        match session {
            Some(session) => RouteDecision::handle(RouteAction::Wake {
                session: Some(session.into()),
                room: None,
                reason: Some("test hand".into()),
            }),
            None => RouteDecision::Pass,
        }
    }
}

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.hand").expect("合法"),
                acb_kernel::ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision, Capability::WakeRequest],
            ),
            Arc::new(HandRoute),
        )
        .expect("注册路由");
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse(&config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )))
}

fn message(text: &str, sender: &str, at_self: bool) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse("qq:group/1").expect("房间"),
        Sender {
            id: sender.into(),
            name: "u".into(),
            role: None,
        },
        1,
        vec![Element::text(text)],
        at_self,
    )
}

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 发一条 `/open <session>`，取出该会话的票。
fn open_turn(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
    session: &str,
    sender: &str,
) -> acb_kernel::Delivery {
    lock(bridge)
        .ingest(message(&format!("/open {session}"), sender, false))
        .expect("入库");
    lock(bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("直接唤醒应开轮")
}

#[test]
fn feedback_reply_is_recorded_and_wakes_the_original_session() {
    let bridge = bridge();
    let delivery = open_turn(&bridge, "main", "10001");
    let ticket = delivery.ticket;

    let reported = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "report_feedback",
            json!({"text": "缺少任务取消入口", "kind": "gap"}),
        )
        .expect("报反馈");
    let id = reported["id"].as_i64().expect("feedback id");

    let listed = lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "feedback_list", json!({}))
        .expect("列反馈");
    assert_eq!(listed["total"], 1);
    assert_eq!(listed["feedback"][0]["id"], id);

    let got = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "feedback_get",
            json!({"id": id}),
        )
        .expect("查反馈");
    assert_eq!(got["body"], "缺少任务取消入口");
    assert!(got["replies"].as_array().expect("replies").is_empty());

    let replied = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "feedback_reply",
            json!({"id": id, "text": "已记录，下个迭代处理"}),
        )
        .expect("回复反馈");
    assert_eq!(replied["status"], "acked", "回复会把 open 推进到 acked");
    assert!(replied["event_id"].as_u64().is_some(), "回复应发合成事件");
    assert!(
        replied["woke"]
            .as_array()
            .expect("woke")
            .iter()
            .any(|session| session == "main"),
        "原会话必须被唤醒：{}",
        replied["woke"]
    );

    let got = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "feedback_get",
            json!({"id": id}),
        )
        .expect("回复后查反馈");
    let replies = got["replies"].as_array().expect("replies");
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["text"], "已记录，下个迭代处理");
    assert_eq!(replies[0]["author"], "main");

    lock(&bridge).end_turn(&ticket, None).expect("收工");
    let next = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("回复应排进唤醒队列");
    assert_eq!(next.session, "main");
    assert_eq!(next.reason, TurnReason::Wake);
    assert!(
        next.wake_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("feedback")),
        "唤醒原因要说明是反馈回复：{:?}",
        next.wake_reason
    );
    let event = next
        .records
        .iter()
        .find(|record| record.is_event("feedback"))
        .expect("回复事件应进入流水并投递");
    match &event.kind {
        RecordKind::Event { tags, payload, .. } => {
            assert!(tags.contains(&"feedback.reply".to_owned()));
            assert_eq!(
                payload
                    .as_ref()
                    .and_then(|value| value["feedback_id"].as_i64()),
                Some(id)
            );
        }
        other => panic!("unexpected kind {other:?}"),
    }
}

#[test]
fn large_feedback_list_is_indexed_and_get_keeps_full_body() {
    let bridge = bridge();
    let delivery = open_turn(&bridge, "main", "10001");
    let ticket = delivery.ticket;

    let long = "很长的正文".repeat(700);
    let first = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "report_feedback",
            json!({"text": format!("第一条 {long}")}),
        )
        .expect("报第一条");
    let second = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "report_feedback",
            json!({"text": format!("第二条 {long}")}),
        )
        .expect("报第二条");

    let listed = lock(&bridge)
        .tool_call(&ticket, &["main".to_owned()], "feedback_list", json!({}))
        .expect("列反馈");
    assert_eq!(listed["indexed"], true, "超过阈值应自动索引：{listed}");
    assert_eq!(listed["returned"], 2);
    assert!(listed["detail_hint"].as_str().is_some());
    for item in listed["feedback"].as_array().expect("feedback") {
        assert!(item.get("body").is_none(), "索引化后不返回正文：{item}");
        assert!(item.get("suggestion").is_none());
        assert!(item.get("replies").is_none());
        assert!(item["reply_count"].as_u64().is_some());
    }

    let id = first["id"].as_i64().expect("id");
    let got = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "feedback_get",
            json!({"id": id}),
        )
        .expect("详情仍给全文");
    assert!(
        got["body"].as_str().expect("body").contains("很长的正文"),
        "详情必须保留完整正文"
    );
    let _ = second;
}

#[test]
fn feedback_is_scoped_and_reply_requires_admin_cross_session() {
    let bridge = bridge();
    let main_turn = open_turn(&bridge, "main", "10001");
    let reported = lock(&bridge)
        .tool_call(
            &main_turn.ticket,
            &["main".to_owned()],
            "report_feedback",
            json!({"text": "main 的反馈"}),
        )
        .expect("报反馈");
    let id = reported["id"].as_i64().expect("feedback id");
    lock(&bridge)
        .end_turn(&main_turn.ticket, None)
        .expect("收工");

    let observer = open_turn(&bridge, "observer", "10001");
    let listed = lock(&bridge)
        .tool_call(
            &observer.ticket,
            &["observer".to_owned()],
            "feedback_list",
            json!({}),
        )
        .expect("列反馈");
    assert_eq!(listed["total"], 0, "别的会话的反馈不可见");

    let got = lock(&bridge).tool_call(
        &observer.ticket,
        &["observer".to_owned()],
        "feedback_get",
        json!({"id": id}),
    );
    assert_eq!(got.expect_err("不能跨会话查").code, ErrorCode::BadRequest);

    let reply = lock(&bridge).tool_call(
        &observer.ticket,
        &["observer".to_owned()],
        "feedback_reply",
        json!({"id": id, "text": "越权回复"}),
    );
    assert_eq!(
        reply.expect_err("非管理员不能代回").code,
        ErrorCode::Unauthorized
    );
}

#[test]
fn admin_can_reply_across_sessions() {
    let bridge = bridge();
    let main_turn = open_turn(&bridge, "main", "10001");
    let reported = lock(&bridge)
        .tool_call(
            &main_turn.ticket,
            &["main".to_owned()],
            "report_feedback",
            json!({"text": "管理员要能处理"}),
        )
        .expect("报反馈");
    let id = reported["id"].as_i64().expect("feedback id");
    lock(&bridge)
        .end_turn(&main_turn.ticket, None)
        .expect("收工");

    lock(&bridge)
        .ingest(message("在吗", "10002", true))
        .expect("入库");
    let observer = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("管理员会话应被 @ 唤醒");
    let reply = lock(&bridge)
        .tool_call(
            &observer.ticket,
            &["observer".to_owned()],
            "feedback_reply",
            json!({"id": id, "text": "管理员收到"}),
        )
        .expect("管理员可跨会话回复");
    assert_eq!(reply["status"], "acked");
    assert!(
        reply["woke"]
            .as_array()
            .expect("woke")
            .iter()
            .any(|session| session == "main"),
        "回复应唤醒原会话 main：{}",
        reply["woke"]
    );
}
