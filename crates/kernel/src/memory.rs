//! 记忆仓库：裸 git 仓库 + 每个「会话 × 记忆」一个 worktree。
//!
//! 桥对记忆只做两件事（ADR-0017 用户裁定）：
//! 1. **初始化基建**：裸仓库不存在就 `git init --bare`；某个「会话 × 记忆」的 worktree
//!    不存在就 `git worktree add` 从裸仓库 checkout 出来。
//! 2. **提供文件 + git 能力**：把文件操作（读 / 写 / 列目录 / 搜索）和 git 操作
//!    （status / log / diff / blame / add / commit / push / branch / checkout）透给 harness。
//!
//! 桥**绝不**替 harness 提交 / 推送 / 建分支 / 改内容——那些是 harness 通过上面的工具
//! 自己干的。桥唯一的写动作就是「第一次把 worktree 建出来」。
//!
//! 安全铁律：所有文件路径都必须落在**该 worktree 根内**，越界一律拒绝——记忆工具是
//! 「碰一个仓库」的能力，不是「碰宿主机」的能力。
//!
//! worktree 路径固定为 `worktrees_root/<记忆id>/<会话id>`。固定路径是「惰性初始化能否
//! 幂等」的前提：路径上已经是个有效 worktree 就复用，异常就报错，绝不 `rm -rf` 重建。

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_proto::{ErrorCode, ToolError};

/// 记忆仓库管理器：持有 worktree 根目录，负责把「记忆 id + 会话 id」解析成 worktree
/// 路径并保证它存在。
#[derive(Debug, Clone)]
pub struct MemoryRepo {
    worktrees_root: PathBuf,
}

impl MemoryRepo {
    pub fn new(worktrees_root: impl Into<PathBuf>) -> Self {
        Self {
            worktrees_root: worktrees_root.into(),
        }
    }

    /// 某个「会话 × 记忆」的 worktree 路径（确定性，不落地）。
    pub fn worktree_path(&self, memory: &str, session: &str) -> PathBuf {
        self.worktrees_root.join(memory).join(session)
    }

