//! 端到端：群内申请（单条/批量）→ 私信批准 → 同一会话下一次越权操作放行一次。
//!
//! 票据只放宽房间目标；`destructive` / `plugin` 不走票据，保持原有闸门。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Config, Element, ErrorCode, Ingest, MemoryStore, Outbound, OutboundError, RecordDraft,
    RoomId, Sender, Sent,
};
use serde_json::json;

const AT: i64 = 1_793_000_000_000;

type Sends = Arc<Mutex<Vec<(RoomId, Vec<Element>)>>>;

struct Recorder {
    sends: Sends,
}

impl Outbound for Recorder {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        let mut sends = self.sends.lock().expect("没被毒死");
        sends.push((room.clone(), elements.to_vec()));
        Ok(Sent {
            platform_id: format!("p-{}", sends.len()),
            at: AT,
        })
    }
}

fn sessions() -> Vec<String> {
    vec!["main".to_owned()]
}

fn other_sessions() -> Vec<String> {
    vec!["other".to_owned()]
}

fn config() -> &'static str {
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
        name = "DeepSeek"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = "t-main"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = true

        [[sessions]]
        id = "other"
        harness = "dsh"
        persona = "p"
        token = "t-other"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/456"

        [sessions.subscriptions.wake]
        at_mention = true

        [tickets]
        request_cooldown_secs = 0
        admin_cooldown_secs = 0

        [grants.system_admin]
        connection = "qq"
        account = "admin"
    "#
}

fn bridge_with(text: &str) -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        Config::parse(text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );
    (bridge, sends)
}

fn bridge() -> (Bridge<MemoryStore>, Sends) {
    bridge_with(config())
}

fn message(room: &str, sender: &str, text: &str, at_self: bool) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse(room).expect("房间合法"),
        Sender {
            id: sender.to_owned(),
            name: sender.to_owned(),
            role: None,
        },
        AT,
        vec![Element::text(text)],
        at_self,
    )
}

fn text_of(elements: &[Element]) -> String {
    elements
        .iter()
        .filter_map(Element::as_text)
        .collect::<String>()
}

fn approval_dm(sends: &Sends) -> String {
    sends
        .lock()
        .expect("没被毒死")
        .iter()
        .find(|(room, elements)| {
            room.connection == "qq"
                && room.path == ["private", "admin"]
                && text_of(elements).contains("[ACB 授权申请]")
        })
        .map(|(_, elements)| text_of(elements))
        .expect("审批消息已发到管理员私信")
}

fn field(text: &str, name: &str) -> String {
    text.split_whitespace()
        .find_map(|token| token.strip_prefix(name))
        .unwrap_or_else(|| panic!("审批消息里没有 {name}: {text}"))
        .to_owned()
}

#[test]
fn single_request_approve_and_use_once() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "帮我发个别的群", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let request = bridge
        .tool_call(
            &first,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["send_message_to"]}]}),
        )
        .expect("申请成功");
    assert_eq!(request["batch_id"], serde_json::Value::Null);
    assert_eq!(request["tickets"][0]["state"], "pending_approval");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();

    let duplicate = bridge
        .tool_call(
            &first,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["send_message_to"]}]}),
        )
        .expect("重复申请");
    assert_eq!(duplicate["tickets"][0]["duplicate"], true);

    bridge.end_turn(&first, None).expect("收工");
    let text = approval_dm(&sends);
    let nonce = field(&text, "nonce=");

    let handled = bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("管理员命令能入库");
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "私信审批命令由 core 接管，不叫醒 agent"
    );

    bridge
        .ingest(message("qq:group/123", "user1", "再试一次", true))
        .expect("能记下来");
    let second = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let sent = bridge
        .tool_call(
            &second,
            &sessions(),
            "send_message_to",
            json!({"to": "qq:group/999", "elements": [{"type": "text", "text": "approved"}]}),
        )
        .expect("票据放行一次");
    assert_eq!(sent["room"], "qq:group/999");

    let denied = bridge
        .tool_call(
            &second,
            &sessions(),
            "send_message_to",
            json!({"to": "qq:group/999", "elements": [{"type": "text", "text": "again"}]}),
        )
        .expect_err("用后即废");
    assert_eq!(denied.code, ErrorCode::RoomNotSubscribed);
}

#[test]
fn two_requests_in_one_turn_are_aggregated_into_one_message() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "帮我做两件事", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let first = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["send_message_to"]}]}),
        )
        .expect("第一条");
    let second = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/998", "ops": ["get_history"]}]}),
        )
        .expect("第二条");
    bridge.end_turn(&ticket, None).expect("收工");

    let text = approval_dm(&sends);
    assert!(text.contains("待批准 2 项"), "{text}");
    assert!(
        text.contains(first["tickets"][0]["ticket"].as_str().unwrap()),
        "{text}"
    );
    assert!(
        text.contains(second["tickets"][0]["ticket"].as_str().unwrap()),
        "{text}"
    );
    assert_eq!(
        sends
            .lock()
            .expect("没被毒死")
            .iter()
            .filter(|(room, elements)| room.path == ["private", "admin"]
                && text_of(elements).contains("[ACB 授权申请]"))
            .count(),
        1,
        "同一会话的两张 pending 只应发一条私信"
    );
}

