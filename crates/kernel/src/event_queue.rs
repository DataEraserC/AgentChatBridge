//! 入站事件队列（ADR-0027 Phase 2c-2b）。
//!
//! 适配器只把平台事件翻译成 `RecordDraft` 丢进队列；由 core 侧的 drain 线程
//! 统一 `ingest`。适配器不再直接拿 `Bridge` 的锁，也不再有 `try_lock` + backlog。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Frame;
use crate::config::QueueConfig;
use crate::endpoint::ControlRegistry;
use crate::engine::Bridge;
use crate::route_worker::RouteWorker;
use crate::storage::{RecordDraft, Store};

/// 队列使用率到多少时打一次 warning。固定 80%，不配置。
const QUEUE_WARN_PERCENT: usize = 80;

/// 队列的运行时统计。入站与路由动作共用。
#[derive(Clone, Default)]
pub struct QueueStats {
    inner: Arc<QueueStatsInner>,
}

#[derive(Default)]
struct QueueStatsInner {
    label: &'static str,
    capacity: usize,
    depth: AtomicUsize,
    high_watermark: AtomicUsize,
    rejected: AtomicU64,
    warned: AtomicBool,
}

/// 一次快照，控制台/测试用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub label: &'static str,
    pub capacity: usize,
    pub depth: usize,
    pub high_watermark: usize,
    pub rejected: u64,
    pub warn_percent: usize,
    pub warned: bool,
}

impl QueueStats {
    pub fn new(label: &'static str, capacity: usize) -> Self {
        Self {
            inner: Arc::new(QueueStatsInner {
                label,
                capacity,
                ..QueueStatsInner::default()
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    fn warn_threshold(&self) -> usize {
        self.inner.capacity.saturating_mul(QUEUE_WARN_PERCENT) / 100
    }

    pub(crate) fn enqueued(&self) {
        let depth = self
            .inner
            .depth
            .fetch_add(1, Ordering::SeqCst)
            .saturating_add(1);
        self.inner.high_watermark.fetch_max(depth, Ordering::SeqCst);
        let threshold = self.warn_threshold().max(1);
        if depth >= threshold && !self.inner.warned.swap(true, Ordering::SeqCst) {
            eprintln!(
                "queue: {} reached {}% depth={} capacity={}",
                self.inner.label, QUEUE_WARN_PERCENT, depth, self.inner.capacity
            );
        }
    }

    pub(crate) fn dequeued(&self) {
        // 计数与入队/出队不是同一个原子操作：接收端可能先出队再看到发送端的计数。
        // 计数器绝不能被减到 0 以下，否则下一次 enqueued 会溢出。
        let depth = self
            .inner
            .depth
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |depth| {
                depth.checked_sub(1)
            })
            .map(|previous| previous - 1)
            .unwrap_or(0);
        if depth < self.warn_threshold() {
            self.inner.warned.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn rejected(&self) {
        self.inner.rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            label: self.inner.label,
            capacity: self.inner.capacity,
            depth: self.inner.depth.load(Ordering::SeqCst),
            high_watermark: self.inner.high_watermark.load(Ordering::SeqCst),
            rejected: self.inner.rejected.load(Ordering::Relaxed),
            warn_percent: QUEUE_WARN_PERCENT,
            warned: self.inner.warned.load(Ordering::SeqCst),
        }
    }
}

/// 入站队列的写端。可克隆，给每条适配器连接一个。
#[derive(Clone)]
pub struct EventQueue {
    tx: SyncSender<RecordDraft>,
    stats: QueueStats,
}

/// 入站队列的读端，只有 core 的 drain 线程持有。
pub struct EventReceiver {
    rx: Receiver<RecordDraft>,
    stats: QueueStats,
}

/// 入站被拒的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitError {
    Full,
    Closed,
}

impl SubmitError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Closed => "closed",
        }
    }
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl EventReceiver {
    /// 等一条事件。测试和 conformance 用；core 的 drain 线程走同一个队列。
    pub fn recv_timeout(&self, timeout: Duration) -> Result<RecordDraft, RecvTimeoutError> {
        match self.rx.recv_timeout(timeout) {
            Ok(draft) => {
                self.stats.dequeued();
                Ok(draft)
            }
            Err(error) => Err(error),
        }
    }
}

impl EventQueue {
    /// 默认容量（测试/嵌入式用）。生产 main 用 `channel_with`。
    pub fn channel() -> (EventQueue, EventReceiver) {
        let (queue, receiver, _) = Self::channel_with(QueueConfig::default());
        (queue, receiver)
    }

    pub fn channel_with(config: QueueConfig) -> (EventQueue, EventReceiver, QueueStats) {
        let stats = QueueStats::new("inbound", config.inbound);
        let (tx, rx) = sync_channel(config.inbound);
        (
            EventQueue {
                tx,
                stats: stats.clone(),
            },
            EventReceiver {
                rx,
                stats: stats.clone(),
            },
            stats,
        )
    }

    /// 适配器线程调用。队列有界，绝不阻塞平台读循环；满/接收端没了返回明确错误。
    pub fn submit(&self, draft: RecordDraft) -> Result<(), SubmitError> {
        // 先计数再入队：接收端可能立刻取走，不能让它先看到 0 再减。
        self.stats.enqueued();
        match self.tx.try_send(draft) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.stats.dequeued();
                self.stats.rejected();
                let snapshot = self.stats.snapshot();
                eprintln!(
                    "queue: inbound full capacity={} rejected={}",
                    snapshot.capacity, snapshot.rejected
                );
                Err(SubmitError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.stats.dequeued();
                Err(SubmitError::Closed)
            }
        }
    }

