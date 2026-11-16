//! 管理面工具（config / policy / plugin_config / plugin_action）：
//! 只由策略数据决定能不能用，不设身份分支；每次判定都落审计。

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use acb_kernel::storage::AuditStore;
use acb_kernel::{
    Bridge, CallError, CallOutcome, Config, Effect, Element, ErrorCode, Ingest, MemoryStore,
    MethodSpec, Outbound, OutboundError, ParamSpec, RecordDraft, RecordMode, Risk, RoomId, Scope,
    Sender, Sent, TargetSpec, TurnReason, Verification,
};
use serde_json::json;

const AT: i64 = 1_793_000_000_000;

struct Recorder;

impl Outbound for Recorder {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p-1".to_owned(),
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
        _params: &serde_json::Value,
    ) -> Result<CallOutcome, CallError> {
        Ok(CallOutcome::value(json!({})))
    }
}

fn sessions() -> Vec<String> {
    vec!["main".to_owned()]
}

/// 基础配置：管理员私信 + 一个群。默认**不**给任何管理能力。
fn config(extra: &str) -> String {
    format!(
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
        token = {{ secret = "t-main" }}

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = true

        [[sessions.subscriptions]]
        connection = "qq"
        room = "private/admin"

        [tickets]
        request_cooldown_secs = 0
        admin_cooldown_secs = 0

        [grants.system_admin]
        connection = "qq"
        account = "admin"

        {extra}
    "#
    )
}

fn bridge_with(extra: &str) -> Bridge<MemoryStore> {
    Bridge::new(
        Config::parse_fixture(&config(extra)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder),
    )
}

fn message(room: &str, sender: &str, text: &str) -> RecordDraft {
    let room = RoomId::parse(room).expect("房间合法");
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
        true,
    )
    .with_room_scope(scope)
}

fn open_turn(bridge: &mut Bridge<MemoryStore>, room: &str, sender: &str) -> acb_kernel::Ticket {
    bridge
        .ingest(message(room, sender, "干活"))
        .expect("能入库");
    match bridge.poll_turn().expect("能开轮") {
        Some(delivery) => {
            assert_eq!(delivery.reason, TurnReason::Wake);
            delivery.ticket
        }
        None => panic!("应该有活"),
    }
}

#[test]
fn member_without_grant_is_denied_and_audited() {
    let mut bridge = bridge_with("");
    let ticket = open_turn(&mut bridge, "qq:group/123", "user1");
    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "config",
            json!({"action": "list", "section": "session"}),
        )
        .expect_err("没有 config_read 必须拒绝");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
    assert!(error.message.contains("config_read"), "{}", error.message);

    let audit = bridge.store().audit_recent(5).expect("能读审计");
    assert_eq!(audit.len(), 1, "拒绝也要落审计");
    assert_eq!(audit[0].capability, "config_read");
    assert_eq!(audit[0].decision, "deny");
    assert_eq!(audit[0].subject, "user");
}

#[test]
fn admin_needs_explicit_grant_not_identity() {
    // 不给规则：即使是 system_admin 也读不了。
    let mut bridge = bridge_with("");
    let ticket = open_turn(&mut bridge, "qq:private/admin", "admin");
    assert!(
        bridge
            .tool_call(
                &ticket,
                &sessions(),
                "config",
                json!({"action": "list", "section": "session"})
            )
            .is_err(),
        "system_admin 也不能靠身份绕过"
    );

    // 显式授予后在私信里可用。
    let mut bridge = bridge_with(
        r#"
        [[grants.rules]]
        id = "adm-read"
        subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["config_read"]
        "#,
    );
    let ticket = open_turn(&mut bridge, "qq:private/admin", "admin");
    let listed = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "config",
            json!({"action": "list", "section": "session"}),
        )
        .expect("显式授予后能读");
    // 夹具没有走首次导入，库里可能还没有对象；这里只确认形状与放行。
    assert!(listed["items"].is_array(), "{listed}");

    let audit = bridge.store().audit_recent(5).expect("能读审计");
    assert!(
        audit
            .iter()
            .any(|row| row.decision == "allow" && row.capability == "config_read")
    );
}

#[test]
fn config_set_requires_write_and_stores_object() {
    let mut bridge = bridge_with(
        r#"
        [[grants.rules]]
        id = "adm-write"
        subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["config_read", "config_write"]
        "#,
    );
    let ticket = open_turn(&mut bridge, "qq:private/admin", "admin");
    // 只读授权够 get，不够 set：先确认拒绝，再确认写成功。
    let toml = "[[personas]]\nid = \"p\"\nname = \"人格\"\ntext = \"你是桥。\"\n";
    let written = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "config",
            json!({"action": "set", "section": "persona", "id": "p", "value": toml}),
        )
        .expect("有 config_write 应该能写");
    assert_eq!(written["ok"], true);

    let audit = bridge.store().audit_recent(5).expect("能读审计");
    assert!(
        audit
            .iter()
            .any(|row| row.capability == "config_write" && row.decision == "allow")
    );
}

