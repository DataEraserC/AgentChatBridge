//! 路由动作的后台执行者。
//!
//! 路由插件在入库时只做「决定」和「准备」，真正的平台 I/O、下载/转码都在这里。
//! 这个线程不持有 core 的全局锁；只在准备发送和结算回执时短暂回锁。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::DEFAULT_ROUTE_QUEUE;
use crate::engine::{Bridge, PreparedRouteAction, PreparedTask};
use crate::event_queue::QueueStats;
use crate::outbound::OutboundError;
use crate::storage::Store;
use crate::task;

/// 给 drain 线程用的句柄：把准备好的动作丢进后台队列。
#[derive(Clone)]
pub struct RouteWorker {
    tx: SyncSender<PreparedRouteAction>,
    /// 已排进队列 + 正在执行的动作数。排空时据此等待后台发送/任务收尾。
    busy: Arc<AtomicUsize>,
    stats: QueueStats,
}

impl RouteWorker {
    /// 默认路由容量（测试用）。
    pub fn spawn<S: Store + Send + 'static>(
        bridge: Arc<Mutex<Bridge<S>>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self::spawn_with_limit(bridge, shutdown, DEFAULT_ROUTE_QUEUE)
    }

    pub fn spawn_with_limit<S: Store + Send + 'static>(
        bridge: Arc<Mutex<Bridge<S>>>,
        shutdown: Arc<AtomicBool>,
        capacity: usize,
    ) -> Self {
        let (tx, rx) = sync_channel(capacity);
        let busy = Arc::new(AtomicUsize::new(0));
        let stats = QueueStats::new("route", capacity);
        let worker_busy = Arc::clone(&busy);
        let worker_stats = stats.clone();
        std::thread::spawn(move || run_loop(bridge, rx, shutdown, worker_busy, worker_stats));
        Self { tx, busy, stats }
    }

    /// 有界入队：满时拒绝该动作并计数，绝不阻塞 drain 线程。
    ///
    /// 返回**被丢弃的任务**。任务一旦受理就必须有结局，队列满直接丢掉会让它永远停在
    /// `Pending`，`tasks op=wait` 等到死也等不到（2026-11-09 审查 C1）。发送动作没有
    /// 这个约束（丢了就是没发，模型会从上下文看到），所以只交回任务、由调用方落 `failed`。
    pub(crate) fn enqueue(&self, actions: Vec<PreparedRouteAction>) -> Vec<PreparedTask> {
        let mut rejected = 0usize;
        let mut stopped_logged = false;
        let mut dropped: Vec<PreparedTask> = Vec::new();
        for action in actions {
            self.busy.fetch_add(1, Ordering::SeqCst);
            // 先计数再入队：worker 可能立刻取走，不能让它先看到 0 再减。
            self.stats.enqueued();
            match self.tx.try_send(action) {
                Ok(()) => {}
                Err(TrySendError::Full(action)) => {
                    self.busy.fetch_sub(1, Ordering::SeqCst);
                    self.stats.dequeued();
                    self.stats.rejected();
                    rejected = rejected.saturating_add(1);
                    if let PreparedRouteAction::Task(task) = action {
                        dropped.push(task);
                    }
                }
                Err(TrySendError::Disconnected(action)) => {
                    self.busy.fetch_sub(1, Ordering::SeqCst);
                    self.stats.dequeued();
                    rejected = rejected.saturating_add(1);
                    // 队列已断：不 `break`，把这一批剩下的任务也一并交回去收尾，
                    // 否则它们同样是永远 `Pending`。
                    if let PreparedRouteAction::Task(task) = action {
                        dropped.push(task);
                    } else if !stopped_logged {
                        eprintln!("{}", acb_i18n::t!("kernel.route_worker.stopped"));
                        stopped_logged = true;
                    }
                }
            }
        }
        if rejected > 0 {
            eprintln!(
                "queue: route full capacity={} rejected={} (batch rejected={rejected})",
                self.stats.capacity(),
                self.stats.snapshot().rejected
            );
        }
        dropped
    }

    /// 队列里 + 正在执行的动作数。排空判定用。
    pub fn busy(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> QueueStats {
        self.stats.clone()
    }
}