    /// 保证裸仓库存在：不存在就 `git init --bare`。
    ///
    /// 已经存在且是 git 仓库（无论 bare 与否）就复用；存在但不是 git 仓库就报错，
    /// 绝不覆盖用户已有的目录。
    pub fn ensure_bare_repo(&self, location: &str) -> Result<PathBuf, ToolError> {
        let repo_path = PathBuf::from(location);
        if repo_path.exists() {
            if git2::Repository::open(&repo_path).is_ok() {
                return Ok(repo_path);
            }
            return Err(ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.memory.repo_not_git", path = repo_path.display()),
            ));
        }
        git2::Repository::init_bare(&repo_path).map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "kernel.memory.init_bare_failed",
                    path = repo_path.display(),
                    error = error
                ),
            )
        })?;
        Ok(repo_path)
    }

    /// 惰性拿到某个「会话 × 记忆」的 worktree：存在且有效就复用，不存在就 checkout，
    /// 存在但异常就报错。只创建不重建。
    pub fn ensure_worktree(
        &self,
        location: &str,
        memory: &str,
        session: &str,
    ) -> Result<PathBuf, ToolError> {
        let bare = self.ensure_bare_repo(location)?;
        let worktree = self.worktree_path(memory, session);

        let repo = git2::Repository::open(&bare).map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "kernel.memory.open_bare_failed",
                    path = bare.display(),
                    error = error
                ),
            )
        })?;
        // 空裸仓库没有 HEAD，libgit2 无法从它创建 linked worktree。先种一个空提交，
        // 让主仓库有可 checkout 的基点；否则每个 worktree 都会退化成独立仓库。
        ensure_initial_commit(&repo, &bare)?;

        if worktree.exists() {
            let dotgit = worktree.join(".git");
            if dotgit.is_file() {
                if git2::Repository::open(&worktree).is_ok() {
                    return Ok(worktree);
                }
                return Err(ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!("kernel.memory.worktree_invalid", path = worktree.display()),
                ));
            }
            if dotgit.is_dir() && standalone_worktree_is_empty(&worktree) {
                // 旧实现留下的空独立仓库：改名留存，再重建为 linked worktree。
                let backup = orphan_backup_path(&worktree);
                fs::rename(&worktree, &backup).map_err(|error| {
                    ToolError::new(
                        ErrorCode::Internal,
                        acb_i18n::t!(
                            "kernel.memory.worktree_migrate_failed",
                            path = worktree.display(),
                            error = error
                        ),
                    )
                })?;
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "kernel.memory.worktree_migrated",
                        path = worktree.display(),
                        backup = backup.display()
                    )
                );
            } else {
                return Err(ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!(
                        "kernel.memory.worktree_not_linked",
                        path = worktree.display(),
                        bare = bare.display()
                    ),
                ));
            }
        }

        // 不存在（或刚移走中的空独立仓库）→ 从主仓库创建 linked worktree。
        // 分支名用会话 id，避免和主分支撞。
        let branch = format!("acb/{session}");
        if let Some(parent) = worktree.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!(
                        "kernel.memory.checkout_worktree_failed",
                        session = session,
                        error = error,
                        bare = bare.display()
                    ),
                )
            })?;
        }
        // libgit2 不会为 worktree 的 admin 目录自动建中间层；分支名 acb/<session>
        // 对应 <bare>/worktrees/acb/<session>，要先建 acb 这一层。
        if let Some((parent, _)) = branch.rsplit_once('/') {
            fs::create_dir_all(bare.join("worktrees").join(parent)).map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!(
                        "kernel.memory.checkout_worktree_failed",
                        session = session,
                        error = error,
                        bare = bare.display()
                    ),
                )
            })?;
        }
        repo.worktree(&branch, &worktree, None).map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "kernel.memory.checkout_worktree_failed",
                    session = session,
                    error = error,
                    bare = bare.display()
                ),
            )
        })?;

        Ok(worktree)
    }

    /// 把「worktree 内相对路径」解析成绝对路径，并确保它不越出 worktree。
    fn resolve_inside(worktree: &Path, rel: &str) -> Result<PathBuf, ToolError> {
        let base = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
        let joined = if rel.is_empty() || rel == "." {
            base.clone()
        } else {
            base.join(rel)
        };
        // 规范化后再比对前缀，挡住 `..`、绝对路径、符号链接越界。
        let canonical = std::fs::canonicalize(&joined).unwrap_or_else(|_| normalize(&joined));
        if !canonical.starts_with(&base) {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!("kernel.memory.path_outside", rel = rel),
            ));
        }
        Ok(canonical)
    }

    /// 定位 worktree：`session` 是当前会话 id，`memory` 是记忆 id，`session_memories`
    /// 是这个会话引用的记忆 id 列表，`location_of` 按记忆 id 找仓库路径。三者合起来
    /// 保证工具只能碰**本会话声明过**的记忆，且 worktree 保证存在（惰性初始化）。
    pub fn resolve_for(
        &self,
        session: &str,
        memory: &str,
        session_memories: &[String],
        location_of: &dyn Fn(&str) -> Option<String>,
    ) -> Result<PathBuf, ToolError> {
        if !session_memories.iter().any(|m| m == memory) {
            return Err(ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "kernel.memory.memory_not_referenced",
                    memory = memory,
                    available = if session_memories.is_empty() {
                        acb_i18n::t!("kernel.memory.none")
                    } else {
                        session_memories.join(", ")
                    }
                ),
            ));
        }
        let location = location_of(memory).ok_or_else(|| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.memory.no_repo_path", memory = memory),
            )
        })?;
        self.ensure_worktree(&location, memory, session)
    }
}