#[test]
fn batch_request_approve_by_batch_id_and_query() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "批量申请", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let batch = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [
                {"target": "qq:group/999", "ops": ["send_message_to"]},
                {"target": "qq:group/998", "ops": ["get_history"]}
            ]}),
        )
        .expect("批量申请");
    let batch_id = batch["batch_id"].as_str().expect("batch id").to_owned();
    assert_eq!(batch["tickets"].as_array().expect("tickets").len(), 2);

    bridge.end_turn(&ticket, None).expect("收工");
    let text = approval_dm(&sends);
    assert!(
        text.contains(&batch_id),
        "审批消息要显眼带 batch id：{text}"
    );
    assert!(text.contains("batch="), "{text}");

    let handled = bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {batch_id}"),
            false,
        ))
        .expect("整批批准命令");
    assert!(matches!(handled, Ingest::Handled { .. }));

    bridge
        .ingest(message("qq:group/123", "user1", "查一下", true))
        .expect("能记下来");
    let next = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let queried = bridge
        .tool_call(
            &next,
            &sessions(),
            "access_ticket_get",
            json!({"batch_id": batch_id}),
        )
        .expect("按 batch 查询");
    assert_eq!(queried["count"], 2);
    for item in queried["tickets"].as_array().expect("tickets") {
        assert_eq!(item["state"], "active");
        assert_eq!(item["batch_id"], batch_id);
    }
}

#[test]
fn batch_rejects_too_many_items_and_bad_item() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "批量坏申请", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let items: Vec<serde_json::Value> = (0..9)
        .map(|index| json!({"target": format!("qq:group/{index}"), "ops": ["get_history"]}))
        .collect();
    let too_many = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": items}),
        )
        .expect_err("超过 8 条整批拒绝");
    assert_eq!(too_many.code, ErrorCode::BadRequest);
    assert!(too_many.message.contains("max 8"), "{}", too_many.message);

    let bad = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [
                {"target": "qq:group/999", "ops": ["send_message_to"]},
                {"target": "tg:group/999", "ops": ["send_message_to"]}
            ]}),
        )
        .expect_err("坏条目整批拒绝");
    assert_eq!(bad.code, ErrorCode::BadRequest);
    assert!(bad.message.contains("items[1]"), "{}", bad.message);
}

#[test]
fn batch_query_from_another_session_is_rejected() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "批量申请", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let batch = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [
                {"target": "qq:group/999", "ops": ["send_message_to"]},
                {"target": "qq:group/998", "ops": ["get_history"]}
            ]}),
        )
        .expect("批量申请");
    let batch_id = batch["batch_id"].as_str().expect("batch id").to_owned();
    bridge.end_turn(&ticket, None).expect("收工");

    bridge
        .ingest(message("qq:group/456", "user2", "我是另一个会话", true))
        .expect("能记下来");
    let other = bridge
        .poll_turn_for(&other_sessions())
        .expect("能开轮")
        .expect("有活")
        .ticket;
    let denied = bridge
        .tool_call(
            &other,
            &other_sessions(),
            "access_ticket_get",
            json!({"batch_id": batch_id}),
        )
        .expect_err("跨会话 batch 查询必须拒绝");
    assert_eq!(denied.code, ErrorCode::ToolNotAllowed);
    assert!(
        denied.message.contains("another session"),
        "{}",
        denied.message
    );
}

#[test]
fn session_cooldown_rejects_the_next_request() {
    let text = config().replace("request_cooldown_secs = 0", "request_cooldown_secs = 60");
    let (mut bridge, _sends) = bridge_with(&text);
    bridge
        .ingest(message("qq:group/123", "user1", "第一次", true))
        .expect("能记下来");
    let first = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    bridge
        .tool_call(
            &first,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["send_message_to"]}]}),
        )
        .expect("第一次申请");
    bridge.end_turn(&first, None).expect("收工");

    bridge
        .ingest(message("qq:group/123", "user1", "第二次", true))
        .expect("能记下来");
    let second = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let blocked = bridge
        .tool_call(
            &second,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/998", "ops": ["get_history"]}]}),
        )
        .expect_err("冷却期内拒绝");
    assert_eq!(blocked.code, ErrorCode::BadRequest);
    assert!(
        blocked.message.contains("cooling down"),
        "{}",
        blocked.message
    );
    assert!(blocked.message.contains(" 60s") || blocked.message.contains("59s"));
}

#[test]
fn approval_from_a_group_is_ignored() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message(
            "qq:group/123",
            "admin",
            "/approve tk-1 abcd",
            false,
        ))
        .expect("群消息照常入库");
    // 群里的同名账号不是审批入口；消息仍走普通路由/入库路径。
    assert!(matches!(
        bridge
            .ingest(message(
                "qq:group/123",
                "admin",
                "/approve tk-1 abcd",
                false
            ))
            .expect("群消息照常入库"),
        Ingest::Recorded { .. } | Ingest::NotSubscribed { .. }
    ));
}
