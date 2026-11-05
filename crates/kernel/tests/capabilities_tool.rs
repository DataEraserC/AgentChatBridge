//! `capabilities` 只读自检：复用现有判定，不消费票据，不泄露别的会话。

use std::sync::Arc;

use acb_kernel::engine::TICKET_OPS;
use acb_kernel::{
    AccessAction, AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Bridge, CallError,
    CallOutcome, Capability, Config, Effect, Element, ExtensionPoint, MemoryStore, MethodSpec,
    Outbound, OutboundError, PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId,
    RecordDraft, RecordMode, Risk, RoomId, Scope, Sender, Sent, TOOL_NAMES,
};
use serde_json::{Value, json};

const AT: i64 = 1_793_000_000_000;

struct Recorder {
    catalog: Arc<[MethodSpec]>,
}

impl Outbound for Recorder {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p-1".to_owned(),
            at: AT,
        })
    }

    fn catalog(&self, connection: &str) -> Arc<[MethodSpec]> {
        if connection == "qq" {
            self.catalog.clone()
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
        Ok(CallOutcome::value(json!({"ok": true})))
    }
}

fn catalog() -> Arc<[MethodSpec]> {
    let mut methods = Vec::new();
    for index in 0..116 {
        methods.push(MethodSpec {
            name: format!("fake.op{index}").into(),
            summary: "假 op".into(),
            tags: Vec::new().into(),
            risk: Risk::Read,
            scope: Scope::Room,
            params: Vec::new().into(),
            targets: Vec::new().into(),
            effect: Effect::Read,
            record: RecordMode::None,
        });
    }
    methods.push(MethodSpec {
        name: "fake.kick".into(),
        summary: "假破坏性 op".into(),
        tags: Vec::new().into(),
        risk: Risk::Destructive,
        scope: Scope::Room,
        params: Vec::new().into(),
        targets: Vec::new().into(),
        effect: Effect::Write,
        record: RecordMode::None,
    });
    methods.push(MethodSpec {
        name: "account.cookies".into(),
        summary: "假 critical op".into(),
        tags: Vec::new().into(),
        risk: Risk::Critical,
        scope: Scope::Connection,
        params: Vec::new().into(),
        targets: Vec::new().into(),
        effect: Effect::Read,
        record: RecordMode::None,
    });
    methods.into()
}

struct ReadOnly;

impl AccessProvider for ReadOnly {
    fn check(&self, _options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        match request.action {
            AccessAction::Connection | AccessAction::Read => AccessDecision::Allow,
            AccessAction::Send => AccessDecision::Deny,
        }
    }
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

        [sessions.access]
        provider = "test.read_only"

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

fn sessions() -> Vec<String> {
    vec!["main".to_owned()]
}

fn message(room: &str, sender: &str, text: &str) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse(room).expect("房间合法"),
        Sender {
            id: sender.to_owned(),
            name: sender.to_owned(),
            role: None,
        },
        AT,
        vec![Element::text(text)],
        true,
    )
}

fn bridge() -> Bridge<MemoryStore> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_access(
            ProviderDescriptor::new(
                ProviderId::parse("test.read_only").expect("合法"),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            ),
            Arc::new(ReadOnly),
        )
        .expect("能注册自定义 access provider");
    Bridge::with_plugins(
        Config::parse(config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder { catalog: catalog() }),
        plugins,
    )
}

fn open_turn(bridge: &mut Bridge<MemoryStore>, room: &str) -> acb_kernel::Ticket {
    bridge
        .ingest(message(room, "10001", "在吗"))
        .expect("能记下来");
    bridge.poll_turn().expect("能开轮").expect("有活").ticket
}

#[test]
fn session_scope_summarizes_tools_rooms_and_tickets() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "session"}),
        )
        .expect("能查");

    assert_eq!(value["scope"], "session");
    assert_eq!(value["counts"]["tools_total"], TOOL_NAMES.len());
    let tools = value["tools"].as_array().expect("tools 是数组");
    assert_eq!(tools.len(), TOOL_NAMES.len(), "29 个工具都要列出来");
    let capabilities = tools
        .iter()
        .find(|tool| tool["tool"] == "capabilities")
        .expect("capabilities 自己也在清单里");
    assert_eq!(capabilities["available"], true);
    let extensions = tools
        .iter()
        .find(|tool| tool["tool"] == "extensions_list")
        .expect("有 extensions_list");
    assert_eq!(extensions["available"], false);
    assert_eq!(extensions["reason"], "admin_only");
    let send = tools
        .iter()
        .find(|tool| tool["tool"] == "send_message")
        .expect("有 send_message");
    assert_eq!(send["available"], false);
    assert_eq!(send["reason"], "forbidden", "订阅但 provider 拒绝写");

    let rooms = value["rooms"].as_array().expect("rooms 是数组");
    assert_eq!(rooms.len(), 1);
    assert_eq!(rooms[0]["room"], "qq:group/123");
    assert_eq!(rooms[0]["subscribed"], true);
    assert_eq!(rooms[0]["readable"], true);
    assert_eq!(rooms[0]["sendable"], false, "订阅不等于可写");

    let tickets = &value["tickets"];
    assert_eq!(tickets["live"], 0);
    assert_eq!(tickets["pending"], 0);
    assert_eq!(tickets["active"], 0);
    assert_eq!(tickets["max_live"], 8);
    assert_eq!(tickets["max_turn_groups"], 3);
    assert!(tickets["pending_ttl_ms"].as_i64().unwrap_or_default() > 0);
    assert!(tickets["active_ttl_ms"].as_i64().unwrap_or_default() > 0);
    let described: std::collections::BTreeSet<String> = tickets["ops"]
        .as_array()
        .expect("ops 是数组")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let kernel: std::collections::BTreeSet<String> = TICKET_OPS
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();
    assert_eq!(described, kernel, "可申请票据 op 必须等于内核词表");
}

