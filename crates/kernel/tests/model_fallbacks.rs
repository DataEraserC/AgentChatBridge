//! 后备模型链：怎么存、什么时候生效、怎么随投递下去。
//!
//! 背景（2026-11-09 现场）：上游 router 用 `400 content-blocked` 连拒 6 轮，而「本轮
//! 出错」的通知每房间 10 分钟只发 3 条，用户看到的是「群里完全没人回话」。链本身
//! 存在库里（schema 14），桥每轮随 `session_info` 下发给驱动，驱动负责真换模型重放。

use acb_kernel::{
    Bridge, Config, Element, MemoryStore, ModelFallbacks, Outbound, OutboundError, RecordDraft,
    RoomId, Sender, Sent, SqliteStore,
};

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
        models = ["trae/primary", "trae/backup", "trae/b", "trae/c", "trae/d"]

        [[sessions]]
        id = "main"
        harness = "dsh"
        persona = "p"
        model = "trae/primary"
        token = { secret = "t-main" }

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/1"
    "#
}

fn bridge() -> Bridge<MemoryStore> {
    Bridge::new(
        Config::parse_fixture(config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(Noop),
    )
}

fn message() -> RecordDraft {
    RecordDraft::message(
        RoomId::parse("qq:group/1").expect("房间"),
        Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        },
        AT,
        vec![Element::text("在吗")],
        true,
    )
}

#[test]
fn fallback_chain_keeps_its_order() {
    let mut store = MemoryStore::new();
    store
        .replace_model_fallbacks(&["trae/b".into(), "trae/c".into()])
        .expect("能写");
    assert_eq!(
        store.model_fallbacks().expect("能读"),
        vec!["trae/b".to_owned(), "trae/c".to_owned()],
        "顺序就是优先级，不能被打乱"
    );
}

