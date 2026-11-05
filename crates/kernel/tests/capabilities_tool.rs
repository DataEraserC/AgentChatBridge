//! `capabilities` 只读自检：复用现有判定，不消费票据，不泄露别的会话。

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use acb_kernel::engine::TICKET_OPS;
use acb_kernel::{
    AccessAction, AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Bridge, CallError,
    CallOutcome, Capability, Config, Effect, Element, ExtensionPoint, MemoryStore, MethodSpec,
    ON_DEMAND_TOOLS, Outbound, OutboundError, PLUGIN_API_VERSION, PluginManager,
    ProviderDescriptor, ProviderId, RESIDENT_TOOLS, RecordDraft, RecordMode, Risk, RoomId, Scope,
    Sender, Sent, TOOL_NAMES, Verification,
};
use serde_json::{Value, json};

const AT: i64 = 1_793_000_000_000;

/// `capabilities scope=surface` 是每轮都要拉的内部视图。修改工具目录或
/// purpose 文案时，回到这里确认没把每轮成本推上去。
const SURFACE_BYTE_BUDGET: usize = 2048;

/// 加 `view` 之前的 surface 输出，逐字节锁住默认行为。
const LEGACY_SURFACE: &str = r#"{"catalog":[{"purpose":"翻房间历史记录，补齐上下文","tool":"get_history"},{"purpose":"按水位编号精确回看记录","tool":"fetch_records"},{"purpose":"阻塞等同房间下一条消息","tool":"wait_message"},{"purpose":"房间共享笔记（list/set/del）","tool":"notes"},{"purpose":"查最近平台事件","tool":"get_platform_events"},{"purpose":"看本会话的策略 provider","tool":"describe_plugins"},{"direct_executable":false,"reason":"admin_only","requestable":false,"ticket_required":false,"tool":"extensions_list"},{"direct_executable":false,"reason":"admin_only","requestable":false,"ticket_required":false,"tool":"extensions_read"},{"direct_executable":false,"reason":"admin_only","requestable":false,"ticket_required":false,"tool":"extensions_write"},{"direct_executable":false,"reason":"admin_only","requestable":false,"ticket_required":false,"tool":"extensions_reload"},{"purpose":"查后台任务（list/get）","tool":"tasks"},{"purpose":"记录卡点反馈","tool":"report_feedback"},{"purpose":"查反馈记录（list/get）","tool":"feedback"},{"purpose":"回复反馈","tool":"feedback_reply"},{"purpose":"列插件工具","tool":"plugin_tools"},{"purpose":"调用插件工具","tool":"plugin_tool_call"},{"purpose":"读记忆文件","tool":"memory_read"},{"purpose":"写记忆文件","tool":"memory_write"},{"purpose":"列记忆目录","tool":"memory_list"},{"purpose":"搜记忆文本","tool":"memory_search"},{"purpose":"记忆 git（status/log/diff/add/commit/push）","tool":"memory_git"}],"counts":{"catalog":21,"hidden":6,"resident":8,"tools_total":29,"visible":23},"place":"group","resident":["send_message","send_message_to","list_rooms","capabilities","platform_call","access_request","access_ticket_get","ack_messages"],"room":"qq:group/123","scope":"surface","session":"main","subject":"user"}"#;

type Sends = Arc<Mutex<Vec<(RoomId, String)>>>;

struct Recorder {
    catalog: Arc<[MethodSpec]>,
    sends: Sends,
}

