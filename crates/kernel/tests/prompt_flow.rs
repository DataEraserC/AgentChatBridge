//! PromptProvider：声明式上下文注入、预算裁剪、超时与失败降级。

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, PromptContext, PromptDraft, PromptProvider,
    ProviderDescriptor, ProviderId, RecordDraft, RoomId, Sender, Sent, TurnReason,
};

fn config(prompts: &[&str]) -> String {
    let list = prompts
        .iter()
        .map(|id| format!("{id:?}"))
        .collect::<Vec<_>>()
        .join(", ");
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
    name = "DSH"

    [[sessions]]
    id = "main"
    harness = "dsh"
    persona = "p"
    token = {{ secret = "t-main" }}
    prompts = [{list}]

    [[sessions.subscriptions]]
    connection = "qq"
    room = "group/1"
    "#
    )
}

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: 1,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Seen {
    session: String,
    room: String,
    platform: String,
    reason: TurnReason,
    wake_reason: Option<String>,
}

struct StaticPrompt {
    drafts: Vec<PromptDraft>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl PromptProvider for StaticPrompt {
    fn prompts(&self, context: &PromptContext) -> Result<Vec<PromptDraft>, String> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Seen {
                session: context.session.clone(),
                room: context.room.to_string(),
                platform: context.platform.clone(),
                reason: context.reason,
                wake_reason: context.wake_reason.clone(),
            });
        Ok(self.drafts.clone())
    }
}

struct BrokenPrompt;

impl PromptProvider for BrokenPrompt {
    fn prompts(&self, _context: &PromptContext) -> Result<Vec<PromptDraft>, String> {
        Err("provider boom".into())
    }
}

struct SlowPrompt;

impl PromptProvider for SlowPrompt {
    fn prompts(&self, _context: &PromptContext) -> Result<Vec<PromptDraft>, String> {
        std::thread::sleep(Duration::from_millis(200));
        Ok(vec![PromptDraft {
            text: "太慢了".into(),
            priority: 0,
        }])
    }
}

struct PanicPrompt;

impl PromptProvider for PanicPrompt {
    fn prompts(&self, _context: &PromptContext) -> Result<Vec<PromptDraft>, String> {
        panic!("provider panic");
    }
}

fn bridge_with(entries: &[(&str, Arc<dyn PromptProvider>)]) -> Arc<Mutex<Bridge<MemoryStore>>> {
    let ids: Vec<&str> = entries.iter().map(|(id, _)| *id).collect();
    let mut plugins = PluginManager::with_native_builtins();
    for (id, provider) in entries {
        plugins
            .register_prompt(
                ProviderDescriptor::new(
                    ProviderId::parse(id).expect("合法"),
                    ExtensionPoint::Prompt,
                    PLUGIN_API_VERSION,
                    vec![Capability::PromptContribute],
                ),
                provider.clone(),
            )
            .expect("注册 prompt provider");
    }
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse_fixture(&config(&ids)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )))
}

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn open(bridge: &Arc<Mutex<Bridge<MemoryStore>>>) -> acb_kernel::Delivery {
    lock(bridge)
        .ingest(RecordDraft::message(
            RoomId::parse("qq:group/1").expect("房间"),
            Sender {
                id: "10001".into(),
                name: "u".into(),
                role: None,
            },
            1,
            vec![Element::text("在吗")],
            true,
        ))
        .expect("入库");
    lock(bridge).poll_turn().expect("能开轮").expect("有活")
}

#[test]
fn fragments_are_injected_with_priority_order_and_context() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let bridge = bridge_with(&[(
        "test.prompt",
        Arc::new(StaticPrompt {
            drafts: vec![
                PromptDraft {
                    text: "低优先级".into(),
                    priority: 1,
                },
                PromptDraft {
                    text: "高优先级".into(),
                    priority: 5,
                },
            ],
            seen: seen.clone(),
        }),
    )]);

    let delivery = open(&bridge);
    assert_eq!(delivery.prompts.len(), 2);
    assert_eq!(delivery.prompts[0].text, "高优先级");
    assert_eq!(delivery.prompts[0].priority, 5);
    assert_eq!(delivery.prompts[0].source, "test.prompt");
    assert_eq!(delivery.prompts[1].text, "低优先级");
    assert!(!delivery.prompts[0].truncated);
    // 片段是附加数据，不进记录。
    assert_eq!(delivery.records.len(), 1);
    assert_eq!(delivery.records[0].elements.len(), 1);

    let seen = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].session, "main");
    assert_eq!(seen[0].room, "qq:group/1");
    assert_eq!(seen[0].platform, "qq");
    assert_eq!(seen[0].reason, TurnReason::Wake);
}

#[test]
fn per_fragment_limit_truncates_and_total_budget_drops_low_priority() {
    // 单段超 2000 字符：裁剪并标记。
    let bridge = bridge_with(&[(
        "test.prompt",
        Arc::new(StaticPrompt {
            drafts: vec![PromptDraft {
                text: "长".repeat(2500),
                priority: 0,
            }],
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
    )]);
    let delivery = open(&bridge);
    assert_eq!(delivery.prompts.len(), 1);
    assert_eq!(delivery.prompts[0].text.chars().count(), 2000);
    assert!(delivery.prompts[0].truncated);

    // 四段各 2000 字符：整轮上限 6000，第四段整段丢弃。
    let bridge = bridge_with(&[(
        "test.prompt",
        Arc::new(StaticPrompt {
            drafts: (0..4)
                .map(|index| PromptDraft {
                    text: format!("{index}{}", "长".repeat(1999)),
                    priority: index,
                })
                .collect(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }),
    )]);
    let delivery = open(&bridge);
    assert_eq!(delivery.prompts.len(), 3);
    let total: usize = delivery
        .prompts
        .iter()
        .map(|fragment| fragment.text.chars().count())
        .sum();
    assert_eq!(total, 6000, "整轮字符数不能超过预算");
}

#[test]
fn broken_provider_is_skipped_without_losing_other_sources() {
    let bridge = bridge_with(&[
        ("test.broken", Arc::new(BrokenPrompt)),
        (
            "test.good",
            Arc::new(StaticPrompt {
                drafts: vec![PromptDraft {
                    text: "正常片段".into(),
                    priority: 0,
                }],
                seen: Arc::new(Mutex::new(Vec::new())),
            }),
        ),
    ]);

    let delivery = open(&bridge);
    assert_eq!(delivery.prompts.len(), 1, "坏来源只影响自己");
    assert_eq!(delivery.prompts[0].source, "test.good");
    assert_eq!(delivery.prompts[0].text, "正常片段");
}

#[test]
fn slow_provider_times_out_and_is_skipped() {
    let bridge = bridge_with(&[("test.slow", Arc::new(SlowPrompt))]);
    let started = Instant::now();
    let delivery = open(&bridge);
    assert!(delivery.prompts.is_empty(), "超时来源不能阻塞投递");
    assert!(
        started.elapsed() < Duration::from_millis(180),
        "超时后应当继续投递，而不是等 provider 跑完：{:?}",
        started.elapsed()
    );
}

#[test]
fn panicking_provider_is_isolated_and_skipped() {
    let bridge = bridge_with(&[("test.panic", Arc::new(PanicPrompt))]);
    let delivery = open(&bridge);
    assert!(
        delivery.prompts.is_empty(),
        "provider panic 只跳过该来源，不能带崩投递"
    );
}
