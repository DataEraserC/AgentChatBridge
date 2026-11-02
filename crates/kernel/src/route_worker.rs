//! 路由动作的后台执行者。
//!
//! 路由插件在入库时只做「决定」和「准备」，真正的平台 I/O、下载/转码都在这里。
//! 这个线程不持有 core 的全局锁；只在准备发送和结算回执时短暂回锁。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine::{Bridge, PreparedRouteAction, PreparedTask};
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
                eprintln!("路由动作 worker 已经停了，丢掉后续动作");
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
                eprintln!("路由发送没成：{error:?}");
            }
        }
        PreparedRouteAction::Task(task) => run_task(bridge, task),
    }
}

fn run_task<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>, task: PreparedTask) {
    let outcome = match task::run(&task.handler, &task.params) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("路由任务 {} 失败：{error}", task.handler);
            return;
        }
    };
    let elements = match outcome {
        task::TaskOutcome::Nothing => return,
        task::TaskOutcome::Send(elements) => elements,
    };

    let prepared = {
        let mut guard = lock(bridge);
        guard.prepare_route_send(task.config, &task.session, task.room, elements)
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("路由任务的结果组装成发送失败：{error:?}");
            return;
        }
    };
    let result = prepared.execute();
    let mut guard = lock(bridge);
    if let Err(error) = guard.finish_send(prepared, result) {
        eprintln!("路由任务的结果发送失败：{error:?}");
    }
}

fn lock<S: Store>(bridge: &Arc<Mutex<Bridge<S>>>) -> std::sync::MutexGuard<'_, Bridge<S>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
