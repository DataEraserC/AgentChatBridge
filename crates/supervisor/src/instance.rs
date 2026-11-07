//! 运行中 bridge 的可验证实例标识。
//!
//! acbl 重启（含升级）时不拥有 bridge：它必须先读这里的记录，确认“跑着的
//! 确实是本配置期望的那个进程”，再决定接管、拉起或报错。身份不是 pid 本身，
//! 而是 pid + 进程启动 token（Linux `/proc/<pid>/stat` 第 22 列）+ 可执行文件
//! 路径 + 命令行。

use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CommandSpec, now_ms};

pub const INSTANCE_SCHEMA: u32 = 1;
pub const INSTANCE_FILE: &str = "bridge.instance.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstanceRecord {
    pub schema: u32,
    pub instance_id: String,
    pub pid: u32,
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// 进程启动时刻，用来区分 pid 复用；拿不到时为 `None`。
    #[serde(default)]
    pub start_token: Option<String>,
    pub written_at_ms: i64,
}

/// 启动前探测的结果。
pub enum DetectOutcome {
    /// 没有可验证的运行实例，可以正常拉起。
    Vacant,
    /// 有可验证的同一实例，接管而不是再起一个。
    Adopt(Box<InstanceRecord>),
    /// 有人在跑但无法确认身份；禁止盲目再起，退出 78。
    Conflict(String),
}

/// 读取并校验 `bridge.instance.json`。
///
/// `healthy` 是调用方对 `health_url` 的一次探测结果：没有实例记录但健康端点
/// 有响应，说明有别的 bridge 在跑（例如旧 systemd 直跑），必须报错。
pub fn detect(path: &Path, expected: &CommandSpec, healthy: bool) -> DetectOutcome {
    let record = match read_record(path) {
        Ok(Some(record)) => record,
        Ok(None) => {
            if healthy {
                return DetectOutcome::Conflict(format!(
                    "bridge health endpoint is responding but {} is missing; \
                     refusing to start a second instance",
                    path.display()
                ));
            }
            return DetectOutcome::Vacant;
        }
        Err(error) => {
            if healthy {
                return DetectOutcome::Conflict(format!(
                    "cannot read {} while a bridge is healthy: {error}",
                    path.display()
                ));
            }
            return DetectOutcome::Vacant;
        }
    };

    if record.schema != INSTANCE_SCHEMA {
        return if healthy {
            DetectOutcome::Conflict(format!(
                "unsupported instance record schema {} in {}",
                record.schema,
                path.display()
            ))
        } else {
            DetectOutcome::Vacant
        };
    }

    if !pid_liveness_checkable() {
        return if healthy {
            DetectOutcome::Conflict(
                "cannot verify process identity on this platform; refusing to start a second \
                 instance"
                    .to_owned(),
            )
        } else {
            DetectOutcome::Vacant
        };
    }

    if !process_alive(record.pid) {
        // pid 已经不在：记录是陈旧的，可以正常拉起并覆盖。
        return DetectOutcome::Vacant;
    }

    match verify_record(&record, expected) {
        Ok(()) => DetectOutcome::Adopt(Box::new(record)),
        Err(reason) => DetectOutcome::Conflict(reason),
    }
}

/// 校验“记录里的进程”仍然是期望的那个实例。
pub fn verify_record(record: &InstanceRecord, expected: &CommandSpec) -> Result<(), String> {
    if record.schema != INSTANCE_SCHEMA {
        return Err(format!(
            "unsupported instance record schema {}",
            record.schema
        ));
    }
    if record.pid == 0 || !process_alive(record.pid) {
        return Err(format!("recorded bridge pid {} is not running", record.pid));
    }
    if !same_program(&record.program, &expected.program) {
        return Err(format!(
            "instance pid {} runs {:?}, expected {:?}; refusing to adopt a different binary",
            record.pid, record.program, expected.program
        ));
    }
    if record.args != expected.args {
        return Err(format!(
            "instance pid {} args {:?} do not match expected {:?}; \
             refusing to adopt a different configuration",
            record.pid, record.args, expected.args
        ));
    }
    if let Some(recorded) = &record.start_token {
        match process_start_token(record.pid) {
            Some(current) if &current == recorded => {}
            Some(current) => {
                return Err(format!(
                    "pid {} start token changed ({} -> {}); pid was reused, refusing to adopt",
                    record.pid, recorded, current
                ));
            }
            None => {
                return Err(format!(
                    "cannot read start token for pid {}; refusing to adopt",
                    record.pid
                ));
            }
        }
    }
    if let Some(exe) = process_exe(record.pid) {
        if !same_program(&exe.to_string_lossy(), &record.program) {
            return Err(format!(
                "pid {} executable is {}, record says {}; refusing to adopt",
                record.pid,
                exe.display(),
                record.program
            ));
        }
    }
    Ok(())
}

pub fn identity_matches(record: &InstanceRecord) -> bool {
    process_alive(record.pid)
        && record
            .start_token
            .as_deref()
            .map(|token| process_start_token(record.pid).as_deref() == Some(token))
            .unwrap_or(true)
}

