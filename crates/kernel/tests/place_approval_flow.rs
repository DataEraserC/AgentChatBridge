//! 场所轴（适配器声明）与审批路由（配置驱动）的集成测试。

use std::borrow::Cow;
use std::process::Command;
use std::sync::{Arc, Mutex};

use acb_kernel::{
    ADAPTER_API_VERSION, AdapterCapability, AdapterManifest, Bridge, CallError, CallOutcome,
    Config, Effect, Element, Ingest, MemoryStore, MethodSpec, Outbound, OutboundError, Outbounds,
    ParamSpec, PluginManager, RecordDraft, RecordMode, Risk, RoomId, RoomScope, Scope, Sender,
    Sent, Verification,
};
use serde_json::{Value, json};

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

    fn catalog(&self, connection: &str) -> Arc<[MethodSpec]> {
        if connection == "qq" {
            Arc::from(vec![MethodSpec {
                name: "send.mock".into(),
                aliases: Cow::Borrowed(&[]),
                summary: "假发送".into(),
                tags: Vec::new().into(),
                risk: Risk::Write,
                scope: Scope::Room,
                params: vec![ParamSpec::required("text", "文本")].into(),
                targets: Vec::new().into(),
                effect: Effect::Write,
                record: RecordMode::None,
                verification: Verification::verified("place/approval test fake"),
            }])
        } else {
            Arc::from(Vec::<MethodSpec>::new())
        }
    }

    fn call(
        &mut self,
        _room: &RoomId,
        _method: &str,
        _params: &Value,
    ) -> Result<CallOutcome, CallError> {
        Ok(CallOutcome::value(json!({"done": true})))
    }
}

/// 指定目标发送失败，用于验证多目标投递的逐目标隔离。
struct FailingRecorder {
    sends: Sends,
    fail_targets: Vec<String>,
}

impl Outbound for FailingRecorder {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if self
            .fail_targets
            .iter()
            .any(|target| target == &room.to_string())
        {
            return Err(OutboundError::Unavailable("mock send failure".to_owned()));
        }
        let mut sends = self.sends.lock().expect("没被毒死");
        sends.push((room.clone(), elements.to_vec()));
        Ok(Sent {
            platform_id: format!("p-{}", sends.len()),
            at: AT,
        })
    }

    fn catalog(&self, _connection: &str) -> Arc<[MethodSpec]> {
        Arc::from(Vec::<MethodSpec>::new())
    }

    fn call(
        &mut self,
        _room: &RoomId,
        _method: &str,
        _params: &Value,
    ) -> Result<CallOutcome, CallError> {
        Ok(CallOutcome::value(json!({"done": true})))
    }
}

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
        name = "DeepSeek"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = { secret = "t-main" }

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/124"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/127"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/128"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/130"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/131"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/456"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/789"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/140"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/141"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "private/admin"
        [[sessions.subscriptions]]
        connection = "qq"
        room = "private/ghost"

        [tickets]
        request_cooldown_secs = 0
        admin_cooldown_secs = 0

        [grants.system_admin]
        connection = "qq"
        account = "admin"

        [grants.rooms."qq:group/123"]
        approval_route = ["qq:private/admin", "qq:group/456", "qq:group/789", "qq:private/ghost"]

        [grants.rooms."qq:group/124"]
        trust = "trusted"
        members = ["u1"]
        approval_route = ["qq:group/456"]

        [grants.rooms."qq:group/127"]
        approval_route = ["qq:private/unreachable"]

        [grants.rooms."qq:group/128"]
        approval_route = ["qq:group/128"]

        # 混合可达性：一个不可达、一个发送失败、一个正常。
        [grants.rooms."qq:group/140"]
        approval_route = ["qq:private/unreachable", "qq:group/789", "qq:group/456"]

        # 全部目标都发送失败。
        [grants.rooms."qq:group/141"]
        approval_route = ["qq:group/789", "qq:group/456"]

        # 账号级路由：请求房间没有房间级配置时使用。
        [grants.accounts."qq:bot"]
        approval_route = ["qq:private/admin"]

        [grants.rooms."qq:group/456"]
        trust = "trusted"
        members = ["alice"]

        [grants.rooms."qq:group/789"]
        trust = "trusted"
        members = ["bob"]

        [[grants.rules]]
                id = "fixture-grant-10"
                subject = "user"
        place = "group"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["access_request"]

        # 审批人必须真的持有票据里的能力（2026-11-10 起的规则）。
        [[grants.rules]]
                id = "fixture-admin-approval"
                subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["room_read", "room_write"]

        [[grants.roles]]
        id = "fixture-approver"
        name = "fixture-approver"
        capabilities = ["access_approve", "room_write"]

        [[grants.assignments]]
        id = "fixture-alice-approver"
        subject_kind = "account"
        subject_id = "qq:alice"
        role_id = "fixture-approver"
        scope_kind = "connection"
        scope_id = "qq"
        capability_snapshot = ["access_approve", "room_write"]
        role_snapshot = { id = "fixture-approver", name = "fixture-approver", capabilities = ["access_approve", "room_write"] }
    "#
    .to_owned()
}

