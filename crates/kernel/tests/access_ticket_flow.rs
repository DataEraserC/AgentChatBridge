//! 端到端：群内申请（单条/批量）→ 私信批准 → 同一会话下一次越权操作放行一次。
//!
//! 票据只放宽房间目标；`destructive` / `plugin` 不走票据，保持原有闸门。

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, CallError, CallOutcome, Config, Effect, Element, ErrorCode, Ingest, MemoryStore,
    MethodSpec, Outbound, OutboundError, ParamSpec, RecordDraft, RecordKind, RecordMode, Risk,
    RoomId, Scope, Sender, Sent, TargetSpec, TurnReason, Verification,
};
use serde_json::{Value, json};

const AT: i64 = 1_793_000_000_000;

type Sends = Arc<Mutex<Vec<(RoomId, Vec<Element>)>>>;
type Calls = Arc<Mutex<Vec<String>>>;

struct Recorder {
    sends: Sends,
    calls: Calls,
}

/// 目标连接声明一个 destructive + 一个 critical 方法，用来验证两道闸门。
fn destructive_catalog() -> std::sync::Arc<[MethodSpec]> {
    std::sync::Arc::from(vec![
        MethodSpec {
            name: "member.kick".into(),
            aliases: Cow::Borrowed(&[]),
            summary: "踢人".into(),
            tags: Vec::new().into(),
            risk: Risk::Destructive,
            scope: Scope::Room,
            params: vec![ParamSpec::required("user_id", "要踢的人")].into(),
            targets: vec![TargetSpec::room("group_id", "group")].into(),
            effect: Effect::Write,
            record: RecordMode::None,
            verification: Verification::verified("access ticket test fake"),
        },
        MethodSpec {
            name: "account.cookies".into(),
            aliases: Cow::Borrowed(&[]),
            summary: "获取 Cookies".into(),
            tags: Vec::new().into(),
            risk: Risk::Critical,
            scope: Scope::Connection,
            params: vec![
                ParamSpec::optional("domain", acb_kernel::ParamType::String, "域名"),
                ParamSpec::optional("token", acb_kernel::ParamType::String, "token"),
                ParamSpec::optional("cookies", acb_kernel::ParamType::String, "cookies"),
            ]
            .into(),
            targets: Vec::new().into(),
            effect: Effect::Read,
            record: RecordMode::None,
            verification: Verification::verified("access ticket test fake"),
        },
        MethodSpec {
            name: "group.unverified".into(),
            aliases: Cow::Borrowed(&[]),
            summary: "未验证的群设置".into(),
            tags: Vec::new().into(),
            risk: Risk::Write,
            scope: Scope::Room,
            params: Vec::new().into(),
            targets: Vec::new().into(),
            effect: Effect::Write,
            record: RecordMode::None,
            verification: Verification::unverified(
                "add_type 语义只有单一来源",
                "按 research §3.1 逐个回读",
            ),
        },
    ])
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

    fn catalog(&self, connection: &str) -> std::sync::Arc<[MethodSpec]> {
        if connection == "qq" {
            destructive_catalog()
        } else {
            std::sync::Arc::from(Vec::<MethodSpec>::new())
        }
    }

    fn call(
        &mut self,
        _room: &RoomId,
        method: &str,
        _params: &Value,
    ) -> Result<CallOutcome, CallError> {
        self.calls.lock().expect("没被毒死").push(method.to_owned());
        if method == "account.cookies" {
            return Ok(CallOutcome::value(json!({"cookies": "SECRET-COOKIE"})));
        }
        Ok(CallOutcome::value(json!({"done": true})))
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

        # 审批目标房间必须被某个会话订阅，路由才算可达。
        [[sessions.subscriptions]]
        connection = "qq"
        room = "private/admin"

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

        # 房间级路由覆盖全局默认；core 不再拼 private/{account}。
        [grants.rooms."qq:group/123"]
        approval_route = ["qq:private/admin"]

        [[grants.rules]]
                id = "fixture-grant-12"
                subject = "user"
        place = "group"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["access_request"]
    "#
}

fn bridge_with(text: &str) -> (Bridge<MemoryStore>, Sends) {
    let (bridge, sends, _calls) = bridge_with_calls(text);
    (bridge, sends)
}

fn bridge_with_calls(text: &str) -> (Bridge<MemoryStore>, Sends, Calls) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));
    let bridge = Bridge::new(
        Config::parse(text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            sends: sends.clone(),
            calls: calls.clone(),
        }),
    );
    (bridge, sends, calls)
}

