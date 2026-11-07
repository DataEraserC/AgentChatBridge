//! `current.json` 的单写者与 `profile/current` 投影。
//!
//! `current.json` 是执行真值（launcher 只按它选版本），`profile/current` 只是投影。
//! 两者在不同目录，无法用一次 rename 提交，因此：
//! 1. 所有写入都先拿 `upgrade/.lock` 的文件锁，保证只有一个写者；
//! 2. 提交前写 `upgrade/journal.json`（旧原文 + 新原文 + 投影目标）；
//! 3. `current.json` 用临时文件 + fsync + rename 提交，rename 是提交点；
//! 4. 再原子替换投影软链；投影失败时把 `current.json` 补偿回旧原文；
//! 5. 成功后才删除 journal。崩溃留下的 journal 由 [`ActivationWriter::reconcile`]
//!    在下次启动/切换时对账，不猜测。

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::CommandSpec;

const UPGRADE_DIR: &str = "upgrade";
const JOURNAL_NAME: &str = "journal.json";
const LOCK_NAME: &str = ".lock";
const JOURNAL_SCHEMA: u32 = 1;

/// `profile/current` 投影：`link` 是软链，`target` 是相对 `profile` 的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub link: PathBuf,
    pub target: PathBuf,
}

impl Projection {
    /// 从 generation 引用推导投影：payload 必须落在
    /// `<profile>/generations/<id>/root`。
    pub fn discover(generation: &crate::GenerationRef) -> Option<Self> {
        let payload = Path::new(&generation.path);
        if !payload
            .file_name()
            .is_some_and(|name| name.to_string_lossy() == "root")
        {
            return None;
        }
        let generations = payload.parent()?.parent()?;
        if !generations
            .file_name()
            .is_some_and(|name| name.to_string_lossy() == "generations")
        {
            return None;
        }
        let profile = generations.parent()?;
        Some(Self {
            link: profile.join("current"),
            target: PathBuf::from("generations").join(&generation.id).join("root"),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectionRecord {
    link: String,
    target: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct CurrentRecord {
    command: CommandSpec,
    #[serde(default)]
    updated_at_ms: i64,
    #[serde(default)]
    updated_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    projection: Option<ProjectionRecord>,
}

impl CurrentRecord {
    fn projection(&self) -> Option<Projection> {
        self.projection.as_ref().map(|record| Projection {
            link: PathBuf::from(&record.link),
            target: PathBuf::from(&record.target),
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Journal {
    schema: u32,
    from_raw: Option<String>,
    to_raw: String,
    projection: Option<ProjectionRecord>,
    previous_projection_target: Option<String>,
    started_at_ms: i64,
}

/// 一次对账的结果，供调用方区分“无事可做”和“补完了半状态”。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// 没有 journal，投影一致或没有投影记录。
    Nothing,
    /// 没有 journal，但投影与 `current.json` 不一致，已按真值修复。
    ProjectionRepaired,
    /// journal 存在且 `current.json` 已是新值：补完投影，删除 journal。
    Committed,
    /// journal 存在且 `current.json` 还是旧值：恢复旧投影，删除 journal。
    Aborted,
}

pub fn current_path(state_dir: &Path) -> PathBuf {
    state_dir.join("current.json")
}

fn journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join(UPGRADE_DIR).join(JOURNAL_NAME)
}

/// 读 launcher 应该启动的版本指针。
///
/// 文件不存在是合法的首次安装（`Ok(None)`）；读失败或 JSON 损坏返回 `Err`。
/// 调用方不允许用配置版本静默替代损坏的指针。
pub fn load_current(state_dir: &Path) -> Result<Option<CommandSpec>, String> {
    let path = current_path(state_dir);
    let Some(raw) = read_optional(&path)? else {
        return Ok(None);
    };
    let record: CurrentRecord = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "{} is corrupt: {error}; refusing to fall back to the configured version \
             (inspect and delete the file only after deciding which version should run)",
            path.display()
        )
    })?;
    Ok(Some(record.command))
}

/// 单写者锁 + journal + 原子提交。拿到锁的进程才允许改 `current.json`。
pub struct ActivationWriter {
    state_dir: PathBuf,
    _lock: File,
}

impl ActivationWriter {
    /// 打开并独占 `state_dir/upgrade/.lock`。锁在 writer 存活期间一直持有。
    pub fn acquire(state_dir: &Path) -> Result<Self, String> {
        let upgrade = state_dir.join(UPGRADE_DIR);
        fs::create_dir_all(&upgrade)
            .map_err(|error| format!("create {}: {error}", upgrade.display()))?;
        let lock_path = upgrade.join(LOCK_NAME);
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("open {}: {error}", lock_path.display()))?;
        lock_exclusive(&lock, &lock_path)?;
        Ok(Self {
            state_dir: state_dir.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// 对账：处理崩溃留下的 journal，或修复与 `current.json` 不一致的投影。
    pub fn reconcile(&self) -> Result<ReconcileOutcome, String> {
        let journal_file = journal_path(&self.state_dir);
        let journal_raw = read_optional(&journal_file)?;
        let current_raw = read_optional(&current_path(&self.state_dir))?;

        let Some(journal_raw) = journal_raw else {
            return self.repair_projection(current_raw);
        };
        let journal: Journal = serde_json::from_str(&journal_raw).map_err(|error| {
            format!(
                "{} is corrupt: {error}; refusing to guess which version should run",
                journal_file.display()
            )
        })?;
        if journal.schema != JOURNAL_SCHEMA {
            return Err(format!(
                "{}: unsupported journal schema {}",
                journal_file.display(),
                journal.schema
            ));
        }

        if current_raw.as_deref() == Some(journal.to_raw.as_str()) {
            if let Some(projection) = projection_from_record(&journal.projection) {
                write_projection(&projection)?;
            }
            remove_journal(&journal_file)?;
            return Ok(ReconcileOutcome::Committed);
        }
        if current_raw.as_deref() == journal.from_raw.as_deref() {
            if let Some(projection) = projection_from_record(&journal.projection) {
                restore_projection(
                    &projection,
                    journal.previous_projection_target.as_deref(),
                )?;
            }
            remove_journal(&journal_file)?;
            return Ok(ReconcileOutcome::Aborted);
        }
        Err(format!(
            "{} does not match {}; refusing to guess (restore one side manually)",
            journal_file.display(),
            current_path(&self.state_dir).display()
        ))
    }

    /// 提交新指针：journal → `current.json`（fsync + rename）→ 投影。
    ///
    /// `projection` 为 `None` 时沿用旧 `current.json` 里记录的投影软链，并用
    /// 新 command 的 generation id 推导目标；没有旧记录则不动投影（legacy 行为）。
    pub fn commit(
        &self,
        command: &CommandSpec,
        projection: Option<Projection>,
        updated_by: &str,
        now: i64,
    ) -> Result<(), String> {
        self.reconcile()?;

        let current = current_path(&self.state_dir);
        let from_raw = read_optional(&current)?;
        let previous = from_raw
            .as_deref()
            .and_then(|raw| serde_json::from_str::<CurrentRecord>(raw).ok());
        let projection = projection.or_else(|| {
            let link = previous.as_ref()?.projection()?.link;
            Some(Projection {
                link,
                target: PathBuf::from("generations")
                    .join(&command.generation.as_ref()?.id)
                    .join("root"),
            })
        });
        let previous_projection_target = projection
            .as_ref()
            .and_then(|projection| fs::read_link(&projection.link).ok())
            .map(|target| target.to_string_lossy().into_owned());

        let record = CurrentRecord {
            command: command.clone(),
            updated_at_ms: now,
            updated_by: updated_by.to_owned(),
            projection: projection.as_ref().map(|projection| ProjectionRecord {
                link: projection.link.to_string_lossy().into_owned(),
                target: projection.target.to_string_lossy().into_owned(),
            }),
        };
        let to_raw = serde_json::to_string_pretty(&record)
            .map_err(|error| format!("serialize current.json: {error}"))?
            + "\n";

        let journal = Journal {
            schema: JOURNAL_SCHEMA,
            from_raw: from_raw.clone(),
            to_raw: to_raw.clone(),
            projection: record.projection.clone(),
            previous_projection_target: previous_projection_target.clone(),
            started_at_ms: now,
        };
        self.write_journal(&journal)?;

        if let Err(error) = write_atomic(&current, &to_raw) {
            let _ = fs::remove_file(journal_path(&self.state_dir));
            return Err(error);
        }

        if let Some(projection) = &projection {
            if let Err(error) = write_projection(projection) {
                // 投影不是真值，但也不能让 current.json 停在“新值 + 旧投影”。
                let restored_projection = restore_projection(
                    projection,
                    previous_projection_target.as_deref(),
                );
                let restored_current = restore_current(&current, &from_raw);
                let _ = fs::remove_file(journal_path(&self.state_dir));
                return Err(match (restored_current, restored_projection) {
                    (Ok(()), Ok(())) => format!(
                        "projection update failed: {error}; current.json and profile/current restored"
                    ),
                    (current_result, projection_result) => format!(
                        "projection update failed: {error}; restore current.json: {}, restore projection: {}",
                        describe(current_result),
                        describe(projection_result)
                    ),
                });
            }
        }

        remove_journal(&journal_path(&self.state_dir))
    }

    fn write_journal(&self, journal: &Journal) -> Result<(), String> {
        let text = serde_json::to_string_pretty(journal)
            .map_err(|error| format!("serialize journal: {error}"))?
            + "\n";
        let path = journal_path(&self.state_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        write_atomic(&path, &text)
    }

    fn repair_projection(
        &self,
        current_raw: Option<String>,
    ) -> Result<ReconcileOutcome, String> {
        let Some(raw) = current_raw else {
            return Ok(ReconcileOutcome::Nothing);
        };
        let record: CurrentRecord = match serde_json::from_str(&raw) {
            Ok(record) => record,
            // 损坏的指针没有真值可依，留给 load_current 报错。
            Err(_) => return Ok(ReconcileOutcome::Nothing),
        };
        let Some(projection) = record.projection() else {
            return Ok(ReconcileOutcome::Nothing);
        };
        match fs::read_link(&projection.link) {
            Ok(target) if target == projection.target => Ok(ReconcileOutcome::Nothing),
            Ok(_) => {
                write_projection(&projection)?;
                Ok(ReconcileOutcome::ProjectionRepaired)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_projection(&projection)?;
                Ok(ReconcileOutcome::ProjectionRepaired)
            }
            Err(error) => Err(format!(
                "read {}: {error}",
                projection.link.display()
            )),
        }
    }
}

fn projection_from_record(record: &Option<ProjectionRecord>) -> Option<Projection> {
    record.as_ref().map(|record| Projection {
        link: PathBuf::from(&record.link),
        target: PathBuf::from(&record.target),
    })
}

fn describe(result: Result<(), String>) -> String {
    match result {
        Ok(()) => "ok".to_owned(),
        Err(error) => error,
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// 同目录临时文件 + fsync + rename，再 fsync 目录。
fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("no parent directory for {}", path.display()))?;
    let temp = temp_sibling(path, "tmp");
    let result = (|| -> std::io::Result<()> {
        let mut file = File::create(&temp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temp);
        return Err(format!("write {}: {error}", path.display()));
    }
    fsync_dir(parent)
}

fn restore_current(path: &Path, from_raw: &Option<String>) -> Result<(), String> {
    match from_raw {
        Some(raw) => write_atomic(path, raw),
        None => remove_projection(path),
    }
}

fn write_projection(projection: &Projection) -> Result<(), String> {
    #[cfg(unix)]
    {
        let parent = projection
            .link
            .parent()
            .ok_or_else(|| format!("no parent directory for {}", projection.link.display()))?;
        let temp = temp_sibling(&projection.link, "current");
        let _ = fs::remove_file(&temp);
        std::os::unix::fs::symlink(&projection.target, &temp).map_err(|error| {
            format!(
                "create temporary symlink {} -> {}: {error}",
                temp.display(),
                projection.target.display()
            )
        })?;
        fs::rename(&temp, &projection.link).map_err(|error| {
            let _ = fs::remove_file(&temp);
            format!(
                "replace {} -> {}: {error}",
                projection.link.display(),
                projection.target.display()
            )
        })?;
        fsync_dir(parent)
    }
    #[cfg(not(unix))]
    {
        let _ = projection;
        Ok(())
    }
}

fn restore_projection(projection: &Projection, target: Option<&str>) -> Result<(), String> {
    match target {
        Some(target) => write_projection(&Projection {
            link: projection.link.clone(),
            target: PathBuf::from(target),
        }),
        None => remove_projection(&projection.link),
    }
}

fn remove_projection(link: &Path) -> Result<(), String> {
    match fs::remove_file(link) {
        Ok(()) => {
            if let Some(parent) = link.parent() {
                fsync_dir(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", link.display())),
    }
}

fn remove_journal(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                fsync_dir(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {}: {error}", path.display())),
    }
}

fn temp_sibling(path: &Path, tag: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{tag}-{}-{nanos}", std::process::id()))
}

fn fsync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let file = File::open(dir).map_err(|error| format!("open {}: {error}", dir.display()))?;
        file.sync_all()
            .map_err(|error| format!("fsync {}: {error}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn lock_exclusive(file: &File, path: &Path) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: flock 只操作本进程刚打开的文件描述符，失败时返回 -1。
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "lock {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &File, _path: &Path) -> Result<(), String> {
    Ok(())
}