#[test]
fn sqlite_persists_chain_across_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "acb-model-fallbacks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时间")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("建目录");
    let path = dir.join("acb.db");

    {
        let mut store = SqliteStore::open(&path).expect("开库");
        store
            .replace_model_fallbacks(&["trae/b".into(), "trae/c".into()])
            .expect("写");
    }
    {
        let store = SqliteStore::open(&path).expect("重开");
        assert_eq!(
            store.model_fallbacks().expect("读"),
            vec!["trae/b".to_owned(), "trae/c".to_owned()],
            "重启之后链还在，顺序也一样"
        );
    }
    {
        // 先整份换成一条，再清空：两种都要真落到库上。
        let mut store = SqliteStore::open(&path).expect("再开");
        store
            .replace_model_fallbacks(&["trae/d".into()])
            .expect("换");
        assert_eq!(
            store.model_fallbacks().expect("读"),
            vec!["trae/d".to_owned()]
        );
        store.replace_model_fallbacks(&[]).expect("清空");
        assert!(
            store.model_fallbacks().expect("读").is_empty(),
            "清空要真的清空，不能留旧项"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn invalid_model_refs_are_rejected_before_they_reach_the_store() {
    let mut bridge = bridge();
    for bad in ["deepseek-chat", "/x", "trae/", " / "] {
        let error = bridge
            .replace_model_fallbacks(None, &[bad.to_owned()])
            .expect_err("格式不对必须拒绝");
        assert!(
            error.contains("provider/model") || error.contains("不能为空"),
            "错误要说清哪一项不对，实际：{error}"
        );
    }
    assert!(
        bridge.store().model_fallbacks().expect("读").is_empty(),
        "被拒的写入一个字节都不能落库"
    );
}

#[test]
fn duplicate_entries_are_rejected() {
    let mut bridge = bridge();
    let error = bridge
        .replace_model_fallbacks(None, &["trae/b".into(), "trae/b".into()])
        .expect_err("重复项必须拒绝");
    assert!(error.contains("重复"), "要说清是重复，实际：{error}");
}

#[test]
fn chain_reaches_the_next_delivery_only_after_a_turn_boundary() {
    let mut bridge = bridge();
    assert!(
        !bridge.has_pending_policy_reload(),
        "刚起来没有待生效的改动"
    );

    bridge
        .replace_model_fallbacks(None, &["trae/backup".into()])
        .expect("能写");
    assert!(
        bridge.has_pending_policy_reload(),
        "改完要挂标记，一轮里换模型会让同一轮前后上下文不一致"
    );

    // 收一条消息 = 轮次边界；同时它会排出一轮。
    bridge.ingest(message()).expect("入库");
    assert!(!bridge.has_pending_policy_reload(), "边界过了标记要清");

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let info = delivery.session_info.expect("会话信息跟着下发");
    assert_eq!(info.model.as_deref(), Some("trae/primary"), "主模型不变");
    assert_eq!(
        info.model_fallbacks,
        vec!["trae/backup".to_owned()],
        "后备链要随投递下去，否则驱动根本不知道该换哪个"
    );
}

#[test]
fn no_chain_means_no_retry() {
    let mut bridge = bridge();
    bridge.ingest(message()).expect("入库");
    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let info = delivery.session_info.expect("会话信息");
    assert!(
        info.model_fallbacks.is_empty(),
        "没配就下发空链：驱动据此决定不重试"
    );
}

#[test]
fn session_chain_overrides_global_only_for_that_session() {
    let mut bridge = bridge();
    bridge
        .replace_model_fallbacks(None, &["trae/b".into()])
        .expect("全局链能写");
    bridge
        .replace_model_fallbacks(Some("main"), &["trae/c".into()])
        .expect("会话链能写");

    // 写完先入库；内存里的快照在轮次边界才换（和全局链同一个时机）。
    assert_eq!(
        bridge.store().session_model_fallbacks("main").expect("读"),
        vec!["trae/c".to_owned()],
        "库里的会话链要立刻写进去"
    );
    bridge.ingest(message()).expect("入库 = 轮次边界");

    assert_eq!(
        bridge.session_fallbacks("main"),
        ["trae/c".to_owned()],
        "会话自己配了就用自己的"
    );
    assert_eq!(
        bridge.model_fallbacks(),
        ["trae/b".to_owned()],
        "全局链不能被会话链改掉"
    );

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let info = delivery.session_info.expect("会话信息");
    assert_eq!(
        info.model_fallbacks,
        vec!["trae/c".to_owned()],
        "投递要下发这个会话实际生效的链"
    );
}

#[test]
fn session_chain_must_come_from_its_harness_model_list() {
    let mut bridge = bridge();
    let error = bridge
        .replace_model_fallbacks(Some("main"), &["other/x".into()])
        .expect_err("不在 harness 列表里的模型要拒绝");
    assert!(
        error.contains("dsh") && error.contains("other/x"),
        "错误要说清哪个 harness 不认识它，实际：{error}"
    );
    assert!(
        bridge
            .store()
            .session_model_fallbacks("main")
            .expect("读")
            .is_empty(),
        "被拒的写入一个字节都不能落库"
    );

    // 列表里的就能写。
    bridge
        .replace_model_fallbacks(Some("main"), &["trae/backup".into()])
        .expect("列表里的模型能写");
}

#[test]
fn sqlite_persists_session_chain_across_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "acb-session-fallbacks-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时间")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("建目录");
    let path = dir.join("acb.db");

    {
        let mut store = SqliteStore::open(&path).expect("开库");
        store
            .replace_session_model_fallbacks("main", &["trae/b".into(), "trae/c".into()])
            .expect("写");
        store
            .replace_session_model_fallbacks("side", &["trae/d".into()])
            .expect("写");
    }
    {
        let store = SqliteStore::open(&path).expect("重开");
        assert_eq!(
            store.session_model_fallbacks("main").expect("读"),
            vec!["trae/b".to_owned(), "trae/c".to_owned()],
            "顺序要保住"
        );
        let map = store.session_model_fallbacks_map().expect("读");
        assert_eq!(map.len(), 2, "两个会话各一条");
        assert_eq!(map.get("side"), Some(&vec!["trae/d".to_owned()]));

        let mut store = SqliteStore::open(&path).expect("再开");
        store
            .replace_session_model_fallbacks("main", &[])
            .expect("清空");
        assert!(
            store
                .session_model_fallbacks("main")
                .expect("读")
                .is_empty(),
            "清空后不再有单独配置，回落全局"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