fn bridge() -> (Bridge<MemoryStore>, Sends) {
    bridge_with(config())
}

fn message(room: &str, sender: &str, text: &str, at_self: bool) -> RecordDraft {
    let room = RoomId::parse(room).expect("房间合法");
    // 适配器声明结构事实；测试里按路径模拟适配器，不把它塞回 core。
    let scope = if room.path.first().map(String::as_str) == Some("private") {
        acb_kernel::RoomScope::Direct
    } else {
        acb_kernel::RoomScope::MultiParty
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
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["send_message_to"],
                "params": {"elements": [{"type": "text", "text": "approved"}]}
            }]}),
        )
        .expect("申请成功");
    assert_eq!(request["batch_id"], serde_json::Value::Null);
    assert_eq!(request["tickets"][0]["state"], "pending_approval");
    assert_eq!(
        request["tickets"][0]["reason"],
        serde_json::Value::Null,
        "旧调用没带 reason 时回复保持空值"
    );
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();

    let duplicate = bridge
        .tool_call(
            &first,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["send_message_to"],
                "params": {"elements": [{"type": "text", "text": "approved"}]}
            }]}),
        )
        .expect("重复申请");
    assert_eq!(duplicate["tickets"][0]["duplicate"], true);

    bridge.end_turn(&first, None).expect("收工");
    let text = approval_dm(&sends);
    assert!(
        text.contains("参数："),
        "审批私信要显示脱敏后的参数摘要：{text}"
    );
    assert!(
        text.contains("approved"),
        "非凭证参数应出现在摘要里：{text}"
    );
    assert!(
        text.contains("申请者：user1（user1）"),
        "审批私信要显示触发者 id 与显示名：{text}"
    );
    assert!(
        !text.contains("申请人自述"),
        "没有 reason 时审批消息不新增分区：{text}"
    );
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

    let executed = sends
        .lock()
        .expect("没被毒死")
        .iter()
        .any(|(room, elements)| {
            room == &RoomId::parse("qq:group/999").expect("房间")
                && text_of(elements).contains("approved")
        });
    assert!(executed, "批准后 core 应直接执行并发出消息");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有唤醒");
    assert_eq!(delivery.reason, TurnReason::Wake);
    assert_eq!(delivery.session, "main");
    assert_eq!(delivery.wake_reason.as_deref(), Some("ticket_result"));
    let pointer = delivery
        .records
        .iter()
        .find(|record| {
            matches!(&record.kind, RecordKind::Event { event, .. } if event == "ticket_result")
        })
        .expect("原会话流水里有一条 ticket_result 指针");
    assert_eq!(pointer.sender.id, "system", "结果指针不能伪装成人类发言");
    assert!(pointer.sender.role.is_none());
    assert!(!pointer.at_self && !pointer.reply_to_self);
    let pointer_text = text_of(&pointer.elements);
    assert!(pointer_text.contains(&id), "{pointer_text}");
    assert!(pointer_text.contains("qq:group/999"), "{pointer_text}");
    assert!(pointer_text.contains("send_message_to"), "{pointer_text}");
    assert!(pointer_text.contains("succeeded"), "{pointer_text}");
    assert!(
        pointer_text.contains(&format!("access_ticket_get {{\"id\":\"{id}\"}}")),
        "{pointer_text}"
    );

    let after = delivery.ticket;
    let queried = bridge
        .tool_call(&after, &sessions(), "access_ticket_get", json!({"id": id}))
        .expect("查询自己发起的票据");
    assert_eq!(queried["state"], "used");
    assert_eq!(queried["execution_status"], "succeeded");
    assert!(queried["executed_at"].is_number());
    assert!(
        queried["audit_ref"]
            .as_str()
            .is_some_and(|value| value.contains(&id))
    );
    assert!(
        queried["result"]["ops"]
            .as_array()
            .is_some_and(|ops| !ops.is_empty())
    );

    // 唤醒轮沿用原票据触发者，不继承管理员身份；有触发者才允许再申请票据。
    bridge
        .tool_call(
            &after,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/997", "ops": ["get_history"]}]}),
        )
        .expect("唤醒轮仍以原触发者身份");

    let denied = bridge
        .tool_call(
            &after,
            &sessions(),
            "send_message_to",
            json!({"to": "qq:group/999", "elements": [{"type": "text", "text": "again"}]}),
        )
        .expect_err("执行后票据已消费");
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
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["send_message_to"],
                "params": {"elements": [{"type": "text", "text": "approved"}]}
            }]}),
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
    let (mut bridge, sends) = bridge_with(config());
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
                {"target": "qq:group/999", "ops": ["platform_call:member.kick"],
                 "params": {"user_id": "10001"}},
                {"target": "qq:group/998", "ops": ["send_message_to"],
                 "params": {"elements": [{"type": "text", "text": "batch two"}]}}
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

    assert!(
        sends
            .lock()
            .expect("没被毒死")
            .iter()
            .any(|(room, elements)| {
                room == &RoomId::parse("qq:group/998").expect("房间")
                    && text_of(elements).contains("batch two")
            }),
        "第一条失败不中止整批：第二条 send_message_to 仍由 core 发出"
    );

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
        assert_eq!(item["batch_id"], batch_id);
    }
    let tickets = queried["tickets"].as_array().expect("tickets");
    assert_eq!(tickets[0]["state"], "used");
    assert_eq!(tickets[0]["execution_status"], "succeeded");
    assert_eq!(tickets[1]["state"], "used");
    assert_eq!(tickets[1]["execution_status"], "succeeded");
    assert!(
        tickets[1]["result"]["ops"]
            .as_array()
            .is_some_and(|ops| !ops.is_empty())
    );
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
                {"target": "qq:group/999", "ops": ["send_message_to"],
                 "params": {"elements": [{"type": "text", "text": "ok"}]}},
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
                {"target": "qq:group/999", "ops": ["send_message_to"],
                 "params": {"elements": [{"type": "text", "text": "batch one"}]}},
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
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["send_message_to"],
                "params": {"elements": [{"type": "text", "text": "approved"}]}
            }]}),
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