/// 空裸仓库没有 HEAD；先种一个空提交，linked worktree 才有可 checkout 的基点。
fn ensure_initial_commit(repo: &git2::Repository, bare: &Path) -> Result<(), ToolError> {
    if repo.head().is_ok() {
        return Ok(());
    }
    let result = (|| -> Result<(), git2::Error> {
        let signature = git2::Signature::now("ACB", "acb@localhost")?;
        let tree_id = {
            let builder = repo.treebuilder(None)?;
            builder.write()?
        };
        let tree = repo.find_tree(tree_id)?;
        repo.commit(
            Some("refs/heads/main"),
            &signature,
            &signature,
            "memory: initial commit",
            &tree,
            &[],
        )?;
        repo.set_head("refs/heads/main")?;
        Ok(())
    })();
    result.map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!(
                "kernel.memory.init_initial_commit_failed",
                bare = bare.display(),
                error = error
            ),
        )
    })
}

/// 旧实现用 `git init` 建出来的独立仓库：没有提交、目录里除 `.git` 外没有内容时，
/// 可以安全改名留存并重建为 linked worktree。
fn standalone_worktree_is_empty(path: &Path) -> bool {
    let Ok(repo) = git2::Repository::open(path) else {
        return false;
    };
    if repo.head().is_ok() {
        return false;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries
        .filter_map(Result::ok)
        .all(|entry| entry.file_name() == ".git")
}

fn orphan_backup_path(path: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let mut backup = path.to_path_buf();
    let name = format!(
        "{}.orphan-{stamp}",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
    );
    backup.set_file_name(name);
    backup
}

/// 规范化路径：去掉 `.` 和 `..`（不要求真实存在，供越界判断兜底用）。
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 文件操作
// ---------------------------------------------------------------------------

/// 读一个文件，返回其内容。限 worktree 内。
pub fn read_file(worktree: &Path, rel: &str) -> Result<String, ToolError> {
    let path = MemoryRepo::resolve_inside(worktree, rel)?;
    std::fs::read_to_string(&path).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!(
                "kernel.memory.read_file_failed",
                path = path.display(),
                error = error
            ),
        )
    })
}

/// 写一个文件（覆盖）。父目录不存在就建。限 worktree 内。
pub fn write_file(worktree: &Path, rel: &str, content: &str) -> Result<(), ToolError> {
    let path = MemoryRepo::resolve_inside(worktree, rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!(
                    "kernel.memory.create_dir_failed",
                    path = parent.display(),
                    error = error
                ),
            )
        })?;
    }
    std::fs::write(&path, content).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!(
                "kernel.memory.write_file_failed",
                path = path.display(),
                error = error
            ),
        )
    })?;
    Ok(())
}

/// 列出一个目录下的条目（相对路径）。限 worktree 内。
pub fn list_dir(worktree: &Path, rel: &str) -> Result<Vec<String>, ToolError> {
    let path = MemoryRepo::resolve_inside(worktree, rel)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&path).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!(
                "kernel.memory.list_dir_failed",
                path = path.display(),
                error = error
            ),
        )
    })? {
        let entry = entry.map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.memory.read_dir_entry_failed", error = error),
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    entries.sort();
    Ok(entries)
}

/// 在 worktree 内递归搜索文本（grep）。返回命中「文件:行号:内容」。
pub fn search(worktree: &Path, rel: &str, needle: &str) -> Result<Vec<String>, ToolError> {
    let base = MemoryRepo::resolve_inside(worktree, rel)?;
    let mut hits = Vec::new();
    walk(&base, &mut |path| {
        if let Ok(text) = std::fs::read_to_string(path) {
            for (i, line) in text.lines().enumerate() {
                if line.contains(needle) {
                    let rel = path
                        .strip_prefix(worktree)
                        .unwrap_or(path)
                        .display()
                        .to_string();
                    hits.push(format!("{rel}:{}:{line}", i + 1));
                }
            }
        }
    })?;
    hits.sort();
    Ok(hits)
}

fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) -> Result<(), ToolError> {
    if dir.is_file() {
        visit(dir);
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // 跳过 .git 目录：搜索记忆内容不该翻版本库内部。
        if entry.file_name() == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(&path, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// git 操作
// ---------------------------------------------------------------------------

/// `git status`：返回简明的变更概览（每个条目一行，前缀是状态码）。
pub fn git_status(worktree: &Path) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let mut out = String::new();
    let statuses = repo.statuses(None).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_status_failed", error = error),
        )
    })?;
    for entry in statuses.iter() {
        let code = status_code(entry.status());
        let path = entry.path().unwrap_or("?");
        out.push_str(&format!("{code} {path}\n"));
    }
    if out.is_empty() {
        out = "working tree clean".to_owned();
    }
    Ok(out)
}

