//! 停机收敛的后代快照。
//!
//! unit 用 `KillMode=process`，systemd 只杀主进程。组件和 bridge 各有进程组，
//! 但 dsh 会把 subprocess 放进独立的 systemd scope；scope 进程既不在组里，父
//! 进程一死还会被 reparent。所以在任何停止动作之前先按 `/proc` 拍下整棵后代
//! 树，停机末尾按快照 pid 兜底 TERM/​KILL。
//!
//! 只看 ppid 树不够：组件在运行中崩溃后，它拉起的 sidecar 会被 reparent 到
//! systemd（ppid=1/用户 manager），但**不会离开 unit cgroup**，也常常还留在
//! 原组件的进程组里（组长已死）。因此 [`Snapshot::capture_unit`] 额外反查
//! unit cgroup 的成员，并且只对那些「组长已死的孤儿组」做兜底，避免误杀
//! `leave_running` 容器（其 conmon/rootlessport 组长都还活着）。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    /// 进程组 id（`pgrp`）。cgroup 反查到的记录靠「组长是否还活着」判断孤儿组。
    pgrp: u32,
    /// `/proc/<pid>/stat` 的 starttime，用来防 pid 复用后误杀。
    start: u64,
    /// true 表示来自 unit cgroup 反查，而不是 ppid 后代树。
    from_cgroup: bool,
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

    /// 后代树 ∪ unit cgroup 成员。
    ///
    /// 只有 launcher 在 systemd service 里（专用 unit cgroup）时才反查 cgroup：
    /// 交互式 shell / `acbctl run` 的 cgroup 是 session scope，扫它可能误杀
    /// 无关进程。cgroup 记录打上 `from_cgroup`，收敛时只处理孤儿组。
    pub fn capture_unit(roots: &[u32]) -> Self {
        let mut entries = collect_descendants(roots);
        let Some(unit) = unit_service_cgroup() else {
            return Self { entries };
        };
        let known: HashSet<u32> = entries
            .iter()
            .map(|entry| entry.pid)
            .chain(roots.iter().copied())
            .collect();
        for member in collect_cgroup_members(&unit) {
            if known.contains(&member.pid) {
                continue;
            }
            entries.push(member);
        }
        Self { entries }
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
            // ppid 后代树里的照旧收敛；cgroup 反查到的只收孤儿组（组长已死），
            // 否则 leave_running 容器（conmon/rootlessport 组长活着）会被误杀。
            .filter(|entry| !entry.from_cgroup || group_leader_dead(entry.pgrp))
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
                "acb: {label}: descendant pid {} ({}, pgid {}{}) did not exit before deadline; SIGKILL",
                target.pid,
                comm(target.pid),
                target.pgrp,
                if target.from_cgroup {
                    ", orphaned cgroup residue"
                } else {
                    ""
                }
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
            if stat.3 == 'Z' {
                continue;
            }
            children.entry(stat.0).or_default().push(Proc {
                pid,
                ppid: stat.0,
                pgrp: stat.1,
                start: stat.2,
                from_cgroup: false,
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

/// 自己所在的 unit cgroup（仅 unified hierarchy）。
///
/// 只在 systemd service 里启用 cgroup 反查：交互式 shell / `acbctl run` 的
/// cgroup 是 session scope，扫它会把用户会话里的无关进程当成残留。
#[cfg(target_os = "linux")]
fn unit_service_cgroup() -> Option<String> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_string)?;
    path.ends_with(".service").then_some(path)
}

#[cfg(not(target_os = "linux"))]
fn unit_service_cgroup() -> Option<String> {
    None
}

/// 扫 `/proc` 收集 `unit` cgroup 的成员（含嵌套子 cgroup）。
#[cfg(target_os = "linux")]
fn collect_cgroup_members(unit: &str) -> Vec<Proc> {
    let mut members = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return members;
    };
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
        if stat.3 == 'Z' {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
            continue;
        };
        let Some(cgroup) = text
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .map(str::trim)
        else {
            continue;
        };
        if !in_unit(cgroup, unit) {
            continue;
        }
        members.push(Proc {
            pid,
            ppid: stat.0,
            pgrp: stat.1,
            start: stat.2,
            from_cgroup: true,
        });
    }
    members
}

#[cfg(not(target_os = "linux"))]
fn collect_cgroup_members(_unit: &str) -> Vec<Proc> {
    Vec::new()
}

