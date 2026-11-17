//! 策略以库为准：首次导入、库覆盖文件、改完在轮次边界生效。
//!
//! 这条链是 2026-11-09 事故的结构性修复：审批路由以前只能手写 `config.toml`，
//! 漏配一个群没有任何提示，票据建出去就一路 pending 到过期（路由是建票时的快照，
//! 事后补配置也救不回来）。现在它是一条库记录，改完下一次开轮就生效。

use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Config, Element, GrantCapability, GrantRule, GrantScope, GrantSubject, GrantsConfig,
    MemoryStore, Outbound, OutboundError, Place, Policies, RecordDraft, RoomGrant, RoomId,
    RoomScope, Sender, Sent,
};
use serde_json::Value;

const AT: i64 = 1_793_000_000_000;

struct Noop;

impl Outbound for Noop {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p-1".to_owned(),
            at: AT,
        })
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
        token = { secret = "t-main" }

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = true

        [tickets]
        request_cooldown_secs = 0
        admin_cooldown_secs = 0

        [grants.rooms."qq:group/123"]
        approval_route = ["qq:private/admin"]

        [[grants.rules]]
        id = "fixture-allow-read"
        subject = "user"
        place = "group"
        scope = "subscribed"
        capability = ["room_read"]
    "#
}

fn bridge_with(store: MemoryStore) -> Bridge<MemoryStore> {
    Bridge::new(
        Config::parse_fixture(config()).expect("配置合法"),
        store,
        Box::new(Noop),
    )
}

fn message(room: &str, text: &str) -> RecordDraft {
    let room = RoomId::parse(room).expect("房间合法");
    RecordDraft::message(
        room,
        Sender {
            id: "323622442".into(),
            name: "萨卡帕甲鱼".into(),
            role: None,
        },
        AT,
        vec![Element::text(text)],
        false,
    )
    .with_room_scope(RoomScope::MultiParty)
}

#[test]
fn first_start_imports_config_grants_into_store() {
    let bridge = bridge_with(MemoryStore::new());

    let stored = bridge
        .store()
        .policy_load()
        .expect("可读")
        .expect("首次启动要把 config.toml 的 [grants] 导进库；否则重启后权限来源就断了两处");
    assert_eq!(
        stored,
        bridge.config().grants,
        "导入的必须和内存里正在用的那份一致"
    );
    assert_eq!(
        stored.rooms["qq:group/123"].approval_route,
        vec!["qq:private/admin".to_owned()]
    );
    assert_eq!(stored.rules.len(), 1);
}

#[test]
fn store_wins_over_config_once_it_has_content() {
    // 库里已经有一条不一样的审批路由（模拟「上次在控制台改过」）。
    let mut store = MemoryStore::new();
    let mut seeded = GrantsConfig::default();
    seeded.rooms.insert(
        "qq:group/123".to_owned(),
        RoomGrant {
            trust: Default::default(),
            members: vec![],
            approval_route: vec!["qq:group/498587179".to_owned()],
        },
    );
    seeded.rules.push(GrantRule {
        id: "from-store".to_owned(),
        subject: GrantSubject::User,
        place: Place::Room(RoomScope::MultiParty),
        scope: GrantScope::Subscribed,
        target: None,
        capability: vec![GrantCapability::RoomRead],
        permissions: Vec::new(),
    });
    store.policy_replace(&seeded).expect("能写");

    let bridge = bridge_with(store);
    assert_eq!(
        bridge.config().grants,
        seeded,
        "库里有策略时以库为准：config.toml 的 [grants] 只是导入源，不该覆盖库里的修改"
    );
    assert_eq!(
        bridge.config().grants.rooms["qq:group/123"].approval_route,
        vec!["qq:group/498587179".to_owned()],
        "审批路由尤其不能被文件顶回去——这是「补配置救不回旧票」的那个字段"
    );
}

