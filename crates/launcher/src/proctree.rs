//! 停机收敛的后代快照。
//!
//! unit 用 `KillMode=process`，systemd 只杀 launcher 主进程。组件和 bridge 各有
//! 进程组，但 dsh 会把 subprocess 放进独立的 systemd scope；scope 进程既不在
//! 组里，父进程一死还会被 reparent。所以在任何停止动作之前先按 `/proc` 拍下
//! 整棵后代树，停机末尾按快照 pid 兜底 TERM/​KILL。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    /// `/proc/<pid>/stat` 的 starttime，用来防 pid 复用后误杀。
    start: u64,
}

#[derive(Debug, Default)]
pub struct Snapshot {
    entries: Vec<Proc>,
}

impl Snapshot {
    /// 多个根：launcher 自己 + 接管的 bridge（后者不是 launcher 的子进程）。
    /// 只收后代，不含 root 自身。
    pub fn capture_roots(roots: &[u32]) -> Self {
        Self {
            entries: collect_descendants(roots),
        }
    }

    /// 展开 `roots` 及其后代，作为「不许兜底误杀」的保护集。
    pub fn subtrees(&self, roots: &[u32]) -> HashSet<u32> {
        let mut protected: HashSet<u32> = roots.iter().copied().collect();
        loop {
            let before = protected.len();
            for entry in &self.entries {
                if protected.contains(&entry.ppid) {
                    protected.insert(entry.pid);
                }
            }
            if protected.len() == before {
                return protected;
            }
        }
    }

    /// 对快照内未被保护的进程先 TERM；`deadline` 前没退的直接 KILL 并逐条记录。
    pub fn terminate_leftovers(&self, protect: &HashSet<u32>, deadline: Instant, label: &str) {
        let targets: Vec<Proc> = self
            .entries
            .iter()
            .filter(|entry| !protect.contains(&entry.pid))
            .cloned()
            .collect();
        if targets.is_empty() {
            return;
        }
        for target in &targets {
            if alive(target) {
                signal(target.pid, SIGTERM);
            }
        }
        while targets.iter().any(alive) {
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        for target in &targets {
            if !alive(target) {
                continue;
            }
            eprintln!(
                "acb-launcher: {label}: descendant pid {} ({}) did not exit before shutdown deadline; SIGKILL",
                target.pid,
                comm(target.pid)
            );
            signal(target.pid, SIGKILL);
        }
        // 给 SIGKILL 一个生效窗口；不扩大停机上界。
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
fn collect_descendants(roots: &[u32]) -> Vec<Proc> {
    let mut children: HashMap<u32, Vec<Proc>> = HashMap::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for item in dir.flatten() {
            let Some(pid) = item
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Some(stat) = read_stat(pid) else {
                continue;
            };
            if stat.2 == 'Z' {
                continue;
            }
            children.entry(stat.0).or_default().push(Proc {
                pid,
                ppid: stat.0,
                start: stat.1,
            });
        }
    }

    let mut seen: HashSet<u32> = roots.iter().copied().collect();
    let mut queue: Vec<u32> = roots.iter().copied().filter(|pid| *pid != 0).collect();
    let mut descendants = Vec::new();
    while let Some(parent) = queue.pop() {
        let Some(kids) = children.remove(&parent) else {
            continue;
        };
        for kid in kids {
            if !seen.insert(kid.pid) {
                continue;
            }
            queue.push(kid.pid);
            descendants.push(kid);
        }
    }
    descendants
}

#[cfg(not(target_os = "linux"))]
fn collect_descendants(_roots: &[u32]) -> Vec<Proc> {
    Vec::new()
}

/// 返回 `(ppid, starttime, state)`；读不到就当作进程不存在。
#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<(u32, u64, char)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.get(text.rfind(')')? + 1..)?.trim_start();
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    // 从 state 之后数：pgrp 是第 0 个，starttime 是第 17 个。
    let start = fields
        .enumerate()
        .find(|(index, _)| *index == 17)
        .and_then(|(_, field)| field.parse().ok())?;
    Some((ppid, start, state))
}

fn alive(entry: &Proc) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(read_stat(entry.pid), Some((_, start, state)) if start == entry.start && state != 'Z')
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = entry;
        false
    }
}

fn comm(pid: u32) -> String {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|text| text.trim().to_string())
            .unwrap_or_else(|_| "?".to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        "?".to_string()
    }
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn signal(pid: u32, signal: i32) {
    // SAFETY: 只向快照里确认过 starttime 的后代发送信号。
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

#[cfg(not(unix))]
fn signal(_pid: u32, _signal: i32) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    #[test]
    fn subtrees_include_descendants_only() {
        let snapshot = Snapshot {
            entries: vec![
                Proc {
                    pid: 2,
                    ppid: 1,
                    start: 0,
                },
                Proc {
                    pid: 3,
                    ppid: 2,
                    start: 0,
                },
                Proc {
                    pid: 4,
                    ppid: 9,
                    start: 0,
                },
            ],
        };
        let protected = snapshot.subtrees(&[1]);
        assert!(protected.contains(&1));
        assert!(protected.contains(&2));
        assert!(protected.contains(&3));
        assert!(!protected.contains(&4));
    }

    #[test]
    fn force_kills_stubborn_descendants_after_deadline() {
        // shell 忽略 TERM，再挂一个也忽略 TERM 的孙进程。
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sh -c 'trap \"\" TERM; sleep 60' & wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tree");
        let root = child.id();
        let mut snapshot = Snapshot::capture_roots(&[root]);
        let started = Instant::now();
        while snapshot.entries.is_empty() && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
            snapshot = Snapshot::capture_roots(&[root]);
        }
        assert!(!snapshot.entries.is_empty(), "孙进程必须先出现");

        let started = Instant::now();
        snapshot.terminate_leftovers(
            &HashSet::new(),
            Instant::now() + Duration::from_millis(300),
            "test",
        );
        // 孙进程被 SIGKILL 后 `wait` 返回；若超时上界失效，这里会阻塞 60s。
        child.wait().expect("wait tree");
        assert!(started.elapsed() < Duration::from_secs(5), "停机被卡住");
        assert!(!alive(&snapshot.entries[0]), "孙进程必须被 SIGKILL");
    }

    #[test]
    fn protected_subtrees_survive() {
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sh -c 'sleep 60' & wait")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn tree");
        let root = child.id();
        let mut snapshot = Snapshot::capture_roots(&[root]);
        let started = Instant::now();
        while snapshot.entries.is_empty() && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
            snapshot = Snapshot::capture_roots(&[root]);
        }
        let protect = snapshot.subtrees(&[root]);
        snapshot.terminate_leftovers(
            &protect,
            Instant::now() + Duration::from_millis(300),
            "test",
        );
        assert!(alive(&snapshot.entries[0]), "被保护的子进程不得被杀");
        let _ = child.kill();
        let _ = child.wait();
    }
}