fn config_without_request_rule() -> String {
    config().replace(
        "        [[grants.rules]]\n                id = \"fixture-grant-12\"\n                subject = \"user\"\n        place = \"group\"\n        scope = \"connection\"\n        target = { kind = \"connection\", id = \"qq\" }\n        capability = [\"access_request\"]\n",
        "",
    )
}

#[test]
fn group_user_defaults_to_subscribed_targets_only() {
    let (mut bridge, sends) = bridge_with(&config_without_request_rule());
    bridge
        .ingest(message("qq:group/123", "user1", "想申请", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let denied = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["notes:list"]}]}),
        )
        .expect_err("默认不允许申请未订阅目标");
    assert_eq!(denied.code, ErrorCode::ToolNotAllowed);
    assert!(
        denied.message.contains("access_request_denied"),
        "错误要带稳定码：{}",
        denied.message
    );
    assert!(
        sends.lock().expect("没被毒死").is_empty(),
        "被拒的申请不能产生审批私信"
    );

    let allowed = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/123", "ops": ["notes:list"]}]}),
        )
        .expect("已订阅目标默认可申请");
    assert_eq!(allowed["tickets"][0]["state"], "pending_approval");
}

#[test]
fn bot_admin_can_approve_but_ordinary_user_cannot() {
    let text = config()
        .replace(
            "[grants.system_admin]",
            "[grants.bot_admin]\n        accounts = [{ connection = \"qq\", account = \"botadmin\" }]\n\n        [grants.system_admin]",
        )
        .replace(
            "approval_route = [\"qq:private/admin\"]",
            "approval_route = [\"qq:private/admin\", \"qq:private/botadmin\"]",
        );
    let (mut bridge, sends) = bridge_with(&text);
    bridge
        .ingest(message("qq:group/123", "user1", "申请", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["notes:list"]}]}),
        )
        .expect("有规则时能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");
    let approval = approval_dm(&sends);
    let nonce = field(&approval, "nonce=");

    let denied = bridge
        .ingest(message(
            "qq:private/user1",
            "user1",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("命令能入库");
    assert!(
        !matches!(denied, Ingest::Handled { .. }),
        "普通用户不能审批"
    );

    let handled = bridge
        .ingest(message(
            "qq:private/botadmin",
            "botadmin",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("命令能入库");
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "bot_admin 可以审批"
    );

    bridge
        .ingest(message("qq:group/123", "user1", "再开一轮", true))
        .expect("能记下来");
    let after = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let queried = bridge
        .tool_call(&after, &sessions(), "access_ticket_get", json!({"id": id}))
        .expect("能查");
    assert_eq!(queried["state"], "active");
}

#[test]
fn destructive_access_request_creates_a_ticket_by_default() {
    let (mut bridge, sends, calls) = bridge_with_calls(config());
    bridge
        .ingest(message("qq:group/123", "user1", "踢个人", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["platform_call:member.kick"],
                "params": {"user_id": "10001"}
            }]}),
        )
        .expect("默认 destructive=approval，申请必须生成票据");
    assert_eq!(request["tickets"][0]["state"], "pending_approval");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");

    let text = approval_dm(&sends);
    assert!(text.contains(&id), "审批私信要带票据 id：{text}");
    assert!(
        calls.lock().expect("没被毒死").is_empty(),
        "申请阶段根本不该调用平台"
    );
}

