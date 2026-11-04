//! T7a：后台任务记录与 task_list / task_get。
//!
//! 真实走 drain 线程 → RouteWorker → exec，验证任务状态从 queued/running 到
//! succeeded/failed，以及工具查询在轮次结束后仍然可用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_kernel::{
    Bridge, Capability, Config, ControlRegistry, Element, EventQueue, ExtensionPoint, MemoryStore,
    Outbound, OutboundError, PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId,
    RecordDraft, RoomId, RouteAction, RouteContext, RouteDecision, RoutePolicy, RouteProvider,
    RouteWorker, Sender, Sent, TaskHandler, TaskOutcome, drain_events,
};
use serde_json::{Value, json};

fn config(wake_extra: &str) -> String {
    format!(
        r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"

        [[connections.routes]]
        provider = "test.task"

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
        room = "group/1"

        [sessions.subscriptions.wake]
        provider = "acb.wake.default"
        {wake_extra}
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

struct TaskRoute;

impl RouteProvider for TaskRoute {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        // 如果合成事件误入路由，这里会再创建一条任务，测试用 total 计数抓循环。
        if context.event == Some("task") {
            return RouteDecision::Handle {
                store: false,
                wake: false,
                actions: vec![RouteAction::Task {
                    handler: "exec".into(),
                    params: json!({
                        "command": ["sh", "-c", "printf loop"],
                        "send": "text",
                        "wake_on": "always",
                    }),
                }],
            };
        }
        let (command, wake_on) = if context.text.starts_with("/task ok") {
            (vec!["sh", "-c", "printf ok"], "always")
        } else if context.text.starts_with("/task fail") {
            (vec!["sh", "-c", "exit 7"], "always")
        } else if context.text.starts_with("/task plugin") {
            return RouteDecision::Handle {
                store: false,
                wake: false,
                actions: vec![RouteAction::Task {
                    handler: "test.echo".into(),
                    params: json!({"text": "plugin-ok"}),
                }],
            };
        } else if context.text.starts_with("/task quiet") {
            (vec!["sh", "-c", "printf quiet"], "never")
        } else if context.text.starts_with("/task error-ok") {
            (vec!["sh", "-c", "printf error-ok"], "error")
        } else if context.text.starts_with("/task error-fail") {
            (vec!["sh", "-c", "exit 8"], "error")
        } else {
            return RouteDecision::Pass;
        };
        RouteDecision::Handle {
            store: false,
            wake: false,
            actions: vec![RouteAction::Task {
                handler: "exec".into(),
                params: json!({
                    "command": command,
                    "send": "text",
                    "wake_on": wake_on,
                }),
            }],
        }
    }
}

struct EchoTask;

impl TaskHandler for EchoTask {
    fn handle(&self, _task_id: &str, payload: &Value) -> Result<TaskOutcome, String> {
        let text = payload
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("echo");
        Ok(TaskOutcome::Send(vec![vec![Element::text(text)]]))
    }
}

fn bridge(wake_extra: &str) -> Arc<Mutex<Bridge<MemoryStore>>> {
    let id = ProviderId::parse("test.task").expect("合法");
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .register_task_handler(
            ProviderDescriptor::new(
                ProviderId::parse("test.echo").expect("合法"),
                ExtensionPoint::Task,
                PLUGIN_API_VERSION,
                vec![Capability::TaskHandle],
            ),
            "test.echo",
            Arc::new(EchoTask),
        )
        .expect("注册任务 handler");
    plugins
        .register_route(
            ProviderDescriptor::new(
                id,
                acb_kernel::ExtensionPoint::Route,
                PLUGIN_API_VERSION,
                vec![acb_kernel::Capability::RouteDecision],
            ),
            Arc::new(TaskRoute),
        )
        .expect("注册路由");
    Arc::new(Mutex::new(Bridge::with_plugins(
        Config::parse(&config(wake_extra)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )))
}

fn message(text: &str, at_self: bool) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse("qq:group/1").expect("房间"),
        Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        },
        1,
        vec![Element::text(text)],
        at_self,
    )
}