impl Outbound for Recorder {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        let text = elements
            .iter()
            .filter_map(Element::as_text)
            .collect::<String>();
        self.sends
            .lock()
            .expect("没被毒死")
            .push((room.clone(), text));
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
            aliases: if index == 0 {
                vec![Cow::Borrowed("fake.alias")].into()
            } else {
                Cow::Borrowed(&[])
            },
            summary: "假 op".into(),
            tags: Vec::new().into(),
            risk: Risk::Read,
            scope: Scope::Room,
            params: Vec::new().into(),
            targets: Vec::new().into(),
            effect: Effect::Read,
            record: RecordMode::None,
            verification: Verification::verified("capabilities test fake"),
        });
    }
    methods.push(MethodSpec {
        name: "fake.kick".into(),
        aliases: Cow::Borrowed(&[]),
        summary: "假破坏性 op".into(),
        tags: Vec::new().into(),
        risk: Risk::Destructive,
        scope: Scope::Room,
        params: Vec::new().into(),
        targets: Vec::new().into(),
        effect: Effect::Write,
        record: RecordMode::None,
        verification: Verification::verified("capabilities test fake"),
    });
    methods.push(MethodSpec {
        name: "account.cookies".into(),
        aliases: Cow::Borrowed(&[]),
        summary: "假 critical op".into(),
        tags: Vec::new().into(),
        risk: Risk::Critical,
        scope: Scope::Connection,
        params: Vec::new().into(),
        targets: Vec::new().into(),
        effect: Effect::Read,
        record: RecordMode::None,
        verification: Verification::verified("capabilities test fake"),
    });
    methods.push(MethodSpec {
        name: "fake.unverified".into(),
        aliases: Cow::Borrowed(&[]),
        summary: "假 unverified op".into(),
        tags: Vec::new().into(),
        risk: Risk::Write,
        scope: Scope::Room,
        params: Vec::new().into(),
        targets: Vec::new().into(),
        effect: Effect::Write,
        record: RecordMode::None,
        verification: Verification::unverified("只有单一来源", "跑一次实验"),
    });
    methods.push(MethodSpec {
        name: "system.restart".into(),
        aliases: Cow::Borrowed(&[]),
        summary: "假重启 op：Critical + Unverified".into(),
        tags: Vec::new().into(),
        risk: Risk::Critical,
        scope: Scope::Connection,
        params: Vec::new().into(),
        targets: Vec::new().into(),
        effect: Effect::Read,
        record: RecordMode::None,
        verification: Verification::unverified(
            "platform self-declared unsupported: 平台自述不支持重启",
            "隔离实例调用并记录 retcode",
        ),
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

/// 读写都拒绝，用来制造「被过滤但仍可申请」的长尾工具（notes 等）。
struct DenyRead;

impl AccessProvider for DenyRead {
    fn check(&self, _options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        match request.action {
            AccessAction::Connection => AccessDecision::Allow,
            AccessAction::Read | AccessAction::Send => AccessDecision::Deny,
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

        [[grants.rules]]
        subject = "user"
        place = "group"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["access_request"]
    "#
}

fn sessions() -> Vec<String> {
    vec!["main".to_owned()]
}

/// 读写都拒绝的会话 + 可达的审批私信：用来验证「被 hidden 的工具仍能申请并批准」。
fn deny_read_config() -> &'static str {
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
        provider = "test.deny_read"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = true

        # 审批目标房间必须被某个会话订阅，路由才算可达。
        [[sessions.subscriptions]]
        connection = "qq"
        room = "private/admin"

        [tickets]
        request_cooldown_secs = 0
        admin_cooldown_secs = 0

        [grants.system_admin]
        connection = "qq"
        account = "admin"

        [grants.rooms."qq:group/123"]
        approval_route = ["qq:private/admin"]

        [[grants.rules]]
        subject = "user"
        place = "group"
        scope = "connection"
        target = { kind = "connection", id = "qq" }
        capability = ["access_request"]
    "#
}

fn approval_dm(sends: &Sends) -> String {
    sends
        .lock()
        .expect("没被毒死")
        .iter()
        .find(|(room, text)| {
            room.connection == "qq"
                && room.path == ["private", "admin"]
                && text.contains("[ACB 授权申请]")
        })
        .map(|(_, text)| text.clone())
        .expect("审批消息已发到管理员私信")
}

fn field(text: &str, name: &str) -> String {
    text.split_whitespace()
        .find_map(|token| token.strip_prefix(name))
        .unwrap_or_else(|| panic!("审批消息里没有 {name}: {text}"))
        .to_owned()
}

fn message(room: &str, sender: &str, text: &str) -> RecordDraft {
    let room = RoomId::parse(room).expect("房间合法");
    // 适配器声明结构事实；测试里按路径模拟，不把它塞回 core。
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

fn bridge() -> Bridge<MemoryStore> {
    bridge_with_config(config())
}

fn bridge_with_config(text: &str) -> Bridge<MemoryStore> {
    bridge_with_provider(text, "test.read_only", Arc::new(ReadOnly)).0
}

/// 审批流需要看实际发出去的私信，所以顺带把 sends 交回给测试。
fn bridge_with_provider(
    text: &str,
    provider_id: &str,
    provider: Arc<dyn AccessProvider>,
) -> (Bridge<MemoryStore>, Sends) {
    let sends: Sends = Arc::new(Mutex::new(Vec::new()));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_access(
            ProviderDescriptor::new(
                ProviderId::parse(provider_id).expect("合法"),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            ),
            provider,
        )
        .expect("能注册自定义 access provider");
    let bridge = Bridge::with_plugins(
        Config::parse(text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder {
            catalog: catalog(),
            sends: sends.clone(),
        }),
        plugins,
    );
    (bridge, sends)
}

fn open_turn(bridge: &mut Bridge<MemoryStore>, room: &str) -> acb_kernel::Ticket {
    bridge
        .ingest(message(room, "10001", "在吗"))
        .expect("能记下来");
    bridge.poll_turn().expect("能开轮").expect("有活").ticket
}

/// surface 不再重复列 `visible`：可见集 = 常驻 ∪ catalog 里不带
/// `direct_executable=false` 且不是按需注入的长尾工具。
fn visible_tools(value: &Value) -> std::collections::BTreeSet<String> {
    let resident = value["resident"]
        .as_array()
        .expect("resident")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned);
    let catalog = value["catalog"]
        .as_array()
        .expect("catalog")
        .iter()
        .filter(|entry| entry["direct_executable"] != false)
        .filter_map(|entry| entry["tool"].as_str())
        .filter(|tool| !ON_DEMAND_TOOLS.contains(tool))
        .map(str::to_owned);
    resident.chain(catalog).collect()
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
    assert_eq!(capabilities["requestable"], false, "能力自检本身不申请");
    assert_eq!(capabilities["layer"], "resident");
    let extensions = tools
        .iter()
        .find(|tool| tool["tool"] == "extensions_list")
        .expect("有 extensions_list");
    assert_eq!(extensions["available"], false);
    assert_eq!(extensions["reason"], "admin_only");
    assert_eq!(extensions["requestable"], false, "admin 工具不在票据词表");
    assert_eq!(extensions["layer"], "catalog");
    let send = tools
        .iter()
        .find(|tool| tool["tool"] == "send_message")
        .expect("有 send_message");
    assert_eq!(send["available"], false);
    assert_eq!(send["reason"], "forbidden", "订阅但 provider 拒绝写");
    assert_eq!(send["requestable"], true, "被过滤仍可用 TICKET_OPS 申请");
    assert_eq!(send["ticket_required"], true);
    assert_eq!(send["layer"], "resident", "被过滤不改变常驻层");

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

/// 重启是 Critical，但平台自述不支持：capabilities 必须按 unverified 处理，
/// 不能因为 Critical 就把它显示成可直接执行或 verified。
#[test]
fn critical_unverified_restart_is_ticket_only() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "system.restart"}),
        )
        .expect("能查");
    assert_eq!(value["kind"], "op");
    assert_eq!(value["risk"], "critical");
    assert_eq!(value["verification"]["state"], "unverified");
    assert_eq!(value["direct_executable"], false);
    assert_eq!(value["ticket_required"], true);
    assert_eq!(value["reason"], "unverified_requires_ticket");
    assert_eq!(value["requestable"], true);
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
    assert_eq!(ops.len(), 120);
    assert_eq!(subscribed["indexed"], true, "120 个 op 必须索引降级");
    assert!(ops[0].get("example").is_none(), "索引形态不带 example");
    let kick = ops
        .iter()
        .find(|op| op["op"] == "fake.kick")
        .expect("有 fake.kick");
    assert_eq!(kick["risk"], "destructive");
    assert_eq!(kick["direct_executable"], false);
    assert_eq!(kick["reason"], "destructive_requires_ticket");
    assert_eq!(kick["ticket_required"], true);
    assert_eq!(kick["requestable"], true);
    assert_eq!(kick["verification"]["state"], "verified");
    let cookies = ops
        .iter()
        .find(|op| op["op"] == "account.cookies")
        .expect("有 account.cookies");
    assert_eq!(cookies["risk"], "critical");
    assert_eq!(cookies["ticket_required"], true);
    assert_eq!(cookies["reason"], "critical_requires_ticket");
    assert_eq!(cookies["requestable"], true);
    let unverified = ops
        .iter()
        .find(|op| op["op"] == "fake.unverified")
        .expect("有 fake.unverified");
    assert_eq!(unverified["direct_executable"], false);
    assert_eq!(unverified["ticket_required"], true);
    assert_eq!(unverified["reason"], "unverified_requires_ticket");
    assert_eq!(unverified["verification"]["state"], "unverified");
    assert_eq!(unverified["verification"]["reason"], "只有单一来源");

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
    assert_eq!(admin_only["layer"], "catalog");
    assert_eq!(admin_only["requestable"], false);

    let blocked_send = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "send_message"}),
        )
        .expect("能查");
    assert_eq!(blocked_send["reason"], "forbidden");
    assert_eq!(blocked_send["layer"], "resident");
    assert_eq!(blocked_send["direct_executable"], false);
    assert_eq!(blocked_send["requestable"], true);
    assert_eq!(blocked_send["ticket_required"], true);

    let destructive = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "fake.kick"}),
        )
        .expect("能查");
    assert_eq!(destructive["kind"], "op");
    assert_eq!(destructive["reason"], "destructive_requires_ticket");
    assert_eq!(destructive["direct_executable"], false);
    assert_eq!(destructive["ticket_required"], true);
    assert_eq!(destructive["requestable"], true);

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
fn alias_lookups_normalize_to_the_main_op() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let alias = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "fake.alias"}),
        )
        .expect("能查");
    assert_eq!(alias["op"], "fake.op0", "别名必须归一化到主 op");
    assert_eq!(alias["kind"], "op");
    assert_eq!(alias["aliases"][0], "fake.alias", "主 op 要列出别名");

    let main = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "fake.op0"}),
        )
        .expect("能查");
    assert_eq!(main["op"], "fake.op0");
    assert_eq!(main["risk"], alias["risk"], "别名继承主 op 的 risk");
    assert_eq!(
        main["verification"], alias["verification"],
        "别名继承主 op 的 verification"
    );
}