#[test]
fn critical_never_runs_directly_and_only_via_ticket() {
    let (mut bridge, sends, calls) = bridge_with_calls(config());
    bridge
        .ingest(message("qq:group/123", "user1", "查凭据", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    // 先证明过滤只影响可见性：critical op 不可直接执行，但仍可申请票据。
    let capability = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "account.cookies"}),
        )
        .expect("能查");
    assert_eq!(capability["direct_executable"], false);
    assert_eq!(capability["requestable"], true);
    assert_eq!(capability["ticket_required"], true);
    assert_eq!(capability["reason"], "critical_requires_ticket");

    let direct = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"op": "account.cookies", "params": {"domain": "qun.qq.com"}}),
        )
        .expect_err("没有规则授予 critical 时不能直接调用");
    assert!(
        matches!(
            direct.code,
            ErrorCode::ToolNotAllowed | ErrorCode::RoomNotSubscribed
        ),
        "默认 critical 必须被拒绝：{:?}",
        direct.code
    );
    assert!(
        calls.lock().expect("没被毒死").is_empty(),
        "默认 critical 不能绕过票据，平台一次都不该被调用"
    );

    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["platform_call:account.cookies"],
                "params": {
                    "domain": "qun.qq.com",
                    "token": "SECRET-TOKEN",
                    "cookies": "SECRET-COOKIE"
                }
            }]}),
        )
        .expect("critical 票据能申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");

    let approval = approval_dm(&sends);
    assert!(approval.contains("[CRITICAL]"), "{approval}");
    assert!(
        approval.contains("account_secret"),
        "账号级 critical 的能力集要带 account_secret：{approval}"
    );
    assert!(
        approval.contains("账号 bot（连接 qq）"),
        "审批私信要显示 typed target：{approval}"
    );
    assert!(approval.contains("参数："), "{approval}");
    assert!(
        !approval.contains("SECRET-COOKIE") && !approval.contains("SECRET-TOKEN"),
        "审批私信不能打印凭据明文：{approval}"
    );
    assert!(
        approval.contains("\"cookies\":\"***\"") || approval.contains("\"token\":\"***\""),
        "凭据字段要替换成 ***：{approval}"
    );
    let nonce = field(&approval, "nonce=");

    let handled = bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("管理员命令能入库");
    assert!(matches!(handled, Ingest::Handled { .. }));
    assert_eq!(
        calls.lock().expect("没被毒死").len(),
        1,
        "批准后 core 必须真的调用平台"
    );

    let delivery = bridge.poll_turn().expect("能开轮").expect("有唤醒");
    let after = delivery.ticket;
    let queried = bridge
        .tool_call(&after, &sessions(), "access_ticket_get", json!({"id": id}))
        .expect("查询执行结果");
    assert_eq!(queried["state"], "used");
    assert_eq!(queried["execution_status"], "succeeded");
    assert_eq!(
        queried["target"]["kind"], "account",
        "票据快照存 typed target"
    );
    assert_eq!(
        queried["result"]["ops"][0]["result"]["cookies"], "SECRET-COOKIE",
        "显式查询 access_ticket_get 才能看到结果"
    );

    let leaked = sends
        .lock()
        .expect("没被毒死")
        .iter()
        .any(|(_, elements)| text_of(elements).contains("SECRET-COOKIE"));
    assert!(!leaked, "critical 结果不能进入投递正文");
}

