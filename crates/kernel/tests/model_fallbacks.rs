//! 后备模型链：怎么存、什么时候生效、怎么随投递下去。
//!
//! 背景（2026-11-09 现场）：上游 router 用 `400 content-blocked` 连拒 6 轮，而「本轮
//! 出错」的通知每房间 10 分钟只发 3 条，用户看到的是「群里完全没人回话」。链本身
//! 存在库里（schema 14），桥每轮随 `session_info` 下发给驱动，驱动负责真换模型重放。

use acb_kernel::{
    Bridge, Config, Element, FallbackPolicy, MemoryStore, ModelFallbackPolicy, ModelFallbacks,
    Outbound, OutboundError, RecordDraft, RoomId, Sender, Sent, SqliteStore,
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
fn console_scope_shows_saved_chain_before_turn_boundary() {
    // 现场：会话级链保存成功，控制台回读还是全局那份。根因是读的是「本轮生效」
    // 的内存快照，边界没到就不换。控制台读库才对。
    let mut bridge = bridge();
    let before = bridge.stored_fallback_scope(Some("main"));
    assert!(before.inherited, "没配过会话链时如实标「继承全局」");
    assert!(before.models.is_empty(), "内存里还没有任何链");

    bridge
        .replace_model_fallbacks(None, &["trae/b".into()])
        .expect("全局链");
    bridge
        .replace_model_fallbacks(Some("main"), &["trae/c".into()])
        .expect("会话链");

    let global = bridge.stored_fallback_scope(None);
    assert_eq!(global.models, ["trae/b".to_owned()], "全局保存后立刻可见");
    assert!(!global.inherited, "全局范围不存在继承");

    let scoped = bridge.stored_fallback_scope(Some("main"));
    assert_eq!(
        scoped.models,
        ["trae/c".to_owned()],
        "会话保存后立刻回读要看到自己那份，而不是全局链"
    );
    assert!(!scoped.inherited, "保存过就不该再标「继承全局」");
    assert!(scoped.from_store, "库读成功要标明来源");

    // 边界语义不能被读接口改掉：本轮生效的链仍然是旧的那份。
    assert_eq!(
        bridge.session_fallbacks("main"),
        bridge.model_fallbacks(),
        "轮次边界没到，投递用的还是全局链"
    );
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

// ---------------------------------------------------------------- 失败惩罚
//
// 语义：某个模型在窗口内同一 class 累计到阈值，就进惩罚期；惩罚期内它从下发的链里
// 被摘掉；到期自动回归。惩罚是内存态，策略落库。

fn policy(enabled: bool, threshold: u64, window_secs: u64, penalty_secs: u64) -> FallbackPolicy {
    FallbackPolicy {
        enabled,
        failure_threshold: threshold,
        failure_window_secs: window_secs,
        penalty_secs,
    }
}

/// 读一遍真实墙钟：投递里没有 `now` 参数，惩罚判定用的是 `now_millis()`。
fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时间")
        .as_millis() as i64
}

#[test]
fn penalty_starts_only_after_the_threshold_within_the_window() {
    let mut bridge = bridge();
    bridge
        .replace_fallback_policy(None, Some(&policy(true, 2, 60, 300)))
        .expect("全局策略能写");
    bridge
        .replace_model_fallbacks(None, &["trae/b".into()])
        .expect("链");
    // 收一条消息 = 轮次边界：策略和链一起换进内存。
    bridge.ingest(message()).expect("边界");
    assert_eq!(
        bridge.fallback_policy().failure_threshold,
        2,
        "策略要在轮次边界换进内存"
    );

    let now = 1_000_000i64;
    bridge.note_model_failure(Some("main"), "trae/b", "rate_limited", now);
    let plan = bridge.fallback_plan(Some("main"), now + 1_000);
    assert_eq!(
        plan.models,
        ["trae/b".to_owned()],
        "第一次失败不够阈值，不该罚"
    );
    assert!(plan.skipped.is_empty());

    bridge.note_model_failure(Some("main"), "trae/b", "rate_limited", now + 2_000);
    let plan = bridge.fallback_plan(Some("main"), now + 2_000);
    assert!(plan.models.is_empty(), "到阈值就该从链里摘掉");
    assert_eq!(plan.skipped.len(), 1);
    assert_eq!(plan.skipped[0].model, "trae/b");
    assert_eq!(plan.skipped[0].class, "rate_limited");
    assert_eq!(plan.skipped[0].remaining_secs, 300);
}

#[test]
fn failures_outside_the_window_do_not_add_up() {
    let mut bridge = bridge();
    bridge
        .replace_fallback_policy(None, Some(&policy(true, 2, 10, 300)))
        .expect("策略");
    bridge
        .replace_model_fallbacks(None, &["trae/b".into()])
        .expect("链");
    bridge.ingest(message()).expect("边界");

    let now = 1_000_000i64;
    bridge.note_model_failure(Some("main"), "trae/b", "overloaded", now);
    // 20 秒后再失败一次：10 秒窗口里那次已经过期，凑不成 2 次。
    bridge.note_model_failure(Some("main"), "trae/b", "overloaded", now + 20_000);
    assert!(
        bridge
            .fallback_plan(Some("main"), now + 20_000)
            .models
            .contains(&"trae/b".to_owned()),
        "窗口外的失败不能计入阈值"
    );
}

#[test]
fn penalised_model_disappears_from_the_delivered_chain() {
    let mut bridge = bridge();
    bridge
        .replace_fallback_policy(None, Some(&policy(true, 1, 60, 300)))
        .expect("策略");
    bridge
        .replace_model_fallbacks(None, &["trae/b".into(), "trae/c".into()])
        .expect("链");
    bridge.ingest(message()).expect("边界");

    bridge.note_model_failure(Some("main"), "trae/b", "content_blocked", wall_now());

    let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
    let info = delivery.session_info.expect("会话信息");
    assert_eq!(
        info.model_fallbacks,
        vec!["trae/c".to_owned()],
        "惩罚中的模型不能下发；链里其余项的顺序不变"
    );
    assert_eq!(
        bridge.session_fallbacks("main"),
        ["trae/b".to_owned(), "trae/c".to_owned()],
        "库里的链本身不能被惩罚改写：惩罚只是这一刻的过滤"
    );
}

#[test]
fn penalty_expires_on_its_own_without_a_sweep() {
    let mut bridge = bridge();
    bridge
        .replace_fallback_policy(None, Some(&policy(true, 1, 60, 5)))
        .expect("策略");
    bridge
        .replace_model_fallbacks(None, &["trae/b".into()])
        .expect("链");
    bridge.ingest(message()).expect("边界");

    let now = 1_000_000i64;
    bridge.note_model_failure(Some("main"), "trae/b", "rate_limited", now);
    assert!(
        bridge.fallback_plan(Some("main"), now).models.is_empty(),
        "刚进惩罚期要罚"
    );
    assert_eq!(
        bridge.fallback_plan(Some("main"), now + 5_000).models,
        ["trae/b".to_owned()],
        "到期那一刻自动回归，不需要外部清扫"
    );
    assert!(
        bridge.active_model_penalties(now + 5_000).is_empty(),
        "过期的惩罚在查询时被顺手丢掉"
    );
}

#[test]
fn model_policy_overrides_the_global_one() {
    let mut bridge = bridge();
    // 全局：阈值高到不会罚；`trae/b` 自己：一次就罚。
    bridge
        .replace_fallback_policy(None, Some(&policy(true, 99, 60, 300)))
        .expect("全局策略");
    bridge
        .replace_fallback_policy(Some("trae/b"), Some(&policy(true, 1, 60, 300)))
        .expect("模型策略");
    bridge
        .replace_model_fallbacks(None, &["trae/b".into()])
        .expect("链");
    bridge.ingest(message()).expect("边界");

    assert_eq!(
        bridge.model_fallback_policy("trae/b").failure_threshold,
        1,
        "模型自己配了就用自己那份"
    );
    assert_eq!(
        bridge.model_fallback_policy("trae/other").failure_threshold,
        99,
        "没配的模型回落全局"
    );
    assert_eq!(
        bridge.fallback_policy().failure_threshold,
        99,
        "全局那份不能被模型策略改掉"
    );

    let now = 1_000_000i64;
    bridge.note_model_failure(Some("main"), "trae/b", "model_unavailable", now);
    assert!(
        bridge.fallback_plan(Some("main"), now).models.is_empty(),
        "这个模型当天就被罚（阈值 1），链里只剩它 → 没有可用的"
    );
    assert!(
        bridge.fallback_plan(None, now).models.is_empty(),
        "惩罚记在模型上，与哪个会话无关"
    );
}

#[test]
fn invalid_penalty_policy_is_rejected_before_it_reaches_the_store() {
    let mut bridge = bridge();
    let bad = [
        policy(true, 3, 0, 300),          // 窗口 0：永远累计不到阈值
        policy(true, 3, 60, 0),           // 惩罚时长 0：等于不罚
        policy(true, 10_000, 60, 300),    // 阈值过大
        policy(true, 3, 10_000_000, 300), // 窗口过大
        policy(true, 3, 60, 10_000_000),  // 时长过大
    ];
    for item in bad {
        let error = bridge
            .replace_fallback_policy(None, Some(&item))
            .expect_err("非法策略必须拒绝");
        assert!(
            error.contains("失败阈值") || error.contains("统计窗口") || error.contains("惩罚时长"),
            "拒绝原因要能看出哪个字段不对，实际：{error}"
        );
    }
    assert_eq!(
        bridge.store().fallback_policy().expect("读"),
        FallbackPolicy::default(),
        "被拒的写入一个字节都不能落库"
    );

    // 负数在类型层就被 from_parts 挡掉（控制台原来传的是 i64）。
    let error = FallbackPolicy::from_parts(true, -1, 60, 300).expect_err("负数要拒绝");
    assert!(error.contains("负数"), "实际：{error}");
}

/// 惩罚策略落库：MemoryStore 与 SqliteStore 两个实现都要对得上。
#[test]
fn sqlite_persists_penalty_policy_across_reopen() {
    let dir = std::env::temp_dir().join(format!(
        "acb-fallback-policy-{}-{}",
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
            .replace_fallback_policy(&policy(true, 3, 120, 600))
            .expect("写全局");
        store
            .replace_model_fallback_policy("trae/b", Some(&policy(false, 1, 60, 60)))
            .expect("写模型");
    }
    {
        let store = SqliteStore::open(&path).expect("重开");
        let global = store.fallback_policy().expect("读全局");
        assert!(global.enabled);
        assert_eq!(global.failure_threshold, 3);
        assert_eq!(global.failure_window_secs, 120);
        assert_eq!(global.penalty_secs, 600);
        let model = store
            .model_fallback_policy("trae/b")
            .expect("读模型")
            .expect("有这个模型的策略");
        assert!(!model.enabled, "模型自己那份原样读回");
        assert_eq!(store.model_fallback_policies().expect("读全部").len(), 1);
    }
    {
        // 会话侧写 None = 删掉覆盖，回落全局；全局那份不受影响。
        let mut store = SqliteStore::open(&path).expect("再开");
        store
            .replace_model_fallback_policy("trae/b", None)
            .expect("删模型策略");
        assert!(
            store.model_fallback_policy("trae/b").expect("读").is_none(),
            "删掉之后没有覆盖了"
        );
        assert_eq!(
            store.fallback_policy().expect("读全局").failure_threshold,
            3
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