#[test]
fn surface_splits_resident_catalog_and_hidden() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "surface"}),
        )
        .expect("能查");

    assert_eq!(value["scope"], "surface");
    eprintln!(
        "capabilities scope=surface bytes: {}",
        value.to_string().len()
    );
    assert_eq!(value["place"], "group");
    assert_eq!(value["counts"]["tools_total"], TOOL_NAMES.len());
    assert_eq!(value["counts"]["resident"], RESIDENT_TOOLS.len());
    assert_eq!(
        value["counts"]["catalog"],
        TOOL_NAMES.len() - RESIDENT_TOOLS.len()
    );

    let resident: std::collections::BTreeSet<&str> = value["resident"]
        .as_array()
        .expect("resident 是数组")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let expected: std::collections::BTreeSet<&str> = RESIDENT_TOOLS.iter().copied().collect();
    assert_eq!(resident, expected, "常驻层必须逐字等于显式白名单");

    let catalog = value["catalog"].as_array().expect("catalog 是数组");
    let visible: std::collections::BTreeSet<&str> = catalog
        .iter()
        .filter(|entry| entry["direct_executable"] != false)
        .filter_map(|entry| entry["tool"].as_str())
        .chain(RESIDENT_TOOLS.iter().copied())
        .collect();
    for tool in RESIDENT_TOOLS {
        assert!(visible.contains(tool), "常驻工具 {tool} 永远可见");
    }
    assert!(
        !visible.contains(&"extensions_list"),
        "admin-only 工具不进本轮可见集"
    );
    assert_eq!(catalog.len(), TOOL_NAMES.len() - RESIDENT_TOOLS.len());
    assert!(
        catalog
            .iter()
            .any(|entry| entry["tool"] == "extensions_list"),
        "被过滤工具仍要留在目录索引里"
    );
    assert!(
        !catalog.iter().any(|entry| entry["tool"] == "send_message"),
        "常驻工具不重复进目录层"
    );
    let wait = catalog
        .iter()
        .find(|entry| entry["tool"] == "wait_message")
        .expect("wait_message 在 catalog 里");
    assert!(
        wait["purpose"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "可执行条目要有用途"
    );
    assert!(
        wait.get("reason").is_none(),
        "可执行条目不该带权限字段：{wait}"
    );

    let extension = catalog
        .iter()
        .find(|entry| entry["tool"] == "extensions_list")
        .expect("extensions_list 在 catalog 里");
    assert_eq!(extension["reason"], "admin_only");
    assert_eq!(extension["direct_executable"], false);
    assert_eq!(extension["requestable"], false);
    assert_eq!(extension["ticket_required"], false);
    assert!(
        extension.get("purpose").is_none(),
        "被过滤条目本轮不注入，不必带用途：{extension}"
    );
    assert!(value["counts"]["hidden"].as_u64().unwrap_or(0) > 0);
}

