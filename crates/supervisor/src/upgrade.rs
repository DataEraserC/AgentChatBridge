//! 升级请求信箱：ACB 只原子写请求，launcher 认领后执行切换。
//!
//! 目录约定（全部在 supervisor `state_dir` 下）：
//!
//! ```text
//! upgrade/
//!   requests/<id>.json           ACB / CLI 写
//!   inflight/<id>.json           launcher 认领后
//!   history/<id>.request.json    已结束的请求
//!   history/<id>.result.json     结果
//!   active.json                  当前在途请求
//! ```
//!
//! 这里不实现指针切换；切代接口留给 `profile` 的原子提交落地后替换。

use std::collections::BTreeMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const UPGRADE_SCHEMA: u32 = 1;
pub const DEFAULT_TTL_MS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpgradeTarget {
    Generation { id: String },
    Payload { path: String, strategy: String },
}

impl UpgradeTarget {
    pub fn generation_id(&self) -> Option<&str> {
        match self {
            Self::Generation { id } => Some(id),
            Self::Payload { .. } => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Generation { id } => format!("generation {id}"),
            Self::Payload { path, strategy } => format!("payload {path} ({strategy})"),
        }
    }

    pub fn strategy(&self) -> Option<&str> {
        match self {
            Self::Payload { strategy, .. } => Some(strategy),
            Self::Generation { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeRequest {
    pub schema: u32,
    pub request_id: String,
    #[serde(default)]
    pub idempotency_key: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub expires_at_ms: i64,
    pub action: String,
    pub target: UpgradeTarget,
    #[serde(default)]
    pub expected_current_id: Option<String>,
    #[serde(default)]
    pub issue_id: Option<String>,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub requested_by: BTreeMap<String, String>,
}

impl UpgradeRequest {
    pub fn new(request_id: impl Into<String>, target: UpgradeTarget, now_ms: i64) -> Self {
        let request_id = request_id.into();
        Self {
            schema: UPGRADE_SCHEMA,
            idempotency_key: request_id.clone(),
            request_id,
            created_at_ms: now_ms,
            expires_at_ms: now_ms + DEFAULT_TTL_MS,
            action: "upgrade".to_string(),
            target,
            expected_current_id: None,
            issue_id: None,
            reason: String::new(),
            requested_by: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeResult {
    pub schema: u32,
    pub request_id: String,
    pub status: String,
    pub reason_code: String,
    #[serde(default)]
    pub reason: String,
    pub at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_generation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_request_id: Option<String>,
}

impl UpgradeResult {
    pub fn new(request_id: &str, status: &str, reason_code: &str, at_ms: i64) -> Self {
        Self {
            schema: UPGRADE_SCHEMA,
            request_id: request_id.to_string(),
            status: status.to_string(),
            reason_code: reason_code.to_string(),
            reason: String::new(),
            at_ms,
            target_generation_id: None,
            active_request_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveUpgrade {
    pub schema: u32,
    pub request_id: String,
    pub status: String,
    pub target_generation_id: Option<String>,
    pub updated_at_ms: i64,
    pub request: UpgradeRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub schema: u32,
    pub restart_owner: String,
    pub launcher_pid: u32,
    pub profile_dir: String,
    pub state_dir: String,
    pub strategies: Vec<String>,
    pub updated_at_ms: i64,
}

impl Capabilities {
    pub fn has_restart_owner(&self) -> bool {
        self.restart_owner != "none" && self.restart_owner != "direct"
    }
}

#[derive(Debug)]
pub enum SubmitOutcome {
    Queued {
        request_id: String,
        path: PathBuf,
    },
    Duplicate {
        request_id: String,
        status: String,
        path: PathBuf,
    },
    Busy {
        active_request_id: String,
    },
    Rejected {
        reason_code: String,
        reason: String,
    },
}

pub struct Mailbox {
    state_dir: PathBuf,
}

impl Mailbox {
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self {
            state_dir: state_dir.as_ref().to_path_buf(),
        }
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn root(&self) -> PathBuf {
        self.state_dir.join("upgrade")
    }

    pub fn requests_dir(&self) -> PathBuf {
        self.root().join("requests")
    }

    pub fn inflight_dir(&self) -> PathBuf {
        self.root().join("inflight")
    }

    pub fn history_dir(&self) -> PathBuf {
        self.root().join("history")
    }

    pub fn active_path(&self) -> PathBuf {
        self.root().join("active.json")
    }

    pub fn capabilities_path(&self) -> PathBuf {
        self.state_dir.join("capabilities.json")
    }

    pub fn ensure(&self) -> Result<(), String> {
        for dir in [
            self.root(),
            self.requests_dir(),
            self.inflight_dir(),
            self.history_dir(),
        ] {
            fs::create_dir_all(&dir)
                .map_err(|error| format!("create {}: {error}", dir.display()))?;
        }
        Ok(())
    }

    pub fn write_capabilities(&self, capabilities: &Capabilities) -> Result<(), String> {
        self.ensure()?;
        write_json(
            &self.capabilities_path(),
            serde_json::to_vec_pretty(capabilities)
                .map_err(|error| format!("serialize capabilities: {error}"))?
                .as_slice(),
        )
    }

    pub fn read_capabilities(&self) -> Option<Capabilities> {
        let path = self.capabilities_path();
        let text = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn restart_owner(&self) -> Option<String> {
        let capabilities = self.read_capabilities()?;
        if !capabilities.has_restart_owner() {
            return None;
        }
        if !process_alive(capabilities.launcher_pid) {
            return None;
        }
        Some(capabilities.restart_owner)
    }

    pub fn submit(&self, request: UpgradeRequest) -> Result<SubmitOutcome, String> {
        self.ensure()?;
        validate_request(&request)?;

        if let Some(active) = self.read_active() {
            if !is_terminal_status(&active.status) {
                return Ok(SubmitOutcome::Busy {
                    active_request_id: active.request_id,
                });
            }
        }

        if let Some(outcome) = self.find_duplicate(&request)? {
            return Ok(outcome);
        }

        match self.restart_owner() {
            Some(_) => {}
            None => {
                return Ok(SubmitOutcome::Rejected {
                    reason_code: "no_restart_owner".to_string(),
                    reason: "launcher capability is absent or restart_owner=none".to_string(),
                });
            }
        }

        let path = self.requests_dir().join(format!("{}.json", request.request_id));
        let bytes = serde_json::to_vec_pretty(&request)
            .map_err(|error| format!("serialize upgrade request: {error}"))?;
        write_json(&path, &bytes)?;
        Ok(SubmitOutcome::Queued {
            request_id: request.request_id,
            path,
        })
    }

    fn find_duplicate(&self, request: &UpgradeRequest) -> Result<Option<SubmitOutcome>, String> {
        let same_key = |candidate: &UpgradeRequest| {
            candidate.request_id == request.request_id
                || (!request.idempotency_key.is_empty()
                    && candidate.idempotency_key == request.idempotency_key)
        };

        for (path, candidate) in self.read_requests(&self.requests_dir())? {
            if same_key(&candidate) {
                return Ok(Some(SubmitOutcome::Duplicate {
                    request_id: candidate.request_id,
                    status: "queued".to_string(),
                    path,
                }));
            }
            if candidate.target == request.target {
                return Ok(Some(SubmitOutcome::Duplicate {
                    request_id: candidate.request_id,
                    status: "queued".to_string(),
                    path,
                }));
            }
        }
        for (path, candidate) in self.read_requests(&self.inflight_dir())? {
            if same_key(&candidate) || candidate.target == request.target {
                return Ok(Some(SubmitOutcome::Duplicate {
                    request_id: candidate.request_id,
                    status: "inflight".to_string(),
                    path,
                }));
            }
        }
        for (path, candidate) in self.read_requests(&self.history_dir())? {
            if same_key(&candidate) {
                let status = self
                    .read_result(&candidate.request_id)
                    .map(|result| result.status)
                    .unwrap_or_else(|| "completed".to_string());
                return Ok(Some(SubmitOutcome::Duplicate {
                    request_id: candidate.request_id,
                    status,
                    path,
                }));
            }
        }
        Ok(None)
    }

    pub fn read_active(&self) -> Option<ActiveUpgrade> {
        let text = fs::read_to_string(self.active_path()).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn write_active(&self, active: &ActiveUpgrade) -> Result<(), String> {
        self.ensure()?;
        let bytes = serde_json::to_vec_pretty(active)
            .map_err(|error| format!("serialize active upgrade: {error}"))?;
        write_json(&self.active_path(), &bytes)
    }

    pub fn clear_active(&self) -> Result<(), String> {
        match fs::remove_file(self.active_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {}: {error}", self.active_path().display())),
        }
    }

    pub fn write_result(&self, result: &UpgradeResult) -> Result<(), String> {
        self.ensure()?;
        let path = self
            .history_dir()
            .join(format!("{}.result.json", result.request_id));
        let bytes = serde_json::to_vec_pretty(result)
            .map_err(|error| format!("serialize upgrade result: {error}"))?;
        write_json(&path, &bytes)
    }

    pub fn read_result(&self, request_id: &str) -> Option<UpgradeResult> {
        let path = self
            .history_dir()
            .join(format!("{request_id}.result.json"));
        let text = fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn archive_request(&self, request_id: &str, from_inflight: bool) -> Result<(), String> {
        let source = if from_inflight {
            self.inflight_dir()
        } else {
            self.requests_dir()
        }
        .join(format!("{request_id}.json"));
        let target = self
            .history_dir()
            .join(format!("{request_id}.request.json"));
        if !source.exists() {
            return Ok(());
        }
        match fs::rename(&source, &target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&source);
                Ok(())
            }
            Err(error) => Err(format!(
                "archive {} -> {}: {error}",
                source.display(),
                target.display()
            )),
        }
    }

    /// 认领下一份请求。返回 `None` 表示当前没有可执行请求。
    pub fn claim_next(&self, now_ms: i64) -> Result<Option<(UpgradeRequest, PathBuf)>, String> {
        self.ensure()?;
        let mut requests = self.read_requests(&self.requests_dir())?;
        if requests.is_empty() {
            return Ok(None);
        }
        requests.sort_by_key(|(_path, request)| {
            (request.created_at_ms, request.request_id.clone())
        });

        if let Some(active) = self.read_active() {
            if !is_terminal_status(&active.status) {
                return Ok(None);
            }
        }

        let (path, request) = requests.remove(0);
        if is_expired(&request, now_ms) {
            let mut result = UpgradeResult::new(
                &request.request_id,
                "expired",
                "request_expired",
                now_ms,
            );
            result.target_generation_id = request.target.generation_id().map(str::to_string);
            self.write_result(&result)?;
            self.archive_request(&request.request_id, false)?;
            self.clear_active()?;
            return Ok(None);
        }

        // 并发请求按“忙则拒绝、不排队”：除最早的这份外，其余直接结案。
        for (other_path, other) in requests {
            let mut result =
                UpgradeResult::new(&other.request_id, "rejected", "busy", now_ms);
            result.active_request_id = Some(request.request_id.clone());
            result.target_generation_id = other.target.generation_id().map(str::to_string);
            self.write_result(&result)?;
            let _ = fs::remove_file(&other_path);
            self.archive_request(&other.request_id, false)?;
        }

        let inflight = self
            .inflight_dir()
            .join(format!("{}.json", request.request_id));
        fs::rename(&path, &inflight).map_err(|error| {
            format!(
                "claim {} -> {}: {error}",
                path.display(),
                inflight.display()
            )
        })?;
        write_json(
            &inflight,
            &serde_json::to_vec_pretty(&request)
                .map_err(|error| format!("serialize inflight request: {error}"))?,
        )?;

        let active = ActiveUpgrade {
            schema: UPGRADE_SCHEMA,
            request_id: request.request_id.clone(),
            status: "claimed".to_string(),
            target_generation_id: request.target.generation_id().map(str::to_string),
            updated_at_ms: now_ms,
            request: request.clone(),
        };
        self.write_active(&active)?;
        Ok(Some((request, inflight)))
    }

    pub fn set_active_status(&self, status: &str, now_ms: i64) -> Result<Option<ActiveUpgrade>, String> {
        let Some(mut active) = self.read_active() else {
            return Ok(None);
        };
        active.status = status.to_string();
        active.updated_at_ms = now_ms;
        self.write_active(&active)?;
        Ok(Some(active))
    }

    /// 指针提交后进入 restarting，并把实际要启动的代补进 active.json。
    ///
    /// payload 目标在 claim 时还生成不出代 id（`generation_id()` 返回 None），
    /// 若这里不补，重启后的 `resume_active` 拿 current.json 与 None 比对必然不等，
    /// 成功切换会被误判成 `restart_did_not_switch`（假阴性）。
    pub fn mark_restarting(
        &self,
        target_id: &str,
        now_ms: i64,
    ) -> Result<Option<ActiveUpgrade>, String> {
        let Some(mut active) = self.read_active() else {
            return Ok(None);
        };
        active.status = "restarting".to_string();
        active.target_generation_id = Some(target_id.to_string());
        active.updated_at_ms = now_ms;
        self.write_active(&active)?;
        Ok(Some(active))
    }

    fn read_requests(&self, dir: &Path) -> Result<Vec<(PathBuf, UpgradeRequest)>, String> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(format!("read {}: {error}", dir.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| format!("read {}: {error}", dir.display()))?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            // history/ 下同时有 `<id>.request.json` 和 `<id>.result.json`；只跳过结果文件，
            // 归档请求仍需参与重复判定。
            if path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with(".result.json"))
            {
                continue;
            }
            let text = fs::read_to_string(&path)
                .map_err(|error| format!("read {}: {error}", path.display()))?;
            let request: UpgradeRequest = serde_json::from_str(&text)
                .map_err(|error| format!("parse {}: {error}", path.display()))?;
            out.push((path, request));
        }
        Ok(out)
    }
}

pub fn validate_request(request: &UpgradeRequest) -> Result<(), String> {
    if request.schema != UPGRADE_SCHEMA {
        return Err(format!("unsupported upgrade schema: {}", request.schema));
    }
    if request.request_id.trim().is_empty() {
        return Err("request_id must not be empty".to_string());
    }
    if request.action != "upgrade" {
        return Err(format!("unsupported upgrade action: {}", request.action));
    }
    match &request.target {
        UpgradeTarget::Generation { id } if id.trim().is_empty() => {
            Err("target generation id must not be empty".to_string())
        }
        UpgradeTarget::Payload { path, strategy } => {
            if path.trim().is_empty() {
                return Err("payload path must not be empty".to_string());
            }
            if !matches!(strategy.as_str(), "store" | "copy") {
                return Err(format!(
                    "unsupported profile strategy: {strategy} (supported: store, copy)"
                ));
            }
            Ok(())
        }
        UpgradeTarget::Generation { .. } => Ok(()),
    }
}

pub fn is_expired(request: &UpgradeRequest, now_ms: i64) -> bool {
    request.expires_at_ms > 0 && now_ms > request.expires_at_ms
}

pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status,
        "stable" | "failed" | "rolled_back" | "rejected" | "expired" | "aborted"
    )
}

pub fn detect_restart_owner() -> String {
    if let Some(value) = std::env::var_os("ACB_RESTART_OWNER")
        .map(|value| value.to_string_lossy().trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return value;
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return "systemd".to_string();
    }
    "none".to_string()
}

fn write_json(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension(format!(
        "tmp.{}.{}",
        std::process::id(),
        crate::now_ms()
    ));
    {
        let mut file = fs::File::create(&tmp)
            .map_err(|error| format!("create {}: {error}", tmp.display()))?;
        file.write_all(bytes)
            .map_err(|error| format!("write {}: {error}", tmp.display()))?;
        file.sync_all()
            .map_err(|error| format!("sync {}: {error}", tmp.display()))?;
    }
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        format!("rename {} -> {}: {error}", tmp.display(), path.display())
    })?;
    if let Some(parent) = path.parent() {
        let _ = fs::File::open(parent).and_then(|dir| dir.sync_all());
    }
    Ok(())
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn process_alive(_pid: u32) -> bool {
    // Windows 没有稳定的无权限 pid 探活方式；capability 的进程生命周期由服务包装器保证。
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acb-upgrade-test-{name}-{}-{}",
            std::process::id(),
            crate::now_ms()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn capability(mailbox: &Mailbox, owner: &str) {
        mailbox
            .write_capabilities(&Capabilities {
                schema: UPGRADE_SCHEMA,
                restart_owner: owner.to_string(),
                launcher_pid: std::process::id(),
                profile_dir: "/tmp/profile".to_string(),
                state_dir: mailbox.state_dir().display().to_string(),
                strategies: vec!["store".to_string(), "copy".to_string()],
                updated_at_ms: crate::now_ms(),
            })
            .unwrap();
    }

    #[test]
    fn rejects_without_restart_owner() {
        let state = temp_state("no-owner");
        let mailbox = Mailbox::new(&state);
        capability(&mailbox, "none");
        let request = UpgradeRequest::new(
            "req-1",
            UpgradeTarget::Generation {
                id: "gen-1".to_string(),
            },
            crate::now_ms(),
        );
        match mailbox.submit(request).unwrap() {
            SubmitOutcome::Rejected { reason_code, .. } => {
                assert_eq!(reason_code, "no_restart_owner")
            }
            other => panic!("expected rejected, got {other:?}"),
        }
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn queues_and_deduplicates() {
        let state = temp_state("dedupe");
        let mailbox = Mailbox::new(&state);
        capability(&mailbox, "acbctl-run");
        let request = UpgradeRequest::new(
            "req-1",
            UpgradeTarget::Generation {
                id: "gen-1".to_string(),
            },
            crate::now_ms(),
        );
        assert!(matches!(
            mailbox.submit(request.clone()).unwrap(),
            SubmitOutcome::Queued { .. }
        ));
        assert!(matches!(
            mailbox.submit(request).unwrap(),
            SubmitOutcome::Duplicate { .. }
        ));
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn busy_does_not_queue() {
        let state = temp_state("busy");
        let mailbox = Mailbox::new(&state);
        capability(&mailbox, "systemd");
        let first = UpgradeRequest::new(
            "req-1",
            UpgradeTarget::Generation {
                id: "gen-1".to_string(),
            },
            crate::now_ms(),
        );
        mailbox.submit(first.clone()).unwrap();
        let claimed = mailbox.claim_next(crate::now_ms()).unwrap();
        assert!(claimed.is_some());
        let second = UpgradeRequest::new(
            "req-2",
            UpgradeTarget::Generation {
                id: "gen-2".to_string(),
            },
            crate::now_ms(),
        );
        assert!(matches!(
            mailbox.submit(second).unwrap(),
            SubmitOutcome::Busy { .. }
        ));
        let _ = fs::remove_dir_all(state);
    }

    #[test]
    fn expires_at_claim() {
        let state = temp_state("expired");
        let mailbox = Mailbox::new(&state);
        capability(&mailbox, "systemd");
        let mut request = UpgradeRequest::new(
            "req-1",
            UpgradeTarget::Generation {
                id: "gen-1".to_string(),
            },
            1,
        );
        request.expires_at_ms = 2;
        mailbox.submit(request).unwrap();
        assert!(mailbox.claim_next(10).unwrap().is_none());
        let result = mailbox.read_result("req-1").unwrap();
        assert_eq!(result.status, "expired");
        let _ = fs::remove_dir_all(state);
    }
}