    pub fn stats(&self) -> QueueStats {
        self.stats.clone()
    }
}

/// core 的入站 drain 线程。一次只 ingest 一条，保持队列顺序；锁只拿很短的时间。
///
/// 路由插件的 `interrupt` 动作会从 `ingest` 里带回要打断的会话；控制帧在**锁外**发，
/// 发到管理那个会话的 harness 连接。需要 I/O 的 `send` / `task` 动作交给 `RouteWorker`。
/// 没有连接在管就记一行日志，命令本身不丢。
pub fn drain_events<S: Store + Send + 'static>(
    bridge: Arc<Mutex<Bridge<S>>>,
    events: EventReceiver,
    shutdown: Arc<AtomicBool>,
    controls: ControlRegistry,
    worker: RouteWorker,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(draft) => {
                let (result, interrupts, actions) = {
                    let mut guard = bridge
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let result = guard.ingest(draft);
                    // 打断请求和 I/O 动作都在锁内取出、锁外执行。
                    (result, guard.take_interrupts(), guard.take_route_actions())
                };

                for interrupt in interrupts {
                    let frame = Frame::Control {
                        id: format!("ctl-interrupt-{}-{}", now_millis(), interrupt.session),
                        session: interrupt.session.clone(),
                        command: "interrupt".into(),
                        args: serde_json::json!({ "room": interrupt.room }),
                    };
                    if !controls.send_control(frame) {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "kernel.event_queue.interrupt_no_connection",
                                session = interrupt.session
                            )
                        );
                    }
                }
                worker.enqueue(actions);

                if let Err(error) = result {
                    // 记不下来是真问题，说清楚，别装作没事。
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.event_queue.record_write_failed", error = error)
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::{Element, RoomId, Sender};

    fn draft(text: &str) -> RecordDraft {
        RecordDraft::message(
            RoomId::new("qq", "group/1").expect("合法"),
            Sender {
                id: "10001".into(),
                name: "老王".into(),
                role: None,
            },
            1,
            vec![Element::text(text)],
            false,
        )
    }

    #[test]
    fn queue_preserves_submission_order() {
        let (queue, receiver) = EventQueue::channel();
        queue.submit(draft("一")).expect("有空间");
        queue.submit(draft("二")).expect("有空间");

        let first = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("第一条");
        let second = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("第二条");
        assert!(matches!(&first.elements[0], Element::Text { text } if text == "一"));
        assert!(matches!(&second.elements[0], Element::Text { text } if text == "二"));
    }

    #[test]
    fn submit_after_receiver_drop_is_closed() {
        let (queue, receiver) = EventQueue::channel();
        drop(receiver);
        assert_eq!(queue.submit(draft("没人收")), Err(SubmitError::Closed));
    }

    #[test]
    fn queue_rejects_when_full_and_counts() {
        let (queue, _receiver, stats) = EventQueue::channel_with(QueueConfig {
            inbound: 2,
            route: 1,
        });
        assert!(queue.submit(draft("一")).is_ok());
        assert!(queue.submit(draft("二")).is_ok());
        assert_eq!(queue.submit(draft("三")), Err(SubmitError::Full));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.capacity, 2);
        assert_eq!(snapshot.rejected, 1);
        assert_eq!(snapshot.depth, 2);
    }

    #[test]
    fn warning_resets_after_draining_below_threshold() {
        let (queue, receiver, stats) = EventQueue::channel_with(QueueConfig {
            inbound: 10,
            route: 1,
        });
        for index in 0..8 {
            queue.submit(draft(&format!("{index}"))).expect("有空间");
        }
        assert!(stats.snapshot().warned, "跨过 80% 应打 warning");
        receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("取一条");
        assert!(!stats.snapshot().warned, "降到线下后允许再次 warning");
        assert_eq!(stats.snapshot().depth, 7);
    }

    #[test]
    fn drain_thread_ingests_and_wakes() {
        use crate::config::Config;
        use crate::outbound::{Outbound, OutboundError, Sent};
        use crate::storage::MemoryStore;
        use acb_proto::{Element, RoomId};
        use std::thread;
        use std::time::Instant;

        struct Silent;
        impl Outbound for Silent {
            fn send(
                &mut self,
                _room: &RoomId,
                _elements: &[Element],
            ) -> Result<Sent, OutboundError> {
                Ok(Sent {
                    platform_id: "p".into(),
                    at: 1,
                })
            }
        }

        let config = Config::parse(
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
                room = "group/1"

                [sessions.subscriptions.wake]
                at_mention = true
                name_mention = false
                poke = false
                accumulate = 1
            "#,
        )
        .expect("配置合法");
        let bridge = Arc::new(Mutex::new(Bridge::new(
            config,
            MemoryStore::new(),
            Box::new(Silent),
        )));
        let (queue, receiver) = EventQueue::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let route_worker = RouteWorker::spawn(Arc::clone(&bridge), Arc::clone(&shutdown));
        let worker = {
            let bridge = Arc::clone(&bridge);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                drain_events(
                    bridge,
                    receiver,
                    shutdown,
                    ControlRegistry::new(),
                    route_worker,
                )
            })
        };

        queue.submit(draft("在吗")).expect("有空间");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got_turn = false;
        while Instant::now() < deadline && !got_turn {
            thread::sleep(Duration::from_millis(10));
            let mut guard = bridge
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if guard.poll_turn().expect("能开轮").is_some() {
                got_turn = true;
            }
        }

        shutdown.store(true, Ordering::Relaxed);
        let _ = queue.submit(draft("收工"));
        worker.join().expect("drain 线程能退出");
        assert!(got_turn, "入队的事件应该被 ingest 并触发一轮");
    }
}
