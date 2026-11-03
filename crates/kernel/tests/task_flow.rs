//! T7a：后台任务记录与 task_list / task_get。
//!
//! 真实走 drain 线程 → RouteWorker → exec，验证任务状态从 queued/running 到
//! succeeded/failed，以及工具查询在轮次结束后仍然可用。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_kernel::{
    Bridge, Config, ControlRegistry, Element, EventQueue, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, RecordDraft, RoomId,
    RouteAction, RouteContext, RouteDecision, RoutePolicy, RouteProvider, RouteWorker, Sender,
    Sent, drain_events,
};
use serde_json::{Value, json};

fn config() -> &'static str {
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
    "#
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
        let command = if context.text.starts_with("/task ok") {
            vec!["sh", "-c", "printf ok"]
        } else if context.text.starts_with("/task fail") {
            vec!["sh", "-c", "exit 7"]
        } else {
            return RouteDecision::Pass;
        };
        RouteDecision::Handle {
            store: false,
            wake: false,
            actions: vec![RouteAction::Task {
                handler: "exec".into(),
                params: json!({ "command": command, "send": "text" }),
            }],
        }
    }
}

fn bridge() -> Arc<Mutex<Bridge<MemoryStore>>> {
    let id = ProviderId::parse("test.task").expect("合法");
    let mut plugins = PluginManager::with_native_builtins();
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
        Config::parse(config()).expect("配置合法"),
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
fn task_records_success_failure_and_can_be_queried_after_turn_end() {
    let bridge = bridge();
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

    queue.submit(message("/task ok", false));
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

    queue.submit(message("/task fail", false));
    let failed = wait_for_status(&bridge, "failed", Duration::from_secs(3));
    assert_eq!(failed["handler"], "exec");
    assert!(failed["error"].as_str().is_some(), "失败原因可见");

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
    assert_eq!(listed["returned"], 1);
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
fn task_get_is_scoped_to_the_current_session() {
    let bridge = bridge();
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

    queue.submit(message("/task ok", false));
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