#[test]
fn surface_response_stays_within_budget() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "surface"}),
        )
        .expect("能查");
    let bytes = value.to_string().len();
    eprintln!("capabilities scope=surface bytes: {bytes}");
    assert!(
        bytes <= SURFACE_BYTE_BUDGET,
        "surface 每轮都要拉：{bytes} 字节超出 {SURFACE_BYTE_BUDGET} 字节预算"
    );

    let resident = value["resident"].as_array().expect("resident 是数组");
    assert!(
        resident.iter().all(Value::is_string),
        "常驻层只给名字，不给 schema 或权限字段"
    );
    for entry in value["catalog"].as_array().expect("catalog 是数组") {
        let object = entry.as_object().expect("条目是对象");
        if object.get("direct_executable") == Some(&Value::Bool(false)) {
            assert!(object.contains_key("reason"), "不可直调条目必须给原因");
            assert!(
                object.contains_key("requestable") && object.contains_key("ticket_required"),
                "不可直调条目必须给申请状态：{entry}"
            );
        } else {
            assert!(
                object.contains_key("purpose"),
                "可执行条目要给用途：{entry}"
            );
            assert_eq!(
                object.len(),
                2,
                "可执行条目只保留 tool + purpose，其余字段去 scope=catalog：{entry}"
            );
        }
    }
}