fn bridge() -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        Config::parse_fixture(&config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
        }),
    );
    (bridge, sends)
}

fn message(room: &str, sender: &str, text: &str, at_self: bool) -> RecordDraft {
    let room = RoomId::parse(room).expect("房间合法");
    let scope = if room.path.first().map(String::as_str) == Some("private") {
        RoomScope::Direct
    } else {
        RoomScope::MultiParty
    };
    RecordDraft::message(
        room,
        Sender {
            id: sender.to_owned(),
            name: sender.to_owned(),
            role: None,
        },
        AT,
        vec![Element::text(text)],
        at_self,
    )
    .with_room_scope(scope)
}

/// 未知声明：故意不带 room_scope，模拟旧适配器。
fn message_unknown(room: &str, sender: &str, text: &str, at_self: bool) -> RecordDraft {
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

fn sent_to(sends: &Sends, room: &str) -> Vec<String> {
    sends
        .lock()
        .expect("没被毒死")
        .iter()
        .filter(|(target, _)| target.to_string() == room)
        .map(|(_, elements)| text_of(elements))
        .collect()
}

fn approval_text(sends: &Sends, room: &str) -> String {
    sent_to(sends, room)
        .into_iter()
        .rev()
        .find(|text| text.contains("[ACB 授权申请]"))
        .unwrap_or_else(|| panic!("{room} 没收到审批消息"))
}

fn nonce_of(text: &str) -> String {
    text.split_whitespace()
        .find_map(|token| token.strip_prefix("nonce="))
        .expect("审批消息里有 nonce")
        .to_owned()
}

fn start_request(
    bridge: &mut Bridge<MemoryStore>,
    room: &str,
    sender: &str,
    target: &str,
) -> Result<Value, acb_kernel::ToolError> {
    bridge
        .ingest(message(room, sender, "申请", true))
        .expect("能入库");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let request = bridge.tool_call(
        &turn,
        &["main".to_owned()],
        "access_request",
        json!({"items": [{
            "target": target,
            "ops": ["send_message_to"],
            "params": {"elements": [{"type": "text", "text": "approved"}]}
        }]}),
    );
    bridge.end_turn(&turn, None).expect("收工");
    request
}

fn ticket_state(bridge: &mut Bridge<MemoryStore>, room: &str, id: &str) -> Value {
    bridge
        .ingest(message(room, "u1", "再开一轮", true))
        .expect("能入库");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let value = bridge
        .tool_call(
            &turn,
            &["main".to_owned()],
            "access_ticket_get",
            json!({"id": id}),
        )
        .expect("能查票据");
    bridge.end_turn(&turn, None).expect("收工");
    value
}

fn approve(
    bridge: &mut Bridge<MemoryStore>,
    room: &str,
    sender: &str,
    id: &str,
    nonce: &str,
) -> Ingest {
    bridge
        .ingest(message(
            room,
            sender,
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("命令能入库")
}

#[test]
fn unknown_private_looking_room_is_multi_party_and_untrusted() {
    let (mut bridge, _sends) = bridge();
    // 私信路径但没有适配器声明：申请按 MultiParty + Untrusted 判定，admin_private 不生效。
    bridge
        .ingest(message_unknown("qq:private/ghost", "admin", "申请", true))
        .expect("能入库");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let denied = bridge.tool_call(
        &turn,
        &["main".to_owned()],
        "access_request",
        json!({"items": [{"target": "qq:group/999", "ops": ["send_message_to"]}]}),
    );
    assert!(
        denied.is_err(),
        "未知声明不能被当成 Direct 放宽: {denied:?}"
    );
    bridge.end_turn(&turn, None).expect("收工");
}

#[test]
fn trusted_group_member_can_approve_non_member_cannot() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));

    let denied = approve(&mut bridge, "qq:group/456", "mallory", &id, &nonce);
    // 拒绝也要回话：静默会让人以为是命令没生效，然后换个群再试。
    assert!(
        matches!(denied, Ingest::Handled { .. }),
        "授权命令由 core 接管"
    );
    let refusal = sent_to(&sends, "qq:group/456").pop().unwrap_or_default();
    assert!(
        refusal.contains("审批名单"),
        "要说清为什么不能批: {refusal}"
    );

    let handled = approve(&mut bridge, "qq:group/456", "alice", &id, &nonce);
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "trusted 群成员可以批准"
    );

    let ticket = ticket_state(&mut bridge, "qq:group/123", &id);
    // send_message_to 由 core 直接执行，批准后立即用掉；批准字段仍保留。
    assert_eq!(ticket["state"], "used");
    assert_eq!(ticket["approval_place"], "group");
    assert_eq!(ticket["trusted_group"], true, "审计要标 trusted_group");
    assert_eq!(ticket["approval_room"], "qq:group/456");
    assert_eq!(ticket["approver"], "qq:alice");
    assert!(
        ticket["approval_route"]
            .as_array()
            .is_some_and(|route| route.iter().any(|item| item == "qq:group/456")),
        "票据结果要落 approval_route: {ticket}"
    );
}