/// `git log`：最近 `n` 条提交，一行一条（短 hash + 提交信息）。
pub fn git_log(worktree: &Path, n: usize) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let mut revwalk = repo.revwalk().map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_log_failed", error = error),
        )
    })?;
    revwalk.push_head().map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_log_failed", error = error),
        )
    })?;
    let mut out = String::new();
    for oid in revwalk.flatten().take(n) {
        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let short = &oid.to_string()[..8.min(oid.to_string().len())];
        let summary = commit.summary().unwrap_or("");
        out.push_str(&format!("{short} {summary}\n"));
    }
    Ok(out)
}

/// `git diff`：工作区相对最近提交的改动。
pub fn git_diff(worktree: &Path) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let diff = repo.diff_index_to_workdir(None, None).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_diff_failed", error = error),
        )
    })?;
    let mut out = String::new();
    let opts = git2::DiffFormat::Patch;
    diff.print(opts, |_delta, _hunk, line| {
        let content = std::str::from_utf8(line.content()).unwrap_or("");
        let _ = out.push_str(content);
        true
    })
    .map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.render_diff_failed", error = error),
        )
    })?;
    Ok(out)
}

/// `git add`：暂存指定路径（相对 worktree；空串暂存全部）。
pub fn git_add(worktree: &Path, rel: &str) -> Result<(), ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let mut index = repo.index().map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_add_failed", error = error),
        )
    })?;
    if rel.is_empty() || rel == "." {
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .map_err(|error| {
                ToolError::new(
                    ErrorCode::Internal,
                    acb_i18n::t!("kernel.memory.git_add_failed", error = error),
                )
            })?;
    } else {
        index.add_path(Path::new(rel)).map_err(|error| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "kernel.memory.git_add_path_failed",
                    path = rel,
                    error = error
                ),
            )
        })?;
    }
    index.write().map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_add_persist_failed", error = error),
        )
    })?;
    Ok(())
}

/// `git commit`：提交已暂存的内容。`message` 是提交信息。
pub fn git_commit(worktree: &Path, message: &str) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let mut index = repo.index().map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_commit_failed", error = error),
        )
    })?;
    let tree_id = index.write_tree().map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_commit_failed", error = error),
        )
    })?;
    let tree = repo.find_tree(tree_id).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_commit_failed", error = error),
        )
    })?;
    let head = repo.head().ok();
    let parent = head.and_then(|h| h.peel_to_commit().ok());
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    // 优先用仓库/全局配置的身份；没配就用桥的固定身份兜底——记忆提交不该因为宿主机
    // 没配 user.name/user.email 就做不成。
    let sig = repo.signature().or_else(|_| {
        git2::Signature::now("acb-bridge", "acb-bridge@localhost").map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.memory.git_identity_failed", error = error),
            )
        })
    })?;
    let _oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(|error| {
            ToolError::new(
                ErrorCode::Internal,
                acb_i18n::t!("kernel.memory.git_commit_failed", error = error),
            )
        })?;
    Ok("已提交".to_owned())
}

/// `git push`：推送到默认远端。
pub fn git_push(worktree: &Path) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let mut remote = repo.find_remote("origin").map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!("kernel.memory.no_origin", error = error),
        )
    })?;
    // 推送当前 worktree 所在的分支，而不是写死的 acb/HEAD。
    let head = repo.head().map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!(
                "kernel.memory.git_push_detached",
                path = worktree.display(),
                error = error
            ),
        )
    })?;
    let branch = head
        .shorthand()
        .filter(|name| *name != "HEAD")
        .ok_or_else(|| {
            ToolError::new(
                ErrorCode::BadRequest,
                acb_i18n::t!(
                    "kernel.memory.git_push_detached",
                    path = worktree.display(),
                    error = "HEAD is detached"
                ),
            )
        })?;
    let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
    remote.push(&[refspec.as_str()], None).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_push_failed", error = error),
        )
    })?;
    Ok("已推送".to_owned())
}