#[test]
fn catalog_scope_is_the_long_tail_index() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "catalog"}),
        )
        .expect("能查");

    assert_eq!(value["scope"], "catalog");
    assert_eq!(
        value["resident"].as_array().expect("resident").len(),
        RESIDENT_TOOLS.len()
    );
    let tools = value["tools"].as_array().expect("tools 是数组");
    assert_eq!(tools.len(), TOOL_NAMES.len() - RESIDENT_TOOLS.len());
    assert!(
        tools.iter().all(|entry| {
            entry["tool"]
                .as_str()
                .is_some_and(|name| !RESIDENT_TOOLS.contains(&name))
        }),
        "目录层只放非驻留工具"
    );
    let extension = tools
        .iter()
        .find(|entry| entry["tool"] == "extensions_list")
        .expect("extensions_list 在目录里");
    assert_eq!(extension["reason"], "admin_only");
    assert!(
        extension["purpose"]
            .as_str()
            .is_some_and(|text| text.contains("扩展")),
        "用途要写清楚：{extension}"
    );
    assert!(
        extension["when"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "何时用不能空"
    );
}

#[test]
fn resident_entry_points_are_never_filtered_out() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "surface"}),
        )
        .expect("能查");
    let visible = visible_tools(&value);
    for tool in [
        "access_request",
        "access_ticket_get",
        "capabilities",
        "ack_messages",
    ] {
        assert!(
            visible.contains(tool),
            "入口/结束轮次工具 {tool} 永不被过滤"
        );
    }
}