#[test]
fn trusted_group_member_without_approval_role_is_denied() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:group/789"));

    // bob 在可信群里，但没有 access_approve / room_write 的指派；不能靠「群可信」直接批。
    let denied = approve(&mut bridge, "qq:group/789", "bob", &id, &nonce);
    assert!(matches!(denied, Ingest::Handled { .. }));
    let refusal = sent_to(&sends, "qq:group/789").pop().unwrap_or_default();
    assert!(
        refusal.contains("access_approve"),
        "要说清缺什么权限: {refusal}"
    );
}

#[test]
fn untrusted_multi_party_group_cannot_approve() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/128", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:group/128"));

    let denied = approve(&mut bridge, "qq:group/128", "u1", &id, &nonce);
    assert!(
        matches!(denied, Ingest::Handled { .. }),
        "未配置 trusted 的群不能批准，但仍然要回话"
    );
    let refusal = sent_to(&sends, "qq:group/128").pop().unwrap_or_default();
    assert!(
        refusal.contains("不是审批地点"),
        "要说清这里不是审批地点: {refusal}"
    );
}

#[test]
fn group_approval_requires_ticket_id_and_nonce() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));

    let loose = bridge
        .ingest(message(
            "qq:group/456",
            "alice",
            &format!("/approve {id}"),
            false,
        ))
        .expect("命令能入库");
    assert!(
        matches!(loose, Ingest::Handled { .. }),
        "群内无 nonce 的宽松批准必须拒绝，并且要回话"
    );
    let hint = sent_to(&sends, "qq:group/456").pop().unwrap_or_default();
    assert!(hint.contains("nonce"), "要说明群内批准必须带 nonce: {hint}");

    let bare = bridge
        .ingest(message("qq:group/456", "alice", "/approve", false))
        .expect("命令能入库");
    assert!(
        matches!(bare, Ingest::Handled { .. }),
        "群内无 id 的批准必须拒绝，并且要回话"
    );

    // 同一张票据仍然 pending，带 id+nonce 后仍可正常批准。
    assert_eq!(
        ticket_state(&mut bridge, "qq:group/123", &id)["state"],
        "pending_approval"
    );
    let handled = approve(&mut bridge, "qq:group/456", "alice", &id, &nonce);
    assert!(matches!(handled, Ingest::Handled { .. }));
}