/// `git branch`：列出本地分支。
pub fn git_branch(worktree: &Path) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let branches = repo.branches(None).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.git_branch_failed", error = error),
        )
    })?;
    let mut out = String::new();
    for branch in branches.flatten() {
        let (branch, _) = branch;
        let name = branch.name().ok().flatten().unwrap_or("?");
        out.push_str(&format!("{name}\n"));
    }
    Ok(out)
}

/// `git blame`：查某一行/文件是谁改的。
pub fn git_blame(worktree: &Path, rel: &str) -> Result<String, ToolError> {
    let repo = git2::Repository::open(worktree).map_err(|error| {
        ToolError::new(
            ErrorCode::Internal,
            acb_i18n::t!("kernel.memory.open_worktree_failed", error = error),
        )
    })?;
    let blame = repo.blame_file(Path::new(rel), None).map_err(|error| {
        ToolError::new(
            ErrorCode::BadRequest,
            acb_i18n::t!("kernel.memory.git_blame_failed", path = rel, error = error),
        )
    })?;
    let mut out = String::new();
    for hunk in blame.iter() {
        let line_no = hunk.final_start_line();
        let sig = hunk.final_signature();
        let who = sig.name().unwrap_or("?");
        out.push_str(&format!("{line_no}\t{who}\n"));
    }
    Ok(out)
}