#[test]
fn access_request_stays_visible_without_an_approver() {
    let text = config().replace(
        "        [grants.system_admin]\n        connection = \"qq\"\n        account = \"admin\"\n",
        "",
    );
    let mut bridge = bridge_with_config(&text);
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "surface"}),
        )
        .expect("能查");
    let visible = visible_tools(&value);
    assert!(
        visible.contains("access_request"),
        "没有审批人时 access_request 仍必须可见"
    );

    let detail = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "tool", "name": "access_request"}),
        )
        .expect("能查");
    assert_eq!(detail["available"], false);
    assert_eq!(detail["reason"], "no_approver");
    assert_eq!(detail["layer"], "resident");
    assert_eq!(detail["requestable"], false);
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

/// 默认 view 必须与加 `view` 之前逐字节一致；显式 `view=visible` 和只给 view
/// 都要落到同一份输出。
#[test]
fn surface_visible_view_matches_the_legacy_output() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    for args in [
        json!({"scope": "surface"}),
        json!({"scope": "surface", "view": "visible"}),
        json!({"view": "visible"}),
        // visible 不看分页参数：传了也不改变默认输出。
        json!({"scope": "surface", "view": "visible", "limit": 5, "offset": 3}),
    ] {
        let value = bridge
            .tool_call(&ticket, &sessions(), "capabilities", args.clone())
            .expect("能查");
        assert_eq!(
            value.to_string(),
            LEGACY_SURFACE,
            "view=visible 必须与旧输逐字节一致：args={args}"
        );
    }
}

#[test]
fn hidden_view_lists_permission_filtered_tools_with_reasons() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"view": "hidden"}),
        )
        .expect("只给 view 也按 surface 查");

    assert_eq!(value["scope"], "surface");
    assert_eq!(value["view"], "hidden");
    let tools = value["tools"].as_array().expect("tools 是数组");
    assert_eq!(tools.len(), 4, "默认配置只过滤掉 4 个 admin-only 工具");
    for entry in tools {
        assert!(
            entry["tool"]
                .as_str()
                .is_some_and(|tool| !RESIDENT_TOOLS.contains(&tool)),
            "常驻工具不会被过滤：{entry}"
        );
        assert!(
            ["place", "scope", "capability", "risk", "subscription"]
                .contains(&entry["why_hidden"].as_str().unwrap_or_default()),
            "why_hidden 必须是已知维度：{entry}"
        );
        assert!(entry["reason"].is_string(), "具体原因不能省：{entry}");
    }
    let extension = tools
        .iter()
        .find(|entry| entry["tool"] == "extensions_list")
        .expect("extensions_list 被过滤");
    assert_eq!(extension["why_hidden"], "capability");
    assert_eq!(extension["reason"], "admin_only");
    assert_eq!(extension["requestable"], false);
    assert_eq!(extension["ticket_required"], false);
    assert!(
        !tools
            .iter()
            .any(|entry| entry["tool"] == "get_history" || entry["tool"] == "fetch_records"),
        "按需层不是权限过滤，不该进 hidden"
    );
    assert_eq!(value["counts"]["permission_hidden"], 4);
    assert_eq!(value["counts"]["on_demand"], ON_DEMAND_TOOLS.len());
}

