//! 路由动作的后台执行者。
//!
//! 路由插件在入库时只做「决定」和「准备」，真正的平台 I/O、下载/转码都在这里。
//! 这个线程不持有 core 的全局锁；只在准备发送和结算回执时短暂回锁。

use std::sync::atomic::{AtomicBool, Ordering};
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
}

impl RouteWorker {
    pub fn spawn<S: Store + Send + 'static>(
        bridge: Arc<Mutex<Bridge<S>>>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let (tx, rx) = channel();
        std::thread::spawn(move || run_loop(bridge, rx, shutdown));
        Self { tx }
    }

    pub(crate) fn enqueue(&self, actions: Vec<PreparedRouteAction>) {
        for action in actions {
            if self.tx.send(action).is_err() {
                eprintln!("{}", acb_i18n::t!("kernel.route_worker.stopped"));
                break;
            }
        }
    }
}

fn run_loop<S: Store + Send + 'static>(
    bridge: Arc<Mutex<Bridge<S>>>,
    rx: Receiver<PreparedRouteAction>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(action) => run_action(&bridge, action),
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
            return;
        }
    };
    let candidates = match outcome {
        task::TaskOutcome::Nothing => return,
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
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.route_worker.task_result_assemble_failed",
                        error = format!("{error:?}")
                    )
                );
                return;
            }
        };
        match prepared.execute() {
            Ok(sent) => {
                let mut guard = lock(bridge);
                if let Err(error) = guard.finish_send(prepared, Ok(sent)) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "kernel.route_worker.task_result_send_failed",
                            error = format!("{error:?}")
                        )
                    );
                }
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
                let mut guard = lock(bridge);
                if let Err(error) = guard.finish_send(prepared, Err(error)) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "kernel.route_worker.task_result_send_failed",
                            error = format!("{error:?}")
                        )
                    );
                }
                return;
            }
        }
    }
}

fn lock<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>) -> std::sync::MutexGuard<'_, Bridge<S>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