fn lock(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
) -> std::sync::MutexGuard<'_, Bridge<MemoryStore>> {
    bridge
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait_for_total(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
    total: usize,
    timeout: Duration,
) -> Value {
    let start = Instant::now();
    loop {
        let list = lock(bridge)
            .task_list("main", None, 20, 0)
            .expect("能列任务");
        if list["total"].as_u64() == Some(total as u64) {
            return list;
        }
        if start.elapsed() > timeout {
            panic!("等待任务总数 {total} 超时；最后结果：{list}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_task(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
    predicate: impl Fn(&Value) -> bool,
    timeout: Duration,
) -> Value {
    let start = Instant::now();
    loop {
        let list = lock(bridge)
            .task_list("main", None, 20, 0)
            .expect("能列任务");
        if let Some(task) = list["tasks"]
            .as_array()
            .and_then(|tasks| tasks.iter().find(|task| predicate(task)))
        {
            return task.clone();
        }
        if start.elapsed() > timeout {
            panic!("等待任务超时；最后结果：{list}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_status(
    bridge: &Arc<Mutex<Bridge<MemoryStore>>>,
    status: &str,
    timeout: Duration,
) -> Value {
    let start = Instant::now();
    loop {
        let list = lock(bridge)
            .task_list("main", None, 20, 0)
            .expect("能列任务");
        if let Some(task) = list["tasks"]
            .as_array()
            .and_then(|tasks| tasks.iter().find(|task| task["status"] == status))
        {
            return task.clone();
        }
        if start.elapsed() > timeout {
            panic!("等待任务状态 {status} 超时；最后结果：{list}");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn plugin_task_handler_runs_in_the_route_worker() {
    let bridge = bridge("");
    let (queue, receiver) = EventQueue::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker = RouteWorker::spawn(Arc::clone(&bridge), Arc::clone(&shutdown));
    let drain = {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            drain_events(bridge, receiver, shutdown, ControlRegistry::new(), worker)
        })
    };

    let _ = queue.submit(message("/task plugin", false));
    let task = wait_for_task(
        &bridge,
        |task| task["handler"] == "test.echo" && task["status"] == "succeeded",
        Duration::from_secs(3),
    );
    assert_eq!(task["result"]["outcome"], "sent");
    assert!(task["error"].is_null());

    shutdown.store(true, Ordering::Relaxed);
    let _ = queue.submit(message("收工", false));
    drain.join().expect("drain 收工");
}

#[test]
fn task_records_success_failure_and_can_be_queried_after_turn_end() {
    let bridge = bridge("");
    let (queue, receiver) = EventQueue::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker = RouteWorker::spawn(Arc::clone(&bridge), Arc::clone(&shutdown));
    let drain = {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            drain_events(bridge, receiver, shutdown, ControlRegistry::new(), worker)
        })
    };

    let _ = queue.submit(message("/task ok", false));
    let ok = wait_for_status(&bridge, "succeeded", Duration::from_secs(3));
    assert_eq!(ok["handler"], "exec");
    assert!(
        ok["params_summary"]
            .as_str()
            .expect("summary")
            .contains("printf")
    );
    assert!(ok["started_at"].as_i64().is_some());
    assert!(ok["finished_at"].as_i64().is_some());
    assert_eq!(ok["error"], Value::Null);
    assert!(ok["event_id"].as_u64().is_some(), "成功后应发合成事件");
    assert_eq!(
        ok["event_woke"].as_array().map(Vec::len),
        Some(0),
        "默认唤醒策略不应被任务事件直接绕过"
    );
    assert_eq!(
        wait_for_total(&bridge, 1, Duration::from_secs(3))["total"],
        1,
        "合成事件不能再次进入 route 触发任务"
    );

    let _ = queue.submit(message("/task fail", false));
    let failed = wait_for_status(&bridge, "failed", Duration::from_secs(3));
    assert_eq!(failed["handler"], "exec");
    assert!(failed["error"].as_str().is_some(), "失败原因可见");
    assert!(failed["event_id"].as_u64().is_some(), "失败后应发合成事件");
    assert_eq!(
        wait_for_total(&bridge, 2, Duration::from_secs(3))["total"],
        2
    );

    let _ = queue.submit(message("/task quiet", false));
    let quiet = wait_for_task(
        &bridge,
        |task| task["wake_on"] == "never" && task["status"] == "succeeded",
        Duration::from_secs(3),
    );
    assert_eq!(quiet["event_id"], Value::Null, "wake_on=never 不发事件");
    assert_eq!(quiet["event_error"], Value::Null);
    assert_eq!(
        wait_for_total(&bridge, 3, Duration::from_secs(3))["total"],
        3,
        "quiet 任务也不能触发额外任务"
    );

    let _ = queue.submit(message("/task error-ok", false));
    let error_ok = wait_for_task(
        &bridge,
        |task| task["wake_on"] == "error" && task["status"] == "succeeded",
        Duration::from_secs(3),
    );
    assert_eq!(
        error_ok["event_id"],
        Value::Null,
        "wake_on=error 在成功时不发事件"
    );

    let _ = queue.submit(message("/task error-fail", false));
    let error_fail = wait_for_task(
        &bridge,
        |task| task["wake_on"] == "error" && task["status"] == "failed",
        Duration::from_secs(3),
    );
    assert!(
        error_fail["event_id"].as_u64().is_some(),
        "wake_on=error 在失败时发事件"
    );

    // 正常消息开一轮，模拟 AI 在任务完成后回来查结果。
    lock(&bridge).ingest(message("在吗", true)).expect("入库");
    let delivery = lock(&bridge).poll_turn().expect("能开轮").expect("有活");
    let ticket = delivery.ticket;

    let listed = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "task_list",
            json!({"status": "failed"}),
        )
        .expect("task_list");
    assert_eq!(listed["returned"], 2);
    assert_eq!(listed["tasks"][0]["status"], "failed");

    let task_id = failed["id"].as_str().expect("task id");
    let got = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "task_get",
            json!({"id": task_id}),
        )
        .expect("task_get");
    assert_eq!(got["status"], "failed");

    // 收工后票仍在宽限窗内，任务查询不依赖任务触发时的票。
    lock(&bridge).end_turn(&ticket, None).expect("收工");
    let got_after = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "task_get",
            json!({"id": task_id}),
        )
        .expect("收工后仍可查任务");
    assert_eq!(got_after["status"], "failed");

    shutdown.store(true, Ordering::Relaxed);
    drain.join().expect("drain 线程能收工");
}

#[test]
fn task_event_wakes_through_existing_policy() {
    let bridge = bridge(
        "at_mention = false\n        reply = false\n        name_mention = false\n        event_tags = [\"task.failed\"]",
    );
    let (queue, receiver) = EventQueue::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker = RouteWorker::spawn(Arc::clone(&bridge), Arc::clone(&shutdown));
    let drain = {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            drain_events(bridge, receiver, shutdown, ControlRegistry::new(), worker)
        })
    };

    let _ = queue.submit(message("/task fail", false));
    let failed = wait_for_status(&bridge, "failed", Duration::from_secs(3));
    assert_eq!(
        failed["event_woke"].as_array().map(Vec::len),
        Some(1),
        "event_tags 命中 task.failed 时，现有 wake 策略应当开轮"
    );
    assert_eq!(failed["event_woke"][0], "main");

    lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("任务失败事件应当已经唤醒 main");

    shutdown.store(true, Ordering::Relaxed);
    drain.join().expect("drain 线程能收工");
}

