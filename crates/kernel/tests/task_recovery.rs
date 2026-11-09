//! 崩溃收尾：上一次进程留下的未闭环任务，重启后必须有一个结局。
//!
//! `pending` / `running` 的语义是「**本进程**正在管」。进程被杀之后库里这些行没人负责，
//! 但状态看起来还在跑：`tasks op=wait` 等到死，`drain_snapshot` 还把 running 算作在跑，
//! SIGTERM 的排空因此永远不完成。启动时统一置 `unknown`（不是 `failed`——消息可能已经
//! 发出去了，说失败是编）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    Bridge, Config, Element, Outbound, OutboundError, RoomId, Sent, SqliteStore, TaskRecord,
    TaskStatus, Tasks,
};

const AT: i64 = 1_793_000_000_000;

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("acb-recover-{}-{name}-{nanos}", std::process::id()));
        Self {
            path: dir.join("acb.db"),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: AT,
        })
    }
}

fn task(id: &str, status: TaskStatus) -> TaskRecord {
    TaskRecord {
        id: id.to_owned(),
        session: "main".to_owned(),
        room: RoomId::parse("qq:group/1").expect("房间合法"),
        handler: "exec".to_owned(),
        params_summary: "printf ok".to_owned(),
        wake_on: "always".to_owned(),
        status,
        created_at: AT,
        started_at: Some(AT),
        finished_at: None,
        result: None,
        error: None,
        delivery_error: None,
        event_id: None,
        event_woke: Vec::new(),
        event_error: None,
        platform_id: None,
        idempotency_key: None,
    }
}

#[test]
fn unfinished_tasks_from_a_dead_process_become_unknown_on_startup() {
    let db = TempDb::new("reclaim");

    // 模拟「上一次进程正在执行任务时被杀」：一行排队、一行在跑，都没结局。
    {
        let mut store = SqliteStore::open(db.path()).expect("开库");
        store
            .task_create(&task("t-pending", TaskStatus::Pending))
            .expect("写 pending");
        store
            .task_create(&task("t-running", TaskStatus::Running))
            .expect("写 running");
        let mut settled = task("t-done", TaskStatus::Succeeded);
        settled.finished_at = Some(AT + 1);
        store.task_create(&settled).expect("写 succeeded");
    } // store 掉线 —— 等于进程死亡

    {
        let store = SqliteStore::open(db.path()).expect("重开");
        let config = Config::parse_fixture("").expect("空配置合法");
        let bridge = Bridge::new(config, store, Box::new(NoopOutbound));

        // 排空判据不能再把上次遗留的运行中任务算作「本进程还在跑」。
        let snapshot = bridge.drain_snapshot();
        assert_eq!(
            snapshot["running_tasks"], 0,
            "遗留 running 不能算本进程在跑"
        );
        assert_eq!(snapshot["queued_tasks"], 0, "遗留 pending 不能算排队中");
    }

    // 状态本身也要对：是 unknown（不知道），不是 failed（假的确定）。
    let store = SqliteStore::open(db.path()).expect("再看一眼");
    for id in ["t-pending", "t-running"] {
        let record = store.task_find(id).expect("可读").expect("记录还在");
        assert_eq!(record.status, TaskStatus::Unknown, "{id} 应置 unknown");
        assert!(
            record.finished_at.is_some(),
            "{id} 必须有收尾时间，否则分不清「在跑」和「已收尾」"
        );
        assert!(
            record.error.as_deref().is_some_and(|text| !text.is_empty()),
            "{id} 要写明为什么是 unknown"
        );
    }
    // 已经闭环的记录不许被动。
    let done = store.task_find("t-done").expect("可读").expect("记录还在");
    assert_eq!(done.status, TaskStatus::Succeeded);
    assert!(done.error.is_none(), "成功记录不该被写上错误");
}