#[test]
fn room_scope_separates_subscription_read_and_write() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");

    let subscribed = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "room", "room": "qq:group/123"}),
        )
        .expect("能查");
    assert_eq!(subscribed["subscribed"], true);
    assert_eq!(subscribed["readable"], true);
    assert_eq!(subscribed["sendable"], false);
    assert_eq!(subscribed["reason"], "forbidden");

    let ops = subscribed["ops"].as_array().expect("ops 是数组");
    assert_eq!(ops.len(), 118);
    assert_eq!(subscribed["indexed"], true, "118 个 op 必须索引降级");
    assert!(ops[0].get("example").is_none(), "索引形态不带 example");
    let kick = ops
        .iter()
        .find(|op| op["op"] == "fake.kick")
        .expect("有 fake.kick");
    assert_eq!(kick["risk"], "destructive");
    assert_eq!(kick["direct_executable"], false);
    assert_eq!(kick["reason"], "destructive_disabled");
    assert_eq!(kick["requestable"], false);
    let cookies = ops
        .iter()
        .find(|op| op["op"] == "account.cookies")
        .expect("有 account.cookies");
    assert_eq!(cookies["risk"], "critical");
    assert_eq!(cookies["ticket_required"], true);
    assert_eq!(cookies["reason"], "critical_requires_ticket");
    assert_eq!(cookies["requestable"], true);

    let outside = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "room", "room": "qq:group/999"}),
        )
        .expect("能查");
    assert_eq!(outside["subscribed"], false);
    assert_eq!(
        outside["readable"], true,
        "这个测试 provider 对任意房间放行读：订阅不等于可读"
    );
    assert_eq!(outside["sendable"], false);
    assert_eq!(outside["reason"], "not_subscribed");
}

#[test]
fn tool_scope_reports_stable_reasons() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");

    let admin_only = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "extensions_list"}),
        )
        .expect("能查");
    assert_eq!(admin_only["kind"], "tool");
    assert_eq!(admin_only["available"], false);
    assert_eq!(admin_only["reason"], "admin_only");

    let blocked_send = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "send_message"}),
        )
        .expect("能查");
    assert_eq!(blocked_send["reason"], "forbidden");

    let destructive = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "fake.kick"}),
        )
        .expect("能查");
    assert_eq!(destructive["kind"], "op");
    assert_eq!(destructive["reason"], "destructive_disabled");
    assert_eq!(destructive["direct_executable"], false);

    let critical = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "account.cookies"}),
        )
        .expect("能查");
    assert_eq!(critical["reason"], "critical_requires_ticket");
    assert_eq!(critical["ticket_required"], true);

    let unknown = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "no.such.op"}),
        )
        .expect("未知 op 是查询结果，不是工具错误");
    assert_eq!(unknown["found"], false);
    assert_eq!(unknown["reason"], "unknown_op");

    let notes = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "notes"}),
        )
        .expect("能查");
    assert_eq!(notes["available"], true);
}

#[test]
fn ticket_counts_do_not_leak_other_sessions() {
    let mut bridge = bridge();
    let other = open_turn(&mut bridge, "qq:group/456");
    let request = bridge
        .tool_call(
            &other,
            &["other".to_owned()],
            "access_request",
            json!({"items": [{"target": "qq:group/999", "ops": ["notes:list"]}]}),
        )
        .expect("other 能申请");
    let other_id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&other, None).expect("收工");

    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "session"}),
        )
        .expect("能查");
    assert_eq!(value["tickets"]["live"], 0, "不能把别的会话的票据算进来");
    assert_eq!(value["tickets"]["pending"], 0);
    assert!(
        !value.to_string().contains(&other_id),
        "响应里不能出现别的会话的票据 id：{value}"
    );
}