#[test]
fn task_get_is_scoped_to_the_current_session() {
    let bridge = bridge("");
    let (queue, receiver) = EventQueue::channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let worker = RouteWorker::spawn(Arc::clone(&bridge), Arc::clone(&shutdown));
    let drain = {
        let bridge = Arc::clone(&bridge);
        let shutdown = Arc::clone(&shutdown);
        thread::spawn(move || {
            drain_events(bridge, receiver, shutdown, ControlRegistry::new(), worker)
        })
    };

    let _ = queue.submit(message("/task ok", false));
    let ok = wait_for_status(&bridge, "succeeded", Duration::from_secs(3));
    let task_id = ok["id"].as_str().expect("task id").to_owned();

    lock(&bridge).ingest(message("在吗", true)).expect("入库");
    let ticket = lock(&bridge)
        .poll_turn()
        .expect("能开轮")
        .expect("有活")
        .ticket;

    let error = lock(&bridge)
        .tool_call(
            &ticket,
            &["main".to_owned()],
            "task_get",
            json!({"id": format!("{task_id}-other")}),
        )
        .expect_err("不存在的 id 必须报错");
    assert!(error.message.contains("task") || error.message.contains("任务"));

    shutdown.store(true, Ordering::Relaxed);
    drain.join().expect("drain 线程能收工");
}