#[test]
fn unverified_op_is_direct_denied_and_ticket_approval_shows_the_reason() {
    let (mut bridge, sends, calls) = bridge_with_calls(config());
    bridge
        .ingest(message("qq:group/123", "user1", "想改群设置", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let direct = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"op": "group.unverified"}),
        )
        .expect_err("unverified 不能直接调");
    assert_eq!(direct.code, ErrorCode::ToolNotAllowed);
    assert!(direct.message.contains("unverified"), "{}", direct.message);
    assert!(
        calls.lock().expect("没被毒死").is_empty(),
        "被拒的直调不能碰平台"
    );

    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["platform_call:group.unverified"]
            }]}),
        )
        .expect("unverified 可以走票据");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");

    let approval = approval_dm(&sends);
    assert!(approval.contains("[CRITICAL]"), "{approval}");
    assert!(approval.contains("[UNVERIFIED]"), "{approval}");
    assert!(
        approval.contains("未验证：") && approval.contains("add_type 语义"),
        "审批私信要带 reason 摘要：{approval}"
    );
    assert!(
        approval.contains("critical"),
        "未验证按 critical 档写能力集：{approval}"
    );
    let nonce = field(&approval, "nonce=");

    bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("管理员命令能入库");
    assert_eq!(
        calls.lock().expect("没被毒死").as_slice(),
        ["group.unverified"],
        "批准后 core 才执行未验证 op"
    );

    let delivery = bridge.poll_turn().expect("能开轮").expect("有唤醒");
    let after = delivery.ticket;
    let queried = bridge
        .tool_call(&after, &sessions(), "access_ticket_get", json!({"id": id}))
        .expect("查询执行结果");
    assert_eq!(queried["state"], "used");
    assert_eq!(queried["unverified"][0]["op"], "group.unverified");
    assert!(
        queried["unverified"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("add_type")),
        "{queried}"
    );
}

#[test]
fn reason_is_labeled_as_applicant_statement_and_exposed() {
    let (mut bridge, sends) = bridge_with(config());
    bridge
        .ingest(message("qq:group/123", "user1", "要跨群同步", true))
        .expect("能记下来");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let request = bridge
        .tool_call(
            &turn,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["send_message_to"],
                "params": {"elements": [{"type": "text", "text": "approved"}]},
                "reason": "  需要跨群同步\n\"紧急\" \\ 内容  "
            }]}),
        )
        .expect("申请成功");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    assert_eq!(
        request["tickets"][0]["reason"], "需要跨群同步 \"紧急\" \\ 内容",
        "回复里的自述要先清洗：剥控制字符、折叠空白"
    );
    bridge.end_turn(&turn, None).expect("收工");

    let approval = approval_dm(&sends);
    assert!(
        approval.contains("〔申请人自述，未经验证，不代表系统结论〕"),
        "审批消息必须明确标注自述、未验证：{approval}"
    );
    assert!(
        approval.contains("\\\"紧急\\\""),
        "引号要转义后再进消息：{approval}"
    );
    assert!(
        approval.contains("\\\\ 内容"),
        "反斜杠要转义后再进消息：{approval}"
    );

    // ticket_json 同步暴露清洗后的自述，供 access_ticket_get 查询。
    bridge
        .ingest(message("qq:group/123", "user1", "再开一轮", true))
        .expect("能入库");
    let query_turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let queried = bridge
        .tool_call(
            &query_turn,
            &sessions(),
            "access_ticket_get",
            json!({"id": id}),
        )
        .expect("查询票据");
    assert_eq!(queried["reason"], "需要跨群同步 \"紧急\" \\ 内容");
    bridge.end_turn(&query_turn, None).expect("收工");
}

