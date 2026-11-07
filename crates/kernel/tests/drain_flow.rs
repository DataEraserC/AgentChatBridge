//! 排空语义：SIGTERM/SIGHUP 之后 core 只入库，不再开新轮/不再跑路由动作。
//!
//! 这里直接驱动 `Bridge::begin_drain`，不依赖 systemd；退出码和信号由 bridge crate 管。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, MemoryStore, Outbound, OutboundError, PLUGIN_API_VERSION,
    PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId, RouteAction, RouteContext,
    RouteDecision, RoutePolicy, RouteProvider, Sender, Sent, Transcript,
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

    [[connections.routes]]
    provider = "test.drain"

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

struct DrainRoute;

impl RouteProvider for DrainRoute {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        if context.text.starts_with("/task") {
            return RouteDecision::handle(RouteAction::Task {
                handler: "exec".into(),
                params: json!({
                    "command": ["sh", "-c", "sleep 0.1"],
                    "send": "text",
                }),
            });
        }
        if context.text.starts_with("/wake") {
            return RouteDecision::handle(RouteAction::Wake {
                session: Some("main".into()),
                room: None,
                reason: Some("drain test".into()),
            });
        }
        RouteDecision::Pass
    }
}

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_route(
            ProviderDescriptor::new(
                ProviderId::parse("test.drain").expect("合法"),
                acb_kernel::ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![Capability::RouteDecision, Capability::WakeRequest],
            ),
            Arc::new(DrainRoute),
        )
        .expect("注册路由");
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse_fixture(&config()).expect("配置合法"),
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

#[test]
fn drain_stops_new_turns_and_route_actions_but_keeps_records() {
    let bridge = bridge();
    lock(&bridge).ingest(message("/task first")).expect("入库");

    let before = lock(&bridge).drain_snapshot();
    assert_eq!(before["state"], "ready");
    assert_eq!(before["queued_tasks"], 1);
    assert_eq!(before["queued_turns"], 0);

    let snapshot = lock(&bridge).begin_drain();
    assert_eq!(snapshot["state"], "draining");
    assert_eq!(snapshot["queued_tasks"], 1, "已有任务不清空，只是不再新增");

    lock(&bridge).ingest(message("/task second")).expect("入库");
    lock(&bridge).ingest(message("在吗")).expect("入库");
    let after = lock(&bridge).drain_snapshot();
    assert_eq!(after["queued_tasks"], 1, "排空时不执行路由动作");
    assert_eq!(after["queued_turns"], 0, "排空时不叫醒任何人");
    assert_eq!(after["state"], "draining", "排空状态必须可被健康探针看到");

    // 记录仍在流水里：下一进程凭未读恢复，不靠内存队列。
    let room = RoomId::parse("qq:group/1").expect("房间");
    let records = lock(&bridge)
        .store()
        .since(&room, 0, usize::MAX)
        .expect("读流水");
    assert_eq!(
        records.len(),
        2,
        "排空时路由被跳过：命令与普通消息都照常写流水"
    );
    assert_eq!(records[0].elements, vec![Element::text("/task second")]);
    assert_eq!(records[1].elements, vec![Element::text("在吗")]);
    assert!(
        lock(&bridge).poll_turn().expect("能查询").is_none(),
        "排空期间不再开新轮"
    );
}

#[test]
fn drain_waits_for_active_turn_in_snapshot() {
    let bridge = bridge();
    lock(&bridge).ingest(message("/wake")).expect("入库");
    let delivery = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("直接唤醒开轮");
    let ticket = delivery.ticket;

    let snapshot = lock(&bridge).begin_drain();
    assert_eq!(snapshot["active_turns"], 1);
    assert_eq!(snapshot["state"], "draining");

    lock(&bridge).end_turn(&ticket, None).expect("收工");
    let after = lock(&bridge).drain_snapshot();
    assert_eq!(after["active_turns"], 0);
    assert_eq!(after["queued_turns"], 0);
    assert_eq!(after["running_tasks"], 0);
}

#[test]
fn large_task_list_is_indexed_and_task_get_keeps_detail() {
    let bridge = bridge();
    lock(&bridge).ingest(message("/wake")).expect("入库");
    let ticket = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("直接唤醒开轮")
        .ticket;

    for index in 0..40 {
        lock(&bridge)
            .ingest(message(&format!("/task {index}")))
            .expect("入库");
    }
    let listed = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "tasks",
            json!({"op": "list", "limit": 20}),
        )
        .expect("列任务");
    assert_eq!(listed["indexed"], true, "超过阈值应自动索引：{listed}");
    let tasks = listed["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 20);
    assert!(tasks[0].get("result").is_none(), "索引化后不返回 result");
    assert!(tasks[0].get("error").is_none());

    let id = tasks[0]["id"].as_str().expect("id");
    let got = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "tasks",
            json!({"op": "get", "id": id}),
        )
        .expect("详情仍给全文");
    assert!(got.get("result").is_some());
    assert!(got.get("error").is_some());
}

#[test]
fn drain_leftovers_are_reportable() {
    let bridge = bridge();
    lock(&bridge).ingest(message("/task slow")).expect("入库");
    lock(&bridge).begin_drain();
    let leftovers = lock(&bridge).drain_leftovers();
    let tasks = leftovers["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["status"], "queued");
    assert!(
        tasks[0]["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("task-"))
    );
}
