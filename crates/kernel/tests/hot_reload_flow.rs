//! 端到端：管理员保存新配置 → 暂存 → 下一次收消息/开轮时换上（ADR-0009 轮次边界重载）。
//!
//! 验证三件事：
//! 1. 暂存不立即生效（config_version 不动）；
//! 2. 收到新订阅房间的消息时自动换上新配置——新订阅的**第一条消息**就能开轮；
//! 3. 丢弃暂存后配置回到旧的。

use acb_kernel::{
    Bridge, Config, DependencyRegistry, Element, Ingest, MemoryStore, Outbound, OutboundError,
    RecordDraft, RoomId, Sender, Sent, TurnReason,
};
use serde_json::Value;

const AT: i64 = 1_793_000_000_000;

struct Recorder;

impl Outbound for Recorder {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "p".to_owned(),
            at: AT,
        })
    }
}

/// 基线配置：一个会话，只订 qq:group/123。
fn config_base() -> String {
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
        name = "DSH"

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        token = "t-main"

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        at_mention = false
        name_mention = false
        poke = false
        accumulate = 50
    "#
    .to_owned()
}

/// 新配置：同一个会话**多订**了一个房间 group/999。
/// 注意 TOML 语义：`[sessions.subscriptions.wake]` 挂在**最近一个** `[[sessions.subscriptions]]`
/// 上，所以新订阅要放在 wake 段之后——否则 wake 会从旧房间挪到新房间头上。
fn config_with_extra_room() -> String {
    config_base().replace(
        "accumulate = 50",
        concat!(
            "accumulate = 50\n",
            "\n",
            "        [[sessions.subscriptions]]\n",
            "        connection = \"qq\"\n",
            "        room = \"group/999\"",
        ),
    )
}

/// 新配置：连接 id 改成 qq2、harness id 改成 dsh2，其余引用同步改。
fn config_renamed() -> String {
    config_base()
        .replace("id = \"qq\"", "id = \"qq2\"")
        .replace("id = \"dsh\"", "id = \"dsh2\"")
        .replace("harness = \"dsh\"", "harness = \"dsh2\"")
        .replace("connection = \"qq\"", "connection = \"qq2\"")
}

fn message(room: &str, sender: &str, text: &str, at_self: bool) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse(room).expect("房间合法"),
        Sender {
            id: sender.to_owned(),
            name: "老王".to_owned(),
            role: None,
        },
        AT,
        vec![Element::text(text)],
        at_self,
    )
}

fn bridge(config_text: &str) -> Bridge<MemoryStore> {
    Bridge::new(
        acb_kernel::Config::parse(config_text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Recorder),
    )
}

#[test]
fn staged_config_applies_at_next_ingest_and_new_room_wakes() {
    let mut bridge = bridge(&config_base());
    assert_eq!(bridge.config_version(), 0);
    assert!(!bridge.has_staged_config());

    // 管理员保存新配置：暂存，不立即生效。
    bridge.stage_config(acb_kernel::Config::parse(&config_with_extra_room()).expect("新配置合法"));
    assert!(bridge.has_staged_config());
    assert_eq!(bridge.config_version(), 0, "暂存不换版本，还没生效");

    // 新房间来消息：收消息入口把暂存配置换上——新订阅的第一条消息就开轮。
    let ingested = bridge
        .ingest(message("qq:group/999", "10001", "新房间第一条", true))
        .expect("能记下来");
    assert!(
        matches!(&ingested, Ingest::Recorded { woke, .. } if woke.contains(&"main".to_owned())),
        "新订阅立即参与唤醒：{ingested:?}"
    );
    assert!(!bridge.has_staged_config(), "换上之后暂存清空");
    assert_eq!(bridge.config_version(), 1, "换配置版本 +1");
    assert_eq!(
        bridge
            .config()
            .session("main")
            .expect("会话还在")
            .subscriptions
            .len(),
        2,
        "桥里跑的就是新配置"
    );

    // 开轮：投的就是刚才那条新房间消息。
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    assert_eq!(delivery.session, "main");
    assert_eq!(delivery.room, RoomId::parse("qq:group/999").expect("合法"));
    assert_eq!(delivery.reason, TurnReason::Wake);
    assert_eq!(
        delivery.records[0].elements[0],
        Element::text("新房间第一条")
    );
}

#[test]
fn discard_staged_config_keeps_old_config() {
    let mut bridge = bridge(&config_base());

    bridge.stage_config(acb_kernel::Config::parse(&config_with_extra_room()).expect("新配置合法"));
    assert!(bridge.discard_staged_config(), "有暂存可丢");
    assert!(!bridge.has_staged_config());
    assert_eq!(bridge.config_version(), 0, "没换过配置");
    assert_eq!(
        bridge
            .config()
            .session("main")
            .expect("会话还在")
            .subscriptions
            .len(),
        1,
        "还是旧配置"
    );

    // 新房间消息现在不该唤醒（旧配置没订它）。
    let ingested = bridge
        .ingest(message("qq:group/999", "10001", "不该醒", true))
        .expect("能记下来");
    assert!(
        matches!(ingested, Ingest::NotSubscribed { .. }),
        "丢弃暂存后回到旧订阅范围：{ingested:?}"
    );
    assert!(!bridge.discard_staged_config(), "没有暂存时丢弃返回 false");
}

#[test]
fn config_reload_prunes_removed_dependencies() {
    let config = Config::parse(&config_base()).expect("配置合法");
    let deps = DependencyRegistry::from_config(&config);
    deps.platform_connected("qq", AT);
    deps.harness_connection_opened("dsh", AT);
    let mut bridge =
        Bridge::new(config, MemoryStore::new(), Box::new(Recorder)).with_dependencies(deps.clone());

    bridge.stage_config(Config::parse(&config_renamed()).expect("新配置合法"));
    // 没有待开轮次也要在下一轮边界换上配置。
    assert!(bridge.poll_turn().expect("能换配置").is_none());

    let snapshot = deps.snapshot(None);
    let ids: Vec<&str> = snapshot["dependencies"]
        .as_array()
        .expect("数组")
        .iter()
        .filter_map(|item| item["id"].as_str())
        .collect();
    assert!(!ids.contains(&"qq"), "被移除的连接直接删除：{snapshot}");
    assert!(
        !ids.contains(&"dsh"),
        "被移除的 harness 直接删除：{snapshot}"
    );

    let added = snapshot["dependencies"]
        .as_array()
        .expect("数组")
        .iter()
        .find(|item| item["id"] == "qq2")
        .expect("新连接已登记");
    assert_eq!(added["state"], "unknown");
    assert_eq!(added["lastError"], Value::Null);
}

#[test]
fn re_staging_replaces_previous_staged_config() {
    let mut bridge = bridge(&config_base());

    bridge.stage_config(acb_kernel::Config::parse(&config_with_extra_room()).expect("第一份暂存"));
    // 再存一次别的：只保留最后一份。
    let third = config_base().replace("accumulate = 50", "accumulate = 3");
    bridge.stage_config(acb_kernel::Config::parse(&third).expect("第三份合法"));

    bridge
        .ingest(message("qq:group/123", "10001", "唤醒", true))
        .expect("能记下来");
    bridge.poll_turn().expect("能开轮");

    assert_eq!(bridge.config_version(), 1, "只换过一次");
    assert_eq!(
        bridge
            .config()
            .session("main")
            .expect("会话还在")
            .subscriptions[0]
            .wake
            .accumulate,
        Some(3),
        "生效的是最后一次保存的那份"
    );
}