fn status_code(status: git2::Status) -> &'static str {
    if status.is_index_new() || status.is_wt_new() {
        return "A";
    }
    if status.is_index_modified() || status.is_wt_modified() {
        return "M";
    }
    if status.is_index_deleted() || status.is_wt_deleted() {
        return "D";
    }
    if status.is_wt_renamed() || status.is_index_renamed() {
        return "R";
    }
    "?"
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个测试一个独立临时目录，互不干扰、测完自动清。
    fn tmp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acb-memory-{}-{}",
            std::process::id(),
            now_millis()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn now_millis() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时钟")
            .as_nanos()
    }

    #[test]
    fn ensure_bare_repo_creates_and_is_idempotent() {
        let root = tmp();
        let repo_path = root.join("main.git");

        // 第一次：建裸仓库。
        let got = MemoryRepo::new(root.join("wt")).ensure_bare_repo(repo_path.to_str().unwrap());
        assert!(got.is_ok());
        assert!(repo_path.exists());

        // 第二次：已存在且是 git 仓库，直接复用。
        let again = MemoryRepo::new(root.join("wt")).ensure_bare_repo(repo_path.to_str().unwrap());
        assert!(again.is_ok());
    }

    #[test]
    fn ensure_bare_repo_refuses_non_git_directory() {
        let root = tmp();
        let not_a_repo = root.join("just-a-dir");
        std::fs::create_dir_all(&not_a_repo).expect("建目录");

        let err = MemoryRepo::new(root.join("wt"))
            .ensure_bare_repo(not_a_repo.to_str().unwrap())
            .expect_err("已存在的非 git 目录必须拒");
        assert_eq!(err.code, ErrorCode::Internal);
    }

    #[test]
    fn ensure_worktree_is_idempotent() {
        let root = tmp();
        let bare = root.join("main.git");
        let repo = MemoryRepo::new(root.join("wt"));

        let first = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("首次 checkout");
        assert!(first.exists());
        assert!(
            first.join(".git").is_file(),
            "worktree 必须是主仓库的 linked worktree，而不是独立仓库"
        );
        let worktree_repo = git2::Repository::open(&first).expect("能打开 worktree");
        assert_eq!(
            worktree_repo.head().expect("有 HEAD").shorthand(),
            Some("acb/s1"),
            "worktree 分支应当按会话命名"
        );

        // 第二次：路径已存在且是有效 worktree，直接复用，不报错、不重建。
        let second = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("幂等复用");
        assert_eq!(first, second);
    }

    #[test]
    fn empty_standalone_worktree_is_migrated_to_linked() {
        let root = tmp();
        let bare = root.join("main.git");
        let repo = MemoryRepo::new(root.join("wt"));
        let wt = repo.worktree_path("m1", "s1");
        std::fs::create_dir_all(&wt).expect("建 worktree 目录");
        git2::Repository::init(&wt).expect("旧实现留下的独立仓库");

        let linked = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("空独立仓库应当能迁移");
        assert!(
            linked.join(".git").is_file(),
            "迁移后必须是 linked worktree"
        );

        let parent = wt.parent().expect("有父目录");
        let backups: Vec<_> = std::fs::read_dir(parent)
            .expect("读目录")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("s1.orphan-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "旧独立仓库应当改名留存，不是删除");
    }

    #[test]
    fn git_push_pushes_the_current_worktree_branch() {
        let root = tmp();
        let bare = root.join("main.git");
        let remote = root.join("remote.git");
        git2::Repository::init_bare(&remote).expect("建远端裸仓库");
        let repo = MemoryRepo::new(root.join("wt"));
        let wt = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("checkout");

        write_file(&wt, "shared.md", "第一版").expect("写");
        git_add(&wt, "").expect("暂存");
        git_commit(&wt, "第一版").expect("提交");

        let worktree_repo = git2::Repository::open(&wt).expect("打开 worktree");
        worktree_repo
            .remote("origin", remote.to_str().unwrap())
            .expect("配置 origin");
        git_push(&wt).expect("推送到 origin");

        let remote_repo = git2::Repository::open_bare(&remote).expect("打开远端");
        assert!(
            remote_repo.find_reference("refs/heads/acb/s1").is_ok(),
            "推送的应当是当前会话分支 acb/s1"
        );
    }

    #[test]
    fn worktree_paths_are_namespaced_by_memory_and_session() {
        let repo = MemoryRepo::new("/root/wt");
        assert_eq!(
            repo.worktree_path("m1", "s1"),
            PathBuf::from("/root/wt/m1/s1")
        );
        // 不同记忆、不同会话，路径各不同——这是「多个会话不互踩」的前提。
        assert_ne!(
            repo.worktree_path("m1", "s1"),
            repo.worktree_path("m1", "s2")
        );
        assert_ne!(
            repo.worktree_path("m1", "s1"),
            repo.worktree_path("m2", "s1")
        );
    }

    #[test]
    fn resolve_inside_rejects_path_escape() {
        let root = tmp();
        let bare = root.join("main.git");
        let repo = MemoryRepo::new(root.join("wt"));
        let wt = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("checkout");

        // 越界：`..` 逃出 worktree。
        let err = MemoryRepo::resolve_inside(&wt, "../secret.txt").expect_err("必须拒");
        assert_eq!(err.code, ErrorCode::BadRequest);

        // 绝对路径也不行。
        let err2 = MemoryRepo::resolve_inside(&wt, "/etc/passwd").expect_err("必须拒");
        assert_eq!(err2.code, ErrorCode::BadRequest);
    }

    #[test]
    fn write_then_read_round_trips() {
        let root = tmp();
        let bare = root.join("main.git");
        let repo = MemoryRepo::new(root.join("wt"));
        let wt = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("checkout");

        write_file(&wt, "notes/foo.md", "你好").expect("写文件");
        assert_eq!(read_file(&wt, "notes/foo.md").expect("读文件"), "你好");
        // 父目录自动建好了。
        assert!(wt.join("notes").is_dir());
    }

    #[test]
    fn git_add_commit_log_work_on_the_worktree() {
        let root = tmp();
        let bare = root.join("main.git");
        let repo = MemoryRepo::new(root.join("wt"));
        let wt = repo
            .ensure_worktree(bare.to_str().unwrap(), "m1", "s1")
            .expect("checkout");

        // worktree 已从主仓库 checkout 出 acb/s1 分支；直接写文件 + add + commit 验证链路。
        write_file(&wt, "shared.md", "第一版").expect("写");
        git_add(&wt, "").expect("暂存全部");
        git_commit(&wt, "初始").expect("提交");
        let log = git_log(&wt, 10).expect("看日志");
        assert!(log.contains("初始"), "日志里要有提交信息：{log}");
    }
}