#[test]
fn routes_to_private_and_another_group_are_both_usable() {
    let (mut bridge, sends) = bridge();
    let private = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let private_id = private["tickets"][0]["ticket"].as_str().unwrap().to_owned();
    let private_nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));
    let handled = approve(
        &mut bridge,
        "qq:private/admin",
        "admin",
        &private_id,
        &private_nonce,
    );
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "路由到私聊可批准"
    );
    let ticket = ticket_state(&mut bridge, "qq:group/123", &private_id);
    assert_eq!(ticket["approval_place"], "private");
    assert_eq!(ticket["approval_room"], "qq:private/admin");
    // 其余目标收到 superseded，不再执行第二次批准。
    let group_text = sent_to(&sends, "qq:group/456")
        .into_iter()
        .find(|text| text.contains("无需再处理"))
        .expect("另一个群收到 superseded");
    assert!(group_text.contains(&private_id));

    let group = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/997").expect("能申请");
    let group_id = group["tickets"][0]["ticket"].as_str().unwrap().to_owned();
    let group_nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));
    let handled = approve(
        &mut bridge,
        "qq:group/456",
        "alice",
        &group_id,
        &group_nonce,
    );
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "路由到另一个群可批准"
    );
    let ticket = ticket_state(&mut bridge, "qq:group/123", &group_id);
    assert_eq!(ticket["approval_place"], "group");
    assert_eq!(ticket["approval_room"], "qq:group/456");

    // 已经 active 的票据再次批准不会改写批准房间/批准人。
    let again = approve(
        &mut bridge,
        "qq:private/admin",
        "admin",
        &group_id,
        &group_nonce,
    );
    assert!(
        matches!(again, Ingest::Handled { .. }),
        "命令仍由 core 消费"
    );
    let ticket = ticket_state(&mut bridge, "qq:group/123", &group_id);
    assert_eq!(ticket["state"], "used");
    assert_eq!(ticket["approval_room"], "qq:group/456");
    assert_eq!(ticket["approver"], "qq:alice");
}

#[test]
fn explicit_route_delivers_even_without_subscription_and_never_falls_back() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/127", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();

    // 显式写进 approval_route 的房间即使没有会话订阅，也允许投递：
    // 订阅只约束「收消息要不要叫醒」，审批命令本身在订阅闸门之前由 core 接管。
    assert!(
        sent_to(&sends, "qq:group/127")
            .iter()
            .all(|text| !text.contains("[ACB 授权申请]")),
        "审批投递绝不能回退到申请房间"
    );
    assert!(
        !sent_to(&sends, "qq:private/unreachable").is_empty(),
        "显式路由目标应收到审批消息"
    );

    let ticket = ticket_state(&mut bridge, "qq:group/127", &id);
    assert_eq!(ticket["state"], "pending_approval");
    assert!(ticket["announced"].as_bool().unwrap_or(false));
    assert!(
        ticket["route_unavailable"].is_null(),
        "投递成功要回写票据: {ticket}"
    );
}

#[test]
fn no_carrier_keeps_ticket_pending_and_records_delivery_failure() {
    let mut bridge = Bridge::with_outbounds(
        Config::parse_fixture(&config()).expect("配置合法"),
        MemoryStore::new(),
        Outbounds::new(),
        PluginManager::with_native_builtins(),
    );
    let request = start_request(&mut bridge, "qq:group/127", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();

    let ticket = ticket_state(&mut bridge, "qq:group/127", &id);
    assert_eq!(ticket["state"], "pending_approval");
    assert_eq!(ticket["announced"], false);
    assert!(
        ticket["route_unavailable"]
            .as_str()
            .is_some_and(|reason| reason.contains("carrier is down")),
        "载体不在位要回写投递失败: {ticket}"
    );
}

#[test]
fn cross_room_approval_checks_the_approving_room() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/124", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:group/456"));

    // u1 是申请房间 A 的 members，但不是批准房间 B 的成员：必须按 B 判定。
    let denied = approve(&mut bridge, "qq:group/456", "u1", &id, &nonce);
    assert!(
        matches!(denied, Ingest::Handled { .. }),
        "拒绝也算 core 接管了这条命令"
    );
    let refusal = sent_to(&sends, "qq:group/456").pop().unwrap_or_default();
    assert!(
        refusal.contains("审批名单"),
        "不能用申请房间的成员表批准: {refusal}"
    );

    let handled = approve(&mut bridge, "qq:group/456", "alice", &id, &nonce);
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "B 房间成员可以批准"
    );
    let ticket = ticket_state(&mut bridge, "qq:group/124", &id);
    assert_eq!(ticket["approval_room"], "qq:group/456");

    // 未路由到 C 房间的批准也要拒绝。
    let wrong_room = bridge
        .ingest(message(
            "qq:group/789",
            "bob",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("命令能入库");
    assert!(
        matches!(wrong_room, Ingest::Handled { .. }),
        "不在路由里的房间：拒绝并回话"
    );
    let refusal = sent_to(&sends, "qq:group/789").pop().unwrap_or_default();
    assert!(
        refusal.contains("qq:group/456"),
        "可信群里要说清该去哪批: {refusal}"
    );
}

#[test]
fn account_route_is_used_when_the_room_has_no_override() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/130", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));
    let handled = approve(&mut bridge, "qq:private/admin", "admin", &id, &nonce);
    assert!(matches!(handled, Ingest::Handled { .. }));
    let ticket = ticket_state(&mut bridge, "qq:group/130", &id);
    assert_eq!(
        ticket["approval_route"],
        json!(["qq:private/admin"]),
        "房间级没有配置时应落到账号级"
    );
}