fn run_loop<S: Store + Send + 'static>(
    bridge: Arc<Mutex<Bridge<S>>>,
    rx: Receiver<PreparedRouteAction>,
    shutdown: Arc<AtomicBool>,
    busy: Arc<AtomicUsize>,
    stats: QueueStats,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(action) => {
                stats.dequeued();
                // 单个动作崩溃只丢这一个，worker 循环不能停：停了这个线程，
                // 之后所有路由发送与后台任务都静默失效（2026-11-09 审查 P0-3）。
                if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_action(&bridge, action)
                })) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "kernel.route_worker.action_panicked",
                            error = panic_text(payload.as_ref())
                        )
                    );
                }
                busy.fetch_sub(1, Ordering::SeqCst);
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// panic 的 payload 转成人能读的一句话。`&str` / `String` 覆盖绝大多数
/// `panic!("…")`；其余（比如 `panic_any(结构体)`）只能给个占位。
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        String::from("panic")
    }
}

fn run_action<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>, action: PreparedRouteAction) {
    match action {
        PreparedRouteAction::Send(mut prepared) => {
            if let Err(error) = prepared.relay() {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.route_worker.relay_failed",
                        error = format!("{error:?}")
                    )
                );
                return;
            }
            let result = prepared.execute();
            let mut guard = lock(bridge);
            if let Err(error) = guard.finish_send(prepared, result) {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.route_worker.send_failed",
                        error = format!("{error:?}")
                    )
                );
            }
        }
        PreparedRouteAction::Task(task) => run_task(bridge, task),
    }
}

fn run_task<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>, task: PreparedTask) {
    {
        let mut guard = lock(bridge);
        guard.mark_task_running(&task.task_id);
    }

    // 插件 handler 先取出来，命令/任务在锁外跑；取不到再回退内置 `exec`。
    let plugin_handler = lock(bridge).task_handler(&task.handler);
    let outcome = match plugin_handler {
        // 插件是树外代码，panic 不能带走唯一的 route worker：转成普通失败，
        // 任务按 `failed` 收尾，模型能查到原因，后面的动作照常处理。
        Some(handler) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handler.handle(&task.task_id, &task.params)
        })) {
            Ok(outcome) => outcome,
            Err(payload) => Err(acb_i18n::t!(
                "kernel.route_worker.task_panicked",
                handler = task.handler,
                error = panic_text(payload.as_ref())
            )),
        },
        None => task::run(&task.handler, &task.params),
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "kernel.route_worker.task_failed",
                    handler = task.handler,
                    error = error
                )
            );
            lock(bridge).finish_task(&task.task_id, None, Some(error), None);
            return;
        }
    };
    let candidates = match outcome {
        task::TaskOutcome::Nothing => {
            lock(bridge).finish_task(
                &task.task_id,
                Some(serde_json::json!({ "outcome": "nothing" })),
                None,
                None,
            );
            return;
        }
        task::TaskOutcome::Send(candidates) => candidates,
    };
    let total = candidates.len();

    // 候选按顺序试：只有平台在编码阶段返回 Unsupported（还没发出去）才换下一个。
    // 超时、拒收都不重试，避免把同一条消息发两遍。
    for (index, elements) in candidates.into_iter().enumerate() {
        let prepared = {
            let mut guard = lock(bridge);
            guard.prepare_route_send(
                task.config.clone(),
                &task.session,
                task.room.clone(),
                elements,
            )
        };
        let mut prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let detail = format!("{error:?}");
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.route_worker.task_result_assemble_failed",
                        error = detail
                    )
                );
                lock(bridge).finish_task(
                    &task.task_id,
                    Some(serde_json::json!({ "outcome": "assemble_failed" })),
                    Some(detail),
                    None,
                );
                return;
            }
        };
        if let Err(error) = prepared.relay() {
            let detail = format!("{error:?}");
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "kernel.route_worker.task_result_relay_failed",
                    error = detail
                )
            );
            lock(bridge).finish_task(
                &task.task_id,
                Some(serde_json::json!({ "outcome": "relay_failed" })),
                Some(detail),
                None,
            );
            return;
        }
        match prepared.execute() {
            Ok(sent) => {
                let mut guard = lock(bridge);
                let send_error = guard
                    .finish_send(prepared, Ok(sent))
                    .err()
                    .map(|error| format!("{error:?}"));
                if let Some(error) = &send_error {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.route_worker.task_result_send_failed", error = error)
                    );
                }
                guard.finish_task(
                    &task.task_id,
                    Some(serde_json::json!({
                        "outcome": "sent",
                        "candidates": total,
                    })),
                    None,
                    send_error,
                );
                return;
            }
            Err(OutboundError::Unsupported(detail)) if index + 1 < total => {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.route_worker.candidate_unsupported",
                        index = index + 1,
                        detail = detail
                    )
                );
            }
            Err(error) => {
                let detail = format!("{error:?}");
                let mut guard = lock(bridge);
                let send_error = guard
                    .finish_send(prepared, Err(error))
                    .err()
                    .map(|error| format!("{error:?}"));
                if let Some(error) = &send_error {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("kernel.route_worker.task_result_send_failed", error = error)
                    );
                }
                guard.finish_task(
                    &task.task_id,
                    Some(serde_json::json!({ "outcome": "send_failed" })),
                    None,
                    Some(detail),
                );
                return;
            }
        }
    }

    eprintln!(
        "{}",
        acb_i18n::t!(
            "kernel.route_worker.task_result_assemble_failed",
            error = "all send candidates unsupported"
        )
    );
    lock(bridge).finish_task(
        &task.task_id,
        Some(serde_json::json!({ "outcome": "unsupported" })),
        Some("all send candidates unsupported".to_owned()),
        None,
    );
}