/// `cgroup == unit` 或以 `unit/` 为前缀（排除 `...service2` 这类同前缀误匹配）。
fn in_unit(cgroup: &str, unit: &str) -> bool {
    cgroup == unit
        || cgroup
            .strip_prefix(unit)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// 进程组组长是否已不存在（含 zombie）。孤儿组 = 组件崩掉后 sidecar 所在组。
fn group_leader_dead(pgrp: u32) -> bool {
    if pgrp == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        !matches!(read_stat(pgrp), Some((_, _, _, state)) if state != 'Z')
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

/// 返回 `(ppid, pgrp, starttime, state)`；读不到就当作进程不存在。
#[cfg(target_os = "linux")]
fn read_stat(pid: u32) -> Option<(u32, u32, u64, char)> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = text.get(text.rfind(')')? + 1..)?.trim_start();
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    // 吃掉 pgrp 后，剩余从 field 6 开始；starttime 是 field 22，即 index 16。
    let start = fields
        .enumerate()
        .find(|(index, _)| *index == 16)
        .and_then(|(_, field)| field.parse().ok())?;
    Some((ppid, pgrp, start, state))
}

fn alive(entry: &Proc) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(read_stat(entry.pid), Some((_, _, start, state)) if start == entry.start && state != 'Z')
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn subtrees_include_descendants_only() {
        let snapshot = Snapshot {
            entries: vec![
                Proc {
                    pid: 2,
                    ppid: 1,
                    pgrp: 2,
                    start: 0,
                    from_cgroup: false,
                },
                Proc {
                    pid: 3,
                    ppid: 2,
                    pgrp: 2,
                    start: 0,
                    from_cgroup: false,
                },
                Proc {
                    pid: 4,
                    ppid: 9,
                    pgrp: 4,
                    start: 0,
                    from_cgroup: false,
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

    #[test]
    fn orphaned_cgroup_group_is_reaped() {
        // 组件崩掉后 sidecar 的典型形态：组长（组件）已退出，成员还留在组里。
        let dir = std::env::temp_dir().join(format!("acb-proctree-orphan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let pid_file = dir.join("orphan.pid");
        let mut leader = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 60 & echo $! > {}",
                pid_file.display()
            ))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn leader");
        let leader_pid = leader.id();
        leader.wait().expect("leader exits");
        let member = wait_pid(&pid_file);
        let (ppid, pgrp, start, _) = read_stat(member).expect("member stat");
        assert_eq!(pgrp, leader_pid, "成员应留在已退出的组长的进程组");
        assert!(group_leader_dead(pgrp), "组长已退出，组应判为孤儿");

        let snapshot = Snapshot {
            entries: vec![Proc {
                pid: member,
                ppid,
                pgrp,
                start,
                from_cgroup: true,
            }],
        };
        snapshot.terminate_leftovers(
            &HashSet::new(),
            Instant::now() + Duration::from_millis(300),
            "test",
        );
        assert!(
            !matches!(read_stat(member), Some((_, _, _, state)) if state != 'Z'),
            "孤儿组里的 sidecar 必须被收敛"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_group_cgroup_member_survives() {
        // leave_running 容器的形态：组长（conmon/rootlessport）还活着。
        let dir = std::env::temp_dir().join(format!("acb-proctree-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let pid_file = dir.join("member.pid");
        let mut leader = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 60 & echo $! > {}; wait",
                pid_file.display()
            ))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn leader");
        let leader_pid = leader.id();
        let member = wait_pid(&pid_file);
        let (ppid, pgrp, start, _) = read_stat(member).expect("member stat");
        assert_eq!(pgrp, leader_pid);
        assert!(!group_leader_dead(pgrp), "组长还活着，组不是孤儿");

        let snapshot = Snapshot {
            entries: vec![Proc {
                pid: member,
                ppid,
                pgrp,
                start,
                from_cgroup: true,
            }],
        };
        snapshot.terminate_leftovers(
            &HashSet::new(),
            Instant::now() + Duration::from_millis(300),
            "test",
        );
        assert!(
            matches!(read_stat(member), Some((_, _, _, state)) if state != 'Z'),
            "活跃组的 cgroup 成员不得被误杀"
        );

        signal(leader_pid, SIGKILL);
        signal(member, SIGKILL);
        let _ = leader.wait();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unit_cgroup_prefix_matches_only_same_unit() {
        assert!(in_unit("/user.slice/a.service", "/user.slice/a.service"));
        assert!(in_unit("/user.slice/a.service/nested", "/user.slice/a.service"));
        assert!(!in_unit("/user.slice/a.service2", "/user.slice/a.service"));
        assert!(!in_unit("/user.slice/other.service", "/user.slice/a.service"));
    }

    fn wait_pid(path: &std::path::Path) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(Instant::now() < deadline, "child pid file not written");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