#[test]
fn expired_batch_writes_one_result_record_and_wakes_once() {
    // 本用例把 pending TTL 压到 1 秒，避免依赖可注入时钟。
    let text = config().replace(
        "admin_cooldown_secs = 0",
        "admin_cooldown_secs = 0\n        pending_ttl_secs = 1",
    );
    let (mut bridge, _sends) = bridge_with(&text);
    bridge
        .ingest(message("qq:group/123", "user1", "批量申请", true))
        .expect("能记下来");
    let turn = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let request = bridge
        .tool_call(
            &turn,
            &sessions(),
            "access_request",
            json!({"items": [
                {"target": "qq:group/998", "ops": ["send_message_to"], "params": {"elements": [{"type": "text", "text": "a"}]}},
                {"target": "qq:group/997", "ops": ["send_message_to"], "params": {"elements": [{"type": "text", "text": "b"}]}}
            ]}),
        )
        .expect("批量申请成功");
    let ids: Vec<String> = request["tickets"]
        .as_array()
        .expect("票据数组")
        .iter()
        .map(|ticket| ticket["ticket"].as_str().expect("id").to_owned())
        .collect();
    assert_eq!(ids.len(), 2);
    bridge.end_turn(&turn, None).expect("收工");

    std::thread::sleep(std::time::Duration::from_millis(1200));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间")
        .as_millis() as i64;
    let queued = bridge.tick_tickets(now);
    assert_eq!(queued, 1, "同一会话+房间的整批过期只唤醒一次");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有唤醒");
    assert_eq!(delivery.reason, TurnReason::Wake);
    assert_eq!(delivery.wake_reason.as_deref(), Some("ticket_result"));
    let results: Vec<_> = delivery
        .records
        .iter()
        .filter(|record| {
            matches!(&record.kind, RecordKind::Event { event, .. } if event == "ticket_result")
        })
        .collect();
    assert_eq!(results.len(), 1, "整批过期只写一条结果流水");
    let text = text_of(&results[0].elements);
    for id in &ids {
        assert!(text.contains(id), "过期流水要列出批次内票据: {text}");
    }
    assert!(text.contains("已过期"), "{text}");
    assert!(text.contains("approval_timeout"), "{text}");
    assert_eq!(results[0].sender.id, "system", "过期流水由系统写，不是人");
    bridge.end_turn(&delivery.ticket, None).expect("收工");

    assert!(
        bridge.poll_turn().expect("能轮询").is_none(),
        "聚合唤醒不能每张票各排一轮"
    );

    // 终态与拒绝语义分开：Expired + expire_reason，没有 approver。
    bridge
        .ingest(message("qq:group/123", "user1", "查结果", true))
        .expect("能入库");
    let query = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    for id in &ids {
        let ticket = bridge
            .tool_call(&query, &sessions(), "access_ticket_get", json!({"id": id}))
            .expect("查询票据");
        assert_eq!(ticket["state"], "expired");
        assert_eq!(ticket["expire_reason"], "approval_timeout");
        assert_eq!(ticket["approver"], serde_json::Value::Null);
    }
    bridge.end_turn(&query, None).expect("收工");
}

#[test]
fn mixed_target_kinds_are_refused_before_a_ticket_exists() {
    let (mut bridge, sends) = bridge_with(config());
    bridge
        .ingest(message("qq:group/123", "user1", "混合目标", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["notes:list", "platform_call:account.cookies"]
            }]}),
        )
        .expect_err("一张票不能混房间和账号目标");
    assert_eq!(error.code, ErrorCode::BadRequest);
    assert!(
        error.message.contains("target_kind_mismatch"),
        "错误要带稳定码：{}",
        error.message
    );
    assert!(
        sends.lock().expect("没被毒死").is_empty(),
        "被拒的申请不能产生审批私信"
    );
}

#[test]
fn critical_runs_directly_when_a_rule_grants_it() {
    let text = format!(
        "{}\n[[grants.rules]]\nid = \"fixture-grant-14\"\nsubject = \"user\"\nplace = \"group\"\nscope = \"connection\"\ntarget = {{ kind = \"connection\", id = \"qq\" }}\ncapability = [\"connection_read\", \"critical\"]\n",
        config()
    );
    let (mut bridge, _sends, calls) = bridge_with_calls(&text);
    bridge
        .ingest(message("qq:group/123", "user1", "查凭据", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let capability = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "account.cookies"}),
        )
        .expect("能查");
    assert_eq!(
        capability["direct_executable"], true,
        "规则显式授予 critical 后就是直调"
    );
    assert_eq!(capability["ticket_required"], false);

    let result = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"op": "account.cookies", "params": {"domain": "qun.qq.com"}}),
        )
        .expect("规则授予 critical 时直调");
    assert_eq!(result["cookies"], "SECRET-COOKIE");
    assert_eq!(
        calls.lock().expect("没被毒死").as_slice(),
        ["account.cookies"]
    );
}