fn lock<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>) -> std::sync::MutexGuard<'_, Bridge<S>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_proto::RoomId;

    fn task(id: &str, config: Arc<crate::config::Config>) -> PreparedRouteAction {
        PreparedRouteAction::Task(PreparedTask {
            task_id: id.to_owned(),
            room: RoomId::parse("qq:group/1").expect("房间合法"),
            session: "main".to_owned(),
            config,
            handler: "noop".to_owned(),
            params: serde_json::json!({}),
        })
    }

    #[test]
    fn full_route_queue_rejects_without_inflating_busy() {
        let config = Arc::new(crate::config::Config::parse_fixture("").expect("空配置合法"));
        let (tx, _rx) = sync_channel(1);
        let worker = RouteWorker {
            tx,
            busy: Arc::new(AtomicUsize::new(0)),
            stats: QueueStats::new("route", 1),
        };
        worker.enqueue(vec![task("a", Arc::clone(&config))]);
        assert_eq!(worker.busy(), 1);
        assert_eq!(worker.stats().snapshot().depth, 1);

        let dropped = worker.enqueue(vec![task("b", config)]);
        let snapshot = worker.stats().snapshot();
        assert_eq!(worker.busy(), 1, "被拒绝的动作不算 busy");
        assert_eq!(snapshot.rejected, 1);
        assert_eq!(snapshot.depth, 1);
        // 任务被拒不能就这么没了：必须交回调用方落 failed，否则永远 Pending。
        assert_eq!(dropped.len(), 1, "被拒的任务必须交回");
        assert_eq!(dropped[0].task_id, "b");
    }

    #[test]
    fn stopped_route_queue_hands_back_every_task_in_batch() {
        let config = Arc::new(crate::config::Config::parse_fixture("").expect("空配置合法"));
        let (tx, rx) = sync_channel(1);
        drop(rx); // 接收端已消失 → 队列断了
        let worker = RouteWorker {
            tx,
            busy: Arc::new(AtomicUsize::new(0)),
            stats: QueueStats::new("route", 1),
        };

        let dropped = worker.enqueue(vec![task("a", Arc::clone(&config)), task("b", config)]);
        // 断了之后不能 break 掉整批：剩下的任务同样是「受理了没结局」。
        assert_eq!(dropped.len(), 2, "队列已断时整批任务都要交回收尾");
        assert_eq!(dropped[0].task_id, "a");
        assert_eq!(dropped[1].task_id, "b");
        assert_eq!(worker.busy(), 0);
    }
}