/// 原子写入实例记录（同目录临时文件 + rename）。
pub fn write_record(path: &Path, spec: &CommandSpec, pid: u32) -> Result<(), String> {
    let record = InstanceRecord {
        schema: INSTANCE_SCHEMA,
        instance_id: new_instance_id(pid),
        pid,
        program: spec.program.clone(),
        args: spec.args.clone(),
        start_token: process_start_token(pid),
        written_at_ms: now_ms(),
    };
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temp = path.with_extension("tmp");
    let text = serde_json::to_string_pretty(&record)
        .map_err(|error| format!("serialize instance record: {error}"))?;
    fs::write(&temp, text).map_err(|error| format!("write {}: {error}", temp.display()))?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("rename {} -> {}: {error}", temp.display(), path.display())
    })
}

/// 把冲突原因留在 state 目录，便于 systemd/journal 之外复盘。
pub fn write_conflict(state_dir: &Path, reason: &str) {
    let path = state_dir.join("adopt-conflict.json");
    let body = serde_json::json!({
        "at_ms": now_ms(),
        "reason": reason,
    });
    let _ = fs::create_dir_all(state_dir);
    let _ = fs::write(
        path,
        serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string()),
    );
}

pub fn read_record(path: &Path) -> Result<Option<InstanceRecord>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("parse {}: {error}", path.display()))
}

/// 结束一个被接管的进程：先 SIGTERM，等不到再 SIGKILL。
#[allow(unsafe_code)]
pub fn terminate(pid: u32, start_token: Option<&str>) -> Result<(), String> {
    if pid == 0 {
        return Ok(());
    }
    if let (Some(expected), Some(current)) = (start_token, process_start_token(pid)) {
        if expected != current {
            return Err(format!(
                "pid {} start token changed; refusing to signal a reused pid",
                pid
            ));
        }
    }
    #[cfg(unix)]
    {
        const SIGTERM: i32 = 15;
        const SIGKILL: i32 = 9;
        if !process_alive(pid) {
            return Ok(());
        }
        unsafe {
            libc::kill(pid as i32, SIGTERM);
        }
        for _ in 0..50 {
            if !process_alive(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if process_alive(pid) {
            unsafe {
                libc::kill(pid as i32, SIGKILL);
            }
        }
        for _ in 0..20 {
            if !process_alive(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if process_alive(pid) {
            return Err(format!("pid {} did not exit after SIGKILL", pid));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = start_token;
        Err(format!(
            "cannot terminate adopted pid {pid} on this platform; \
             process adoption is not implemented outside Unix (see C-5)"
        ))
    }
}

pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        return Path::new("/proc").join(pid.to_string()).exists();
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        #[allow(unsafe_code)]
        unsafe {
            libc::kill(pid as i32, 0) == 0
        }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn pid_liveness_checkable() -> bool {
    cfg!(unix)
}

/// Linux `/proc/<pid>/stat` 的 starttime（第 22 列），用来防 pid 复用。
pub fn process_start_token(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let stat =
            fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        // 第 3 列 state 是 after_comm 的第 0 个字段，starttime 是第 22 列。
        after_comm.split_whitespace().nth(19).map(str::to_owned)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

pub fn process_exe(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let path = fs::read_link(Path::new("/proc").join(pid.to_string()).join("exe")).ok()?;
        let text = path.to_string_lossy();
        Some(PathBuf::from(
            text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned(),
        ))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn same_program(left: &str, right: &str) -> bool {
    let canonical = |value: &str| {
        Path::new(value)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(value))
    };
    canonical(left) == canonical(right)
}

fn new_instance_id(pid: u32) -> String {
    let entropy = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("acb-{}-{pid}-{entropy:016x}", now_ms())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            generation: None,
        }
    }

    #[test]
    fn missing_record_is_vacant_when_health_is_down() {
        let dir = std::env::temp_dir().join(format!("acb-instance-{}", now_ms()));
        let path = dir.join(INSTANCE_FILE);
        match detect(&path, &spec("/bin/true", &[]), false) {
            DetectOutcome::Vacant => {}
            _ => panic!("expected Vacant"),
        }
    }

    #[test]
    fn missing_record_conflicts_when_health_is_up() {
        let dir = std::env::temp_dir().join(format!("acb-instance-{}", now_ms()));
        let path = dir.join(INSTANCE_FILE);
        match detect(&path, &spec("/bin/true", &[]), true) {
            DetectOutcome::Conflict(reason) => {
                assert!(reason.contains("refusing to start a second instance"))
            }
            _ => panic!("expected Conflict"),
        }
    }

    #[test]
    fn record_for_dead_pid_is_vacant() {
        let dir = std::env::temp_dir().join(format!("acb-instance-{}", now_ms()));
        let path = dir.join(INSTANCE_FILE);
        let record = InstanceRecord {
            schema: INSTANCE_SCHEMA,
            instance_id: "test".to_owned(),
            pid: 999_999,
            program: "/bin/true".to_owned(),
            args: vec![],
            start_token: None,
            written_at_ms: now_ms(),
        };
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
        match detect(&path, &spec("/bin/true", &[]), false) {
            DetectOutcome::Vacant => {}
            _ => panic!("expected Vacant"),
        }
    }

    #[test]
    fn program_mismatch_is_conflict() {
        let record = InstanceRecord {
            schema: INSTANCE_SCHEMA,
            instance_id: "test".to_owned(),
            pid: std::process::id(),
            program: "/bin/true".to_owned(),
            args: vec![],
            start_token: process_start_token(std::process::id()),
            written_at_ms: now_ms(),
        };
        assert!(verify_record(&record, &spec("/bin/false", &[])).is_err());
    }
}