#[test]
fn all_view_exposes_every_tool_including_filtered_ones() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");
    let value = bridge
        .tool_call(&ticket, &sessions(), "capabilities", json!({"view": "all"}))
        .expect("能查");

    assert_eq!(value["scope"], "surface");
    assert_eq!(value["view"], "all");
    let tools = value["tools"].as_array().expect("tools 是数组");
    assert_eq!(tools.len(), TOOL_NAMES.len(), "all 必须是全集");
    let names: std::collections::BTreeSet<&str> = tools
        .iter()
        .filter_map(|entry| entry["tool"].as_str())
        .collect();
    let expected: std::collections::BTreeSet<&str> = TOOL_NAMES.iter().copied().collect();
    assert_eq!(names, expected, "all 必须逐字覆盖 TOOL_NAMES");

    let extension = tools
        .iter()
        .find(|entry| entry["tool"] == "extensions_list")
        .expect("全集里有 extensions_list");
    assert_eq!(extension["available"], false);
    assert_eq!(extension["why_hidden"], "capability");
    assert_eq!(extension["reason"], "admin_only");
    assert_eq!(extension["layer"], "catalog");

    let send = tools
        .iter()
        .find(|entry| entry["tool"] == "send_message")
        .expect("全集里有 send_message");
    assert_eq!(send["available"], false, "provider 拒绝写");
    assert_eq!(send["why_hidden"], "risk", "在票据词表里：审批可放行");
    assert_eq!(send["reason"], "forbidden");
    assert_eq!(send["requestable"], true);
    assert_eq!(send["ticket_required"], true);
    assert_eq!(send["layer"], "resident");

    let history = tools
        .iter()
        .find(|entry| entry["tool"] == "get_history")
        .expect("全集里有按需工具");
    assert_eq!(history["available"], true);
    assert!(history.get("why_hidden").is_none(), "可执行条目不带维度");

    assert_eq!(value["counts"]["tools_total"], TOOL_NAMES.len());
    assert_eq!(value["counts"]["resident"], RESIDENT_TOOLS.len());
    assert_eq!(value["counts"]["visible"], 23);
    assert_eq!(value["counts"]["permission_hidden"], 4);
    assert!(
        value.to_string().len() <= 4096,
        "all 视图必须受既有字节上限约束：{}",
        value.to_string().len()
    );
    if value["indexed"] == true {
        assert!(
            tools.iter().all(|entry| entry.get("purpose").is_none()),
            "索引化之后不该再带 purpose：{value}"
        );
    }
}

#[test]
fn all_and_hidden_views_page_within_the_byte_budget() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");

    let all = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"view": "all", "offset": 10, "limit": 5}),
        )
        .expect("能查");
    let tools = all["tools"].as_array().expect("tools 是数组");
    assert_eq!(tools.len(), 5, "显式 limit 生效");
    assert_eq!(all["returned"], 5);
    assert_eq!(all["_page"]["total"], TOOL_NAMES.len());
    assert_eq!(all["_page"]["offset"], 10);
    assert_eq!(all["_page"]["has_more"], true);
    assert_eq!(all["_page"]["next_offset"], 15);

    let hidden = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"view": "hidden", "limit": 2}),
        )
        .expect("能查");
    let hidden_tools = hidden["tools"].as_array().expect("tools 是数组");
    assert_eq!(hidden_tools.len(), 2, "hidden 同样受分页约束");
    assert_eq!(hidden["returned"], 2);
    assert_eq!(hidden["_page"]["total"], 4);
    assert_eq!(hidden["_page"]["has_more"], true);
    assert_eq!(hidden["_page"]["next_offset"], 2);
    assert!(hidden.to_string().len() <= 4096, "{hidden}");
}