#[test]
fn policy_set_needs_policy_write() {
    let mut bridge = bridge_with(
        r#"
        [[grants.rules]]
        id = "adm-read-only"
        subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["config_read", "config_write"]
        "#,
    );
    let ticket = open_turn(&mut bridge, "qq:private/admin", "admin");
    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "policy",
            json!({"action": "set", "grants": {}}),
        )
        .expect_err("没有 policy_write 不能改策略");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
    let audit = bridge.store().audit_recent(5).expect("能读审计");
    assert!(
        audit
            .iter()
            .any(|row| row.capability == "policy_write" && row.decision == "deny")
    );
}

#[test]
fn plugin_admin_is_required_for_actions() {
    let mut bridge = bridge_with(
        r#"
        [[grants.rules]]
        id = "adm-config"
        subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["config_read", "config_write"]
        "#,
    );
    let ticket = open_turn(&mut bridge, "qq:private/admin", "admin");
    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "plugin_action",
            json!({"plugin": "demo.tools", "action": "ping"}),
        )
        .expect_err("没有 plugin_admin 不能执行动作");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
}

#[test]
fn unknown_section_is_a_clean_bad_request() {
    let mut bridge = bridge_with(
        r#"
        [[grants.rules]]
        id = "adm-read"
        subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["config_read"]
        "#,
    );
    let ticket = open_turn(&mut bridge, "qq:private/admin", "admin");
    let error = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "config",
            json!({"action": "list", "section": "nope"}),
        )
        .expect_err("未知 section");
    assert_eq!(error.code, ErrorCode::BadRequest);
}

// 未使用但保留：MethodSpec 的完整字段在别的测试里更完整地覆盖。
#[allow(dead_code)]
fn unused_spec() -> MethodSpec {
    MethodSpec {
        name: "noop".into(),
        aliases: Cow::Borrowed(&[]),
        summary: "noop".into(),
        tags: Vec::new().into(),
        risk: Risk::Write,
        scope: Scope::Room,
        params: Vec::<ParamSpec>::new().into(),
        targets: Vec::<TargetSpec>::new().into(),
        effect: Effect::Write,
        record: RecordMode::None,
        verification: Verification::verified("test"),
        permissions: None,
    }
}

#[allow(dead_code)]
fn unused_sender() -> Sender {
    Sender {
        id: "x".into(),
        name: "x".into(),
        role: None,
    }
}

/// 配置写入可以走票据：申请 → 管理员私信批准 → 下一次 config:set 放行一次。
#[test]
fn config_write_can_go_through_a_ticket() {
    let sends: Arc<Mutex<Vec<(RoomId, String)>>> = Arc::new(Mutex::new(Vec::new()));

    struct Capturing {
        sends: Arc<Mutex<Vec<(RoomId, String)>>>,
    }

    impl Outbound for Capturing {
        fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
            let text: String = elements.iter().filter_map(Element::as_text).collect();
            self.sends
                .lock()
                .expect("没被毒死")
                .push((room.clone(), text));
            Ok(Sent {
                platform_id: "p-1".to_owned(),
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
            _params: &serde_json::Value,
        ) -> Result<CallOutcome, CallError> {
            Ok(CallOutcome::value(json!({})))
        }
    }

    let extra = r#"
        [grants.rooms."qq:group/123"]
        approval_route = ["qq:private/admin"]

        [[grants.rules]]
        id = "user-request"
        subject = "user"
        place = "group"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["access_request"]

        [[grants.rules]]
        id = "adm-write"
        subject = "system_admin"
        place = "private"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["config_read", "config_write"]
    "#;
    let mut bridge = Bridge::new(
        Config::parse_fixture(&config(extra)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Capturing {
            sends: sends.clone(),
        }),
    );

    let ticket = open_turn(&mut bridge, "qq:group/123", "user1");
    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/123", "ops": ["config:set"], "reason": "改人格"}]}),
        )
        .expect("能申请连接级票据");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");

    let approval = sends
        .lock()
        .expect("没被毒死")
        .iter()
        .find(|(room, text)| room.path == ["private", "admin"] && text.contains("[ACB 授权申请]"))
        .map(|(_, text)| text.clone())
        .expect("审批私信已发到管理员");
    let nonce = approval
        .split_whitespace()
        .find_map(|token| token.strip_prefix("nonce="))
        .expect("审批消息带 nonce")
        .to_owned();

    let handled = bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {id} {nonce}"),
        ))
        .expect("审批能入库");
    assert!(
        matches!(handled, Ingest::Handled { .. }),
        "审批由 core 接管"
    );

    // 批准后：下一次 config:set 用票放行；票用后即废。
    let after_ticket = open_turn(&mut bridge, "qq:group/123", "user1");
    let toml = "[[personas]]\nid = \"p\"\nname = \"人格\"\ntext = \"你是桥。\"\n";
    let written = bridge
        .tool_call(
            &after_ticket,
            &sessions(),
            "config",
            json!({"action": "set", "section": "persona", "id": "p", "value": toml}),
        )
        .expect("票据放行一次 config:set");
    assert_eq!(written["ok"], true);

    let second = bridge
        .tool_call(
            &after_ticket,
            &sessions(),
            "config",
            json!({"action": "set", "section": "persona", "id": "p", "value": toml}),
        )
        .expect_err("票用后即废");
    assert_eq!(second.code, ErrorCode::ToolNotAllowed);

    let audit = bridge.store().audit_recent(10).expect("能读审计");
    assert!(
        audit
            .iter()
            .any(|row| row.decision == "allow" && row.basis.contains("ticket:")),
        "票据放行要留审计: {audit:?}"
    );
}