#[test]
fn unverified_op_stays_ticket_only_even_when_rules_grant_critical() {
    let text = format!(
        "{}\n[[grants.rules]]\nid = \"fixture-grant-15\"\nsubject = \"user\"\nplace = \"group\"\nscope = \"connection\"\ntarget = {{ kind = \"connection\", id = \"qq\" }}\ncapability = [\"connection_read\", \"platform\", \"destructive\", \"critical\"]\n",
        config()
    );
    let (mut bridge, _sends, calls) = bridge_with_calls(&text);
    bridge
        .ingest(message("qq:group/123", "user1", "想改群设置", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let direct = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"op": "group.unverified"}),
        )
        .expect_err("unverified 是硬不变量，规则授予也不放行直调");
    assert_eq!(direct.code, ErrorCode::ToolNotAllowed);
    assert!(direct.message.contains("unverified"), "{}", direct.message);
    assert!(
        calls.lock().expect("没被毒死").is_empty(),
        "被拒的直调不能碰平台"
    );

    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["platform_call:group.unverified"]}]}),
        )
        .expect("unverified 仍可申请票据");
    assert_eq!(request["tickets"][0]["state"], "pending_approval");
}

#[test]
fn config_can_grant_critical_but_not_account_secret() {
    let allowed = format!(
        "{}\n        [[grants.rules]]\n                id = \"fixture-grant-16\"\n                subject = \"user\"\n        place = \"group\"\n        scope = \"connection\"\n        target = {{ kind = \"connection\", id = \"qq\" }}\n        capability = [\"critical\"]\n",
        config()
    );
    Config::parse(&allowed).expect("critical 允许被规则显式授予");

    let text = format!(
        "{}\n        [[grants.rules]]\n                id = \"fixture-grant-17\"\n                subject = \"user\"\n        place = \"group\"\n        scope = \"subscribed\"\n        capability = [\"account_secret\"]\n",
        config()
    );
    let error = Config::parse(&text).expect_err("account_secret 仍只能走票据");
    let detail = format!("{error:?}").to_lowercase();
    assert!(detail.contains("account_secret"), "{error:?}");
}

#[test]
fn ticket_ops_use_the_notes_entry_names() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "想读笔记", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let accepted = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["notes:list"]}]}),
        )
        .expect("新 op 名可以申请票据");
    assert_eq!(accepted["tickets"][0]["state"], "pending_approval");

    for old in ["get_notes", "set_note", "del_note"] {
        let error = bridge
            .tool_call(
                &ticket,
                &sessions(),
                "access_request",
                json!({"items": [{"target": "qq:group/999", "ops": [old]}]}),
            )
            .expect_err("旧工具名不再被接受");
        assert_eq!(error.code, ErrorCode::BadRequest);
        assert!(
            error.message.contains("unsupported op"),
            "错误要指出旧名不再支持：{}",
            error.message
        );
        assert!(
            error.message.contains(old),
            "错误里要出现提交的旧名：{}",
            error.message
        );
    }
}

#[test]
fn read_only_ticket_stays_active_for_the_session_to_retry() {
    let (mut bridge, sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "想读别的群", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["get_history"]}]}),
        )
        .expect("申请成功");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");
    let text = approval_dm(&sends);
    let nonce = field(&text, "nonce=");

    bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {id} {nonce}"),
            false,
        ))
        .expect("管理员命令能入库");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有唤醒");
    assert!(delivery.records.iter().any(|record| {
        matches!(&record.kind, RecordKind::Event { event, .. } if event == "ticket_result")
            && text_of(&record.elements).contains("active")
    }));
    let after = delivery.ticket;
    let queried = bridge
        .tool_call(&after, &sessions(), "access_ticket_get", json!({"id": id}))
        .expect("查询票据");
    assert_eq!(queried["state"], "active", "只读票据不能被 core 执行消费");
    assert!(queried["execution_status"].is_null());
}

#[test]
fn ticket_and_normal_call_share_unknown_op_rejection() {
    let (mut bridge, _sends) = bridge();
    bridge
        .ingest(message("qq:group/123", "user1", "层级的 op", true))
        .expect("能记下来");
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    let normal = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "platform_call",
            json!({"op": "member.nope", "params": {}}),
        )
        .expect_err("普通路径拒绝未知 op");
    assert_eq!(normal.code, ErrorCode::BadRequest);
    assert!(
        normal.message.contains("[unknown_op]"),
        "{}",
        normal.message
    );

    let requested = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{
                "target": "qq:group/999",
                "ops": ["platform_call:member.nope"],
                "params": {}
            }]}),
        )
        .expect_err("票据申请走同一段 op 解析");
    assert_eq!(requested.code, ErrorCode::BadRequest);
    assert!(
        requested.message.contains("[unknown_op]"),
        "{}",
        requested.message
    );
    assert!(
        requested.message.contains("items[0]"),
        "{}",
        requested.message
    );
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
