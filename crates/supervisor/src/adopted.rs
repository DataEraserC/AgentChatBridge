//! 接管一个已经在运行的 bridge。
//!
//! acbl 不是 bridge 的所有者：`systemctl stop/restart acb-launcher` 只应影响
//! acbl 自己（unit 里配 `KillMode=process`）。新 acbl 起来后通过
//! [`crate::instance::detect`] 找到旧实例，用本 runner 接管；只有接管失败或
//! 实例已经退出时，才回退到正常的 `ProcessRunner` 拉起。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use crate::instance::{self, InstanceRecord};
use crate::proctree::Snapshot;
use crate::{CommandSpec, DEFAULT_CHILD_STOP_TIMEOUT, ExitInfo, ProcessRunner, Runner};

pub struct AdoptedRunner {
    adopted: Option<InstanceRecord>,
    fallback: ProcessRunner,
}

impl AdoptedRunner {
    pub fn new(record: InstanceRecord, fallback: ProcessRunner) -> Self {
        Self {
            adopted: Some(record),
            fallback,
        }
    }

    /// 接管实例退出后，后续重启仍要写实例记录。
    pub fn with_instance_file(record: InstanceRecord, path: impl Into<PathBuf>) -> Self {
        Self::new(record, ProcessRunner::with_instance_file(path))
    }
}

impl Runner for AdoptedRunner {
    fn start(&mut self, spec: &CommandSpec) -> Result<(), String> {
        if let Some(record) = &self.adopted {
            if record.program == spec.program
                && record.args == spec.args
                && instance::identity_matches(record)
            {
                // Supervisor::new 会立刻调用 start；这里只确认，不重复拉起。
                return Ok(());
            }
            self.adopted = None;
        }
        self.fallback.start(spec)
    }

    fn stop(&mut self) -> Result<(), String> {
        self.stop_within(Instant::now() + DEFAULT_CHILD_STOP_TIMEOUT)
    }

    fn stop_within(&mut self, deadline: Instant) -> Result<(), String> {
        if let Some(record) = self.adopted.take() {
            // 被接管的 bridge 不是 launcher 的子进程，它的插件子进程也不在
            // 新 launcher 的后代树里。先拍快照，停掉 bridge 后再兜底收子进程。
            let snapshot = Snapshot::capture_roots(&[record.pid]);
            let result =
                instance::terminate_within(record.pid, record.start_token.as_deref(), deadline);
            snapshot.terminate_leftovers(&HashSet::new(), deadline, "adopted bridge");
            result
        } else {
            self.fallback.stop_within(deadline)
        }
    }

    fn running(&mut self) -> bool {
        if let Some(record) = &self.adopted {
            if instance::identity_matches(record) {
                return true;
            }
            self.adopted = None;
        }
        self.fallback.running()
    }

    fn last_exit(&mut self) -> Option<ExitInfo> {
        if self.adopted.is_some() {
            None
        } else {
            self.fallback.last_exit()
        }
    }

    fn pid(&mut self) -> Option<u32> {
        self.adopted
            .as_ref()
            .map(|record| record.pid)
            .or_else(|| self.fallback.pid())
    }
}
