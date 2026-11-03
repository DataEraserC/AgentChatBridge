//! 路由动作的后台执行者。
//!
//! 路由插件在入库时只做「决定」和「准备」，真正的平台 I/O、下载/转码都在这里。
//! 这个线程不持有 core 的全局锁；只在准备发送和结算回执时短暂回锁。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine::{Bridge, PreparedRouteAction, PreparedTask};
use crate::outbound::OutboundError;
use crate::storage::Store;
use crate::task;

/// 给 drain 线程用的句柄：把准备好的动作丢进后台队列。
#[derive(Clone)]
pub struct RouteWorker {
    tx: Sender<PreparedRouteAction>,
    /// 已排进队列 + 正在执行的动作数。排空时据此等待后台发送/任务收尾。
    busy: Arc<AtomicUsize>,
}

impl RouteWorker {
    pub fn spawn<S: Store + Send + 'static>(
        bridge: Arc<Mutex<Bridge<S>>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let (tx, rx) = channel();
        let busy = Arc::new(AtomicUsize::new(0));
        let worker_busy = Arc::clone(&busy);
        std::thread::spawn(move || run_loop(bridge, rx, shutdown, worker_busy));
        Self { tx, busy }
    }

    pub(crate) fn enqueue(&self, actions: Vec<PreparedRouteAction>) {
        for action in actions {
            self.busy.fetch_add(1, Ordering::SeqCst);
            if self.tx.send(action).is_err() {
                self.busy.fetch_sub(1, Ordering::SeqCst);
                eprintln!("{}", acb_i18n::t!("kernel.route_worker.stopped"));
                break;
            }
        }
    }

    /// 队列里 + 正在执行的动作数。排空判定用。
    pub fn busy(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }
}

fn run_loop<S: Store + Send + 'static>(
    bridge: Arc<Mutex<Bridge<S>>>,
    rx: Receiver<PreparedRouteAction>,
    shutdown: Arc<AtomicBool>,
    busy: Arc<AtomicUsize>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(action) => {
                run_action(&bridge, action);
                busy.fetch_sub(1, Ordering::SeqCst);
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn run_action<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>, action: PreparedRouteAction) {
    match action {
        PreparedRouteAction::Send(prepared) => {
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

    let outcome = match task::run(&task.handler, &task.params) {
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
        let prepared = match prepared {
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
