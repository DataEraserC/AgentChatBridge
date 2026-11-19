//! 默认投递计划：触发前后 N 条正文 + 完整未读区间。
//!
//! 这些测试锁住用户 2026-11-10 定稿的格式（`docs/plan/downstream-changes.md` D7）：
//! 正文只围着触发消息，未读区间必须完整、不抽样。

use acb_kernel::{
    DefaultDeliveryComposer, DeliveryComposer, DeliveryPolicy, DeliveryRequest, Record, RecordKind,
    RoomId, Sender, default_delivery_plan,
};
use acb_proto::Element;

fn room() -> RoomId {
    RoomId::new("qq", "group/1").expect("合法")
}

fn record(id: u64, text: &str) -> Record {
    Record {
        id,
        room: room(),
        kind: RecordKind::Message,
        sender: Sender {
            id: "10001".into(),
            name: "老王".into(),
            role: None,
        },
        at: 1_793_000_000_000 + id as i64,
        elements: vec![Element::text(text)],
        at_self: false,
        outgoing: false,
        reply_to_self: false,
        platform_id: None,
        quote_ref: None,
        labels: Vec::new(),
        suppressed: false,
        redelivery: None,
        note: None,
    }
}

fn plan(
    unread: &[Record],
    context: &[Record],
    trigger: Option<u64>,
    before: usize,
    after: usize,
    batch: usize,
) -> acb_kernel::DeliveryPlan {
    default_delivery_plan(&DeliveryRequest {
        session: "main",
        room: &room(),
        records: unread,
        context,
        read_ids: &[],
        exclude_read: false,
        trigger_id: trigger,
        context_before: before,
        context_after: after,
        batch_head: batch,
        limit: 30,
    })
}

#[test]
fn trigger_gets_before_and_after_context_only() {
    let unread: Vec<Record> = (10..=40).map(|id| record(id, &format!("m{id}"))).collect();
    let context = vec![record(7, "早"), record(8, "已读"), record(9, "刚聊过")];
    let plan = plan(&unread, &context, Some(30), 3, 3, 3);

    // 触发点 #30：前面带 #27–#29 的上下文，#27/#28 在未读里但以未读副本为准。
    assert!(plan.keep.contains(&30), "触发消息必须在正文里");
    assert!(
        plan.keep.len() <= 7,
        "正文只该是前后 3 条 + 触发：{:?}",
        plan.keep
    );
    assert_eq!(
        plan.unread_ranges,
        vec![[10, 40]],
        "未读区间必须完整，不截断"
    );
    assert!(plan.omitted.is_none(), "默认计划不再用 omitted 跳段");
}

#[test]
fn clock_wake_delivers_the_oldest_batch_head() {
    let unread: Vec<Record> = (1..=10).map(|id| record(id, "x")).collect();
    let plan = plan(&unread, &[], None, 3, 3, 3);
    assert_eq!(plan.keep, vec![1, 2, 3], "没有触发点时投最旧的 3 条");
    assert_eq!(plan.unread_ranges, vec![[1, 10]]);
}

#[test]
fn unread_ranges_split_on_gaps() {
    let unread = vec![
        record(1, "a"),
        record(2, "b"),
        record(5, "c"),
        record(6, "d"),
    ];
    let plan = plan(&unread, &[], Some(6), 3, 3, 3);
    assert_eq!(plan.unread_ranges, vec![[1, 2], [5, 6]]);
}

#[test]
fn context_before_uses_only_the_tail_it_is_given() {
    // 引擎只取触发消息前 N 条交给计划；计划再兜一层：给多了也只留最后 N 条。
    let unread: Vec<Record> = (20..=25).map(|id| record(id, "x")).collect();
    let context: Vec<Record> = (0..=18).map(|id| record(id, "old")).collect();
    let plan = plan(&unread, &context, Some(24), 3, 3, 3);
    let earliest = plan.keep.iter().copied().min().expect("有正文");
    assert_eq!(earliest, 16, "上下文只留最后 3 条：#16–#18 + #24 + #25");
}

#[test]
fn limit_never_drops_the_trigger() {
    let unread: Vec<Record> = (1..=30).map(|id| record(id, "x")).collect();
    let request = DeliveryRequest {
        session: "main",
        room: &room(),
        records: &unread,
        context: &[],
        read_ids: &[],
        exclude_read: false,
        trigger_id: Some(20),
        context_before: 3,
        context_after: 3,
        batch_head: 3,
        limit: 1,
    };
    let plan = default_delivery_plan(&request);
    assert_eq!(plan.keep, vec![20], "条数上限再小也要保住触发消息");
}

#[test]
fn default_composer_matches_the_default_plan() {
    let unread: Vec<Record> = (1..=5).map(|id| record(id, "x")).collect();
    let request = DeliveryRequest {
        session: "main",
        room: &room(),
        records: &unread,
        context: &[],
        read_ids: &[],
        exclude_read: false,
        trigger_id: Some(5),
        context_before: 3,
        context_after: 3,
        batch_head: 3,
        limit: 30,
    };
    let policy = DeliveryPolicy::default();
    let from_composer = DefaultDeliveryComposer.compose(&policy, &request);
    assert_eq!(from_composer, default_delivery_plan(&request));
}

/// `exclude_read = true`：上下文里**已读的**不投，未读正文不受影响。
///
/// 用户场景（2026-11-19）：投递时会带触发消息上下各 N 条，但其中有些早就读过了，
/// 再投一遍只是浪费上下文。要能配成「已读的不投」，且默认保持现在的行为
/// （不排除），由使用者自己决定。
#[test]
fn exclude_read_drops_read_context_but_keeps_unread() {
    let unread: Vec<Record> = (10..=40).map(|id| record(id, &format!("m{id}"))).collect();
    // 上下文：两条已读（#7 #8）、一条未读（#9）。
    let context = vec![record(7, "早"), record(8, "也早"), record(9, "刚聊过")];

    let composer = acb_kernel::DefaultDeliveryComposer;
    let options = acb_kernel::DeliveryPolicy::default();

    let with_read = composer.compose(
        &options,
        &acb_kernel::DeliveryRequest {
            session: "main",
            room: &room(),
            records: &unread,
            context: &context,
            read_ids: &[7, 8],
            exclude_read: true,
            trigger_id: Some(30),
            context_before: 3,
            context_after: 3,
            batch_head: 3,
            limit: 30,
        },
    );

    // 已读的 #7 #8 不该进正文；未读的 #9 该留着。
    assert!(
        !with_read.keep.contains(&7) && !with_read.keep.contains(&8),
        "已读的上下文不该投：{:?}",
        with_read.keep
    );
    assert!(
        with_read.keep.contains(&9),
        "未读的上下文照旧投：{:?}",
        with_read.keep
    );

    // 关掉开关时三条都还在（默认行为不变）。
    let without = composer.compose(
        &options,
        &acb_kernel::DeliveryRequest {
            session: "main",
            room: &room(),
            records: &unread,
            context: &context,
            read_ids: &[7, 8],
            exclude_read: false,
            trigger_id: Some(30),
            context_before: 3,
            context_after: 3,
            batch_head: 3,
            limit: 30,
        },
    );
    assert!(
        without.keep.contains(&9),
        "关掉开关后 #9 仍在：{:?}",
        without.keep
    );
}