/// 可见性与可申请性解耦：被 hidden 的工具仍能走票据申请，批准后照常执行。
#[test]
fn hidden_tool_can_still_be_requested_and_approved() {
    let (mut bridge, sends) =
        bridge_with_provider(deny_read_config(), "test.deny_read", Arc::new(DenyRead));
    let ticket = open_turn(&mut bridge, "qq:group/123");

    let hidden = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"view": "hidden"}),
        )
        .expect("能查");
    let notes = hidden["tools"]
        .as_array()
        .expect("tools 是数组")
        .iter()
        .find(|entry| entry["tool"] == "notes")
        .expect("notes 被权限过滤");
    assert_eq!(notes["why_hidden"], "risk");
    assert_eq!(notes["reason"], "forbidden");
    assert_eq!(notes["requestable"], true);
    assert_eq!(notes["ticket_required"], true);

    // 过滤只影响可见性：直接调用仍会被权限拒绝，而不是因为「看不见」才失败。
    assert!(
        bridge
            .tool_call(&ticket, &sessions(), "notes", json!({"op": "list"}))
            .is_err(),
        "没有票据时 notes 必须被拒绝"
    );

    let request = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "access_request",
            json!({"items": [{"target": "qq:group/123", "ops": ["notes:list"]}]}),
        )
        .expect("被 hidden 的工具仍可申请");
    let id = request["tickets"][0]["ticket"]
        .as_str()
        .expect("票据 id")
        .to_owned();
    bridge.end_turn(&ticket, None).expect("收工");

    let text = approval_dm(&sends);
    let nonce = field(&text, "nonce=");
    let handled = bridge
        .ingest(message(
            "qq:private/admin",
            "admin",
            &format!("/approve {id} {nonce}"),
        ))
        .expect("审批命令能入库");
    assert!(
        matches!(handled, acb_kernel::Ingest::Handled { .. }),
        "审批命令由 core 接管，不叫醒 agent"
    );

    let delivery = bridge.poll_turn().expect("能开轮").expect("有唤醒");
    let after = delivery.ticket;
    let listed = bridge
        .tool_call(&after, &sessions(), "notes", json!({"op": "list"}))
        .expect("票据放行 notes");
    assert_eq!(listed["room"]["connection"], "qq");
    assert_eq!(listed["room"]["path"][0], "group");
    assert_eq!(listed["room"]["path"][1], "123");
    assert_eq!(listed["total"], 0);
}

/// 只给 view 不给 scope、以及 view 与别的 scope 混用时的行为。
#[test]
fn view_only_applies_to_surface() {
    let mut bridge = bridge();
    let ticket = open_turn(&mut bridge, "qq:group/123");

    let implied = bridge
        .tool_call(&ticket, &sessions(), "capabilities", json!({"view": "all"}))
        .expect("只给 view 时按 surface");
    assert_eq!(implied["scope"], "surface");
    assert_eq!(implied["view"], "all");

    let conflict = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "session", "view": "hidden"}),
        )
        .expect_err("view 与其它 scope 混用要报参数错误");
    assert!(
        conflict.message.contains("[bad_params]"),
        "错误信息要明确：{}",
        conflict.message
    );

    let unknown = bridge
        .tool_call(
            &ticket,
            &sessions(),
            "capabilities",
            json!({"scope": "surface", "view": "everything"}),
        )
        .expect_err("未知 view 要报参数错误");
    assert!(
        unknown.message.contains("[bad_params]"),
        "错误信息要明确：{}",
        unknown.message
    );
}