#[test]
fn adapter_default_direct_room_is_the_last_resort() {
    let text = config().replace(
        "[grants.accounts.\"qq:bot\"]\n        approval_route = [\"qq:private/admin\"]",
        "",
    );
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let manifest = AdapterManifest {
        id: Cow::Borrowed("test.adapter"),
        platform: Cow::Borrowed("qq"),
        kind: Cow::Borrowed("onebot11"),
        transports: Vec::new().into(),
        api_version: ADAPTER_API_VERSION,
        capabilities: Vec::<AdapterCapability>::new().into(),
        events: Vec::new().into(),
        event_tags: Vec::new().into(),
        supports_at: true,
        supports_reply: true,
        text_limit: None,
        metadata: acb_kernel::PluginMetadata::EMPTY,
        direct_room: Some(Cow::Owned("qq:private/admin".to_owned())),
    };
    let outbounds = Outbounds::new()
        .with(
            "qq",
            Box::new(Recorder {
                sends: sends.clone(),
            }),
        )
        .with_manifest("qq", manifest);
    let mut bridge = Bridge::with_outbounds(
        Config::parse_fixture(&text).expect("配置合法"),
        MemoryStore::new(),
        outbounds,
        PluginManager::with_native_builtins(),
    );
    let request = start_request(&mut bridge, "qq:group/131", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));
    let handled = approve(&mut bridge, "qq:private/admin", "admin", &id, &nonce);
    assert!(matches!(handled, Ingest::Handled { .. }));
    let ticket = ticket_state(&mut bridge, "qq:group/131", &id);
    assert_eq!(
        ticket["approval_route"],
        json!(["qq:private/admin"]),
        "没有房间/账号路由时应使用适配器 direct_room"
    );
}

#[test]
fn ticket_result_keeps_audit_fields() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));
    approve(&mut bridge, "qq:group/456", "alice", &id, &nonce);
    let ticket = ticket_state(&mut bridge, "qq:group/123", &id);
    assert!(ticket["approval_route"].is_array());
    assert_eq!(ticket["approval_room"], "qq:group/456");
    assert_eq!(ticket["approval_place"], "group");
    assert_eq!(ticket["trusted_group"], true);
    assert_eq!(ticket["approver"], "qq:alice");
    assert!(
        ticket["approval_ref"]
            .as_str()
            .is_some_and(|reference| reference.starts_with("qq:group/456#")),
        "审批记录要带真正的批准房间: {ticket}"
    );
}

