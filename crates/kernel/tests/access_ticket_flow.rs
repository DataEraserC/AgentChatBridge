//! 端到端：群内申请 → 私信批准 → 同一会话下一次越权操作放行一次。
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

        [grants.system_admin]
        connection = "qq"
        account = "admin"
    "#
}

fn bridge() -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        Config::parse(config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );
    (bridge, sends)
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

#[test]
fn request_approve_and_use_once() {
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
            json!({"target": "qq:group/999", "ops": ["send_message_to"]}),
        )
        .expect("申请成功");
    assert_eq!(request["state"], "pending_approval");
    let id = request["ticket"].as_str().expect("票据 id").to_owned();

    let duplicate = bridge
        .tool_call(
            &first,
            &sessions(),
            "access_request",
            json!({"target": "qq:group/999", "ops": ["send_message_to"]}),
        )
        .expect("重复申请");
    assert_eq!(
        duplicate["duplicate"], true,
        "同一目标/op 复用 pending 票据"
    );

    let approval = sends
        .lock()
        .expect("没被毒死")
        .iter()
        .find(|(room, elements)| {
            room.connection == "qq"
                && room.path == ["private", "admin"]
                && text_of(elements).contains("[ACB 授权申请]")
        })
        .cloned()
        .expect("审批消息已发到管理员私信");
    let text = text_of(&approval.1);
    let nonce = text
        .lines()
        .find_map(|line| line.strip_prefix("nonce="))
        .expect("审批消息里带 nonce")
        .trim()
        .to_owned();

    bridge.end_turn(&first, None).expect("收工");
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