#[test]
fn policy_edit_lands_at_the_next_turn_boundary() {
    let mut bridge = bridge_with(MemoryStore::new());
    assert!(!bridge.has_pending_policy_reload());

    let mut next = bridge.config().grants.clone();
    next.rooms.insert(
        "qq:group/769843907".to_owned(),
        RoomGrant {
            trust: Default::default(),
            members: vec![],
            approval_route: vec!["qq:private/323622442".to_owned()],
        },
    );
    bridge.replace_policy(&next).expect("能写");

    assert!(
        bridge.has_pending_policy_reload(),
        "改库之后要挂上「等轮次边界」的标记，不能当场生效——一轮里可能判定多次"
    );
    assert!(
        !bridge
            .config()
            .grants
            .rooms
            .contains_key("qq:group/769843907"),
        "标记还没到边界，当前这一轮的策略快照不能变"
    );

    // 收一条消息就是轮次边界（ingest 会应用暂存的策略）。
    bridge
        .ingest(message("qq:group/123", "在吗"))
        .expect("能入库");

    assert!(!bridge.has_pending_policy_reload(), "边界过了标记要清掉");
    assert_eq!(
        bridge.config().grants.rooms["qq:group/769843907"].approval_route,
        vec!["qq:private/323622442".to_owned()],
        "新票据要按新路由投递"
    );
}

#[test]
fn invalid_policy_is_rejected_before_it_reaches_the_store() {
    let mut bridge = bridge_with(MemoryStore::new());
    let before = bridge.store().policy_load().expect("可读");

    // 空 connection 的 system_admin 通不过校验。
    let mut bad = GrantsConfig::default();
    bad.system_admin = Some(acb_kernel::SystemAdminGrant {
        connection: String::new(),
        account: "323622442".to_owned(),
    });

    assert!(bridge.replace_policy(&bad).is_err(), "坏策略必须拒绝保存");
    assert_eq!(
        bridge.store().policy_load().expect("可读"),
        before,
        "被拒绝的保存不能留下半份数据——那样连回滚都没法做"
    );
    assert!(!bridge.has_pending_policy_reload());
}

/// 序列化往返：落库的是 JSON，形状必须和 `config.toml` 那一份完全一致，
/// 否则「读回来再存回去」会悄悄丢字段。
#[test]
fn policy_json_round_trip_keeps_every_field() {
    let config = Config::parse_fixture(config()).expect("配置合法");
    let text = serde_json::to_string(&config.grants).expect("能序列化");
    let back: GrantsConfig = serde_json::from_str(&text).expect("能反序列化");
    assert_eq!(back, config.grants);

    let value: Value = serde_json::from_str(&text).expect("是合法 JSON");
    assert_eq!(
        value["rooms"]["qq:group/123"]["approval_route"][0],
        "qq:private/admin"
    );
}

/// 便于上面的测试共享一份「有人用过」的存储；也顺带钉住 trait 对象可用性。
#[allow(dead_code)]
fn shared(store: MemoryStore) -> Arc<Mutex<MemoryStore>> {
    Arc::new(Mutex::new(store))
}

/// 控制台读策略必须看到**刚保存的那份**，即使还没到轮次边界。
///
/// 两条语义同时成立：
///   - 运行时 `config().grants` 要等轮次边界才换（一轮里可能判定多次）
///   - 控制台要立刻看到库里的新值（用户刚点过保存）
///
/// 2026-11-17 用户要求「数据库存的和页面上显示的保持一致」。实现上分成两个
/// 入口：`replace_policy` 只写库 + 挂边界标记；`stored_policy` 直接读库给控制台。
#[test]
fn console_sees_the_saved_policy_before_the_turn_boundary() {
    let mut bridge = bridge_with(MemoryStore::new());

    let mut next = bridge.config().grants.clone();
    next.rooms.insert(
        "qq:group/769843907".to_owned(),
        RoomGrant {
            trust: Default::default(),
            members: vec![],
            approval_route: vec!["qq:private/323622442".to_owned()],
        },
    );
    bridge.replace_policy(&next).expect("能写");

    // 运行时快照还没换（边界未到）……
    assert!(
        !bridge
            .config()
            .grants
            .rooms
            .contains_key("qq:group/769843907"),
        "运行时策略仍要等边界"
    );
    // ……但控制台读库能看到。
    let (stored, from_store) = bridge.stored_policy();
    assert!(from_store, "应当真的读到了库");
    assert!(
        stored.rooms.contains_key("qq:group/769843907"),
        "控制台必须看到刚保存的规则，否则保存成功但页面显示旧数据"
    );
}