/// 子进程拿不到 libtest 的捕获缓冲，只能另起一个测试二进制来验证审计行。
#[test]
fn wrong_nonce_audit_logs_deny_not_allow() {
    let binary = std::env::current_exe().expect("测试二进制路径");
    let output = Command::new(binary)
        .args([
            "--exact",
            "wrong_nonce_audit_probe",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .expect("能启动审计探针子进程");
    assert!(
        output.status.success(),
        "探针子进程失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("[access-approval]") && stderr.contains("result=deny"),
        "错 nonce 必须记 deny 审计: {stderr}"
    );
    assert!(
        !stderr.contains("result=allow"),
        "错 nonce 不能记 allow 审计: {stderr}"
    );
}

#[test]
#[ignore = "由 wrong_nonce_audit_logs_deny_not_allow 子进程执行"]
fn wrong_nonce_audit_probe() {
    let (mut bridge, _sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let handled = approve(&mut bridge, "qq:private/admin", "admin", &id, "wrong-nonce");
    assert!(matches!(handled, Ingest::Handled { .. }));
}

#[test]
fn wrong_nonce_is_rejected_and_does_not_approve() {
    let (mut bridge, sends) = bridge();
    let request = start_request(&mut bridge, "qq:group/123", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    let nonce = nonce_of(&approval_text(&sends, "qq:private/admin"));

    let handled = approve(
        &mut bridge,
        "qq:private/admin",
        "admin",
        &id,
        "0000000000000000",
    );
    assert!(matches!(handled, Ingest::Handled { .. }));

    let ticket = ticket_state(&mut bridge, "qq:group/123", &id);
    assert_eq!(ticket["state"], "pending_approval", "错 nonce 不能批准");
    assert!(
        ticket["approval_room"].is_null(),
        "错 nonce 不能写批准房间: {ticket}"
    );
    let failure = sent_to(&sends, "qq:private/admin")
        .into_iter()
        .rev()
        .find(|text| text.contains("授权命令失败"))
        .expect("失败原因要回给审批房间");
    assert!(failure.contains("nonce mismatch"), "{failure}");

    let handled = approve(&mut bridge, "qq:private/admin", "admin", &id, &nonce);
    assert!(matches!(handled, Ingest::Handled { .. }));
    let ticket = ticket_state(&mut bridge, "qq:group/123", &id);
    assert_eq!(ticket["state"], "used", "正确 nonce 仍可批准");
    assert_eq!(ticket["approval_room"], "qq:private/admin");
}

#[test]
fn one_target_send_failure_does_not_block_other_targets() {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let outbounds = Outbounds::new().with(
        "qq",
        Box::new(FailingRecorder {
            sends: sends.clone(),
            fail_targets: vec!["qq:group/789".to_owned()],
        }),
    );
    let mut bridge = Bridge::with_outbounds(
        Config::parse_fixture(&config()).expect("配置合法"),
        MemoryStore::new(),
        outbounds,
        PluginManager::with_native_builtins(),
    );
    let request = start_request(&mut bridge, "qq:group/140", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();

    assert!(
        approval_text(&sends, "qq:group/456").contains(&id),
        "一个不可达 + 一个发送失败时，正常目标仍应收到审批消息"
    );
    let ticket = ticket_state(&mut bridge, "qq:group/140", &id);
    assert!(
        ticket["route_unavailable"].is_null(),
        "只要有目标投递成功，就不能标记整批投递失败: {ticket}"
    );
}

#[test]
fn all_target_send_failures_mark_the_ticket_failed() {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let outbounds = Outbounds::new().with(
        "qq",
        Box::new(FailingRecorder {
            sends: sends.clone(),
            fail_targets: vec!["qq:group/789".to_owned(), "qq:group/456".to_owned()],
        }),
    );
    let mut bridge = Bridge::with_outbounds(
        Config::parse_fixture(&config()).expect("配置合法"),
        MemoryStore::new(),
        outbounds,
        PluginManager::with_native_builtins(),
    );
    let request = start_request(&mut bridge, "qq:group/141", "u1", "qq:group/999").expect("能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();

    let ticket = ticket_state(&mut bridge, "qq:group/141", &id);
    assert_eq!(ticket["state"], "pending_approval");
    assert!(
        ticket["route_unavailable"].is_string(),
        "全部目标失败必须记录投递失败，不能静默: {ticket}"
    );
    assert!(sent_to(&sends, "qq:group/456").is_empty());
}

#[test]
fn batch_approval_by_id_works_in_a_trusted_group_without_a_nonce() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/124", "u1", "批量申请", true))
        .expect("能入库");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let batch = bridge
        .tool_call(
            &turn,
            &["main".to_owned()],
            "access_request",
            json!({"items": [
                {"target": "qq:group/998", "ops": ["send_message_to"],
                 "params": {"elements": [{"type": "text", "text": "one"}]}},
                {"target": "qq:group/997", "ops": ["send_message_to"],
                 "params": {"elements": [{"type": "text", "text": "two"}]}}
            ]}),
        )
        .expect("批量申请");
    let batch_id = batch["batch_id"].as_str().expect("batch id").to_owned();
    bridge.end_turn(&turn, None).expect("收工");

    let text = approval_text(&sends, "qq:group/456");
    assert!(text.contains(&batch_id), "审批消息要带 batch id：{text}");
    // 消息里直接给可复制的完整命令；群内批准必须带 nonce，让人自己拼就是坑人。
    let first_id = batch["tickets"][0]["ticket"].as_str().expect("ticket id");
    assert!(
        text.contains(&format!("/approve {first_id} ")),
        "审批消息要给出可直接复制的批准命令：{text}"
    );

    // 群内按 batch id 整批批准不需要 nonce：bt- 前缀本身就是绑定。
    let handled = bridge
        .ingest(message(
            "qq:group/456",
            "alice",
            &format!("/approve {batch_id}"),
            false,
        ))
        .expect("整批批准命令");
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "可信群里 /approve bt-xxx 应当生效"
    );
    let confirmation = sent_to(&sends, "qq:group/456").pop().unwrap_or_default();
    assert!(
        confirmation.contains("整批"),
        "要回执整批结果：{confirmation}"
    );
}
