//! store generation 的 GC pin。
//!
//! `store` 策略的 generation 只保存指向 `/nix/store` 的软链。nix GC 不知道
//! `current.json` / `lkg.json` 引用了哪条 store path，所以要登记一个 nix 真正扫描的
//! GC 根。普通用户可用的方式是 `nix-store --add-root <pin> --indirect -r <store>`：
//! daemon 会在 `/nix/var/nix/gcroots/auto/` 登记根，pin 只是该根指向的软链。
//!
//! 建根后必须回查 `nix-store --query --roots <store>`，只有输出里确实出现该 pin 才
//! 算成功。没有 nix、daemon 不可用、pin 目录不可写或回查失败时，一律报告
//! `Unprotected`，并说明本地 pin 不会被 nix GC 扫描、不提供保护。把软链放进
//! `~/.local/state/nix/gcroots` 在多用户 Nix 下不会被扫描，不能当作根。

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// pin 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinOutcome {
    /// 建根成功，并已回查确认该 store path 被这个 pin 引用；nix GC 不会回收它。
    Gcroot(PathBuf),
    /// 没有建立可用的 GC 根，不提供保护。`path` 是仍可保留的本地 pin（如果有）。
    Unprotected {
        path: Option<PathBuf>,
        reason: PinFallback,
    },
    /// payload 不在 `/nix/store`，不需要 pin。
    Skipped,
}

/// 未能提供 GC 保护的具体原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinFallback {
    /// 环境里没有 `nix-store`（非 nix 环境，或不在 PATH）。
    NoNix,
    /// `nix-store --add-root` 执行失败（daemon 不可用、store path 无效等）。
    AddRootFailed(String),
    /// add-root 返回成功，但回查不到这个根（根未被扫描）。
    VerificationFailed(String),
    /// pin 目录不可写，连本地软链都没建立。
    DirUnwritable(String),
}

impl std::fmt::Display for PinFallback {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoNix => write!(
                formatter,
                "`nix-store` unavailable (not a nix environment or not on PATH); \
                 local pin is not scanned by nix GC and provides no protection"
            ),
            Self::AddRootFailed(error) => write!(
                formatter,
                "`nix-store --add-root` failed: {error}; \
                 local pin is not scanned by nix GC and provides no protection"
            ),
            Self::VerificationFailed(error) => write!(
                formatter,
                "created root but verification failed: {error}; \
                 the root is not confirmed to be scanned and provides no protection"
            ),
            Self::DirUnwritable(error) => write!(
                formatter,
                "pin directory is not writable: {error}; \
                 no pin was created and the payload is unprotected"
            ),
        }
    }
}

pub(crate) fn pin_store_path(
    root: &Path,
    id: &str,
    store_path: &Path,
) -> Result<PinOutcome, String> {
    if !store_path.starts_with("/nix/store") {
        return Ok(PinOutcome::Skipped);
    }
    pin_with(
        store_path,
        &local_pin_dir(root).join(pin_name(id)),
        &SystemNixStore::from_env(),
    )
}

/// 删除该 generation 的所有 pin（新 pin + 旧实现可能留下的软链）。不存在的 pin 忽略。
pub(crate) fn unpin(root: &Path, id: &str) -> Vec<PathBuf> {
    unpin_with_dirs(&pin_name(id), &legacy_gcroot_dirs(), &local_pin_dir(root))
}

fn pin_name(id: &str) -> String {
    format!("acb-{id}")
}

fn local_pin_dir(root: &Path) -> PathBuf {
    root.parent()
        .map(|parent| parent.join("state").join("pins"))
        .unwrap_or_else(|| PathBuf::from("state/pins"))
}

/// 旧实现直接放普通软链的目录。它们不再被当作 GC 根，只在 unpin 时清理。
fn legacy_gcroot_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(value) = std::env::var_os("ACB_GCROOT_DIR").filter(|value| !value.is_empty()) {
        dirs.push(PathBuf::from(value));
    }
    let state = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".local/state"))
        });
    if let Some(state) = state {
        dirs.push(state.join("nix/gcroots"));
    }
    if let Some(user) = std::env::var_os("USER").filter(|value| !value.is_empty()) {
        dirs.push(
            PathBuf::from("/nix/var/nix/gcroots")
                .join("per-user")
                .join(user),
        );
    }
    dirs.push(PathBuf::from("/nix/var/nix/gcroots"));
    dirs.dedup();
    dirs
}

fn pin_with(store_path: &Path, link: &Path, nix: &dyn NixStore) -> Result<PinOutcome, String> {
    let Some(pin_dir) = link.parent() else {
        return Err(format!("pin path has no parent: {}", link.display()));
    };
    if let Err(error) = fs::create_dir_all(pin_dir) {
        return Ok(PinOutcome::Unprotected {
            path: None,
            reason: PinFallback::DirUnwritable(format!("create {}: {error}", pin_dir.display())),
        });
    }
    // 同名旧 pin 可能指向上一代；先清掉，避免 add-root 沿用错误目标。
    let _ = fs::remove_file(link);

    match nix.add_root(link, store_path) {
        Ok(()) => match verify_root(nix, link, store_path) {
            Ok(true) => Ok(PinOutcome::Gcroot(link.to_path_buf())),
            Ok(false) => Ok(PinOutcome::Unprotected {
                path: Some(link.to_path_buf()),
                reason: PinFallback::VerificationFailed(format!(
                    "`nix-store --query --roots {}` did not list {}",
                    store_path.display(),
                    link.display()
                )),
            }),
            Err(error) => Ok(PinOutcome::Unprotected {
                path: Some(link.to_path_buf()),
                reason: PinFallback::VerificationFailed(error),
            }),
        },
        Err(AddRootError::NoNix) => local_fallback(store_path, link, PinFallback::NoNix),
        Err(AddRootError::Failed(error)) => {
            local_fallback(store_path, link, PinFallback::AddRootFailed(error))
        }
    }
}

fn local_fallback(
    store_path: &Path,
    link: &Path,
    reason: PinFallback,
) -> Result<PinOutcome, String> {
    let _ = fs::remove_file(link);
    match symlink(store_path, link) {
        Ok(()) => Ok(PinOutcome::Unprotected {
            path: Some(link.to_path_buf()),
            reason,
        }),
        Err(error) => Ok(PinOutcome::Unprotected {
            path: None,
            reason: PinFallback::DirUnwritable(error),
        }),
    }
}

/// 回查该 store path 是否确实被 `link` 这个根引用。
fn verify_root(nix: &dyn NixStore, link: &Path, store_path: &Path) -> Result<bool, String> {
    let roots = nix.roots(store_path)?;
    let expected = lexical_absolute(link);
    Ok(roots.iter().any(|root| lexical_absolute(root) == expected))
}

/// 只做词法归一，不跟随软链：`--query --roots` 打印的是根软链本身。
fn lexical_absolute(path: &Path) -> PathBuf {
    let mut normalized = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
    };
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(_) => {}
        }
    }
    normalized
}

fn unpin_with_dirs(name: &str, gcroot_dirs: &[PathBuf], local_dir: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for dir in gcroot_dirs {
        let link = dir.join(name);
        if fs::remove_file(&link).is_ok() {
            removed.push(link);
        }
    }
    let link = local_dir.join(name);
    if fs::remove_file(&link).is_ok() {
        removed.push(link);
    }
    removed
}

#[cfg(unix)]
fn symlink(source: &Path, target: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(source, target).map_err(|error| {
        format!(
            "create pin {} -> {}: {error}",
            target.display(),
            source.display()
        )
    })
}

#[cfg(not(unix))]
fn symlink(_source: &Path, _target: &Path) -> Result<(), String> {
    Err("store pins require Unix symlinks".to_string())
}

enum AddRootError {
    NoNix,
    Failed(String),
}

/// `nix-store` 的两项操作。测试用内存实现，生产用 [`SystemNixStore`]。
trait NixStore {
    fn add_root(&self, link: &Path, store_path: &Path) -> Result<(), AddRootError>;
    fn roots(&self, store_path: &Path) -> Result<Vec<PathBuf>, String>;
}

struct SystemNixStore {
    program: PathBuf,
}

impl SystemNixStore {
    /// `ACB_NIX_STORE` 仅用于测试/部署方显式覆盖，默认走 PATH 上的 `nix-store`。
    fn from_env() -> Self {
        let program = std::env::var_os("ACB_NIX_STORE")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("nix-store"));
        Self { program }
    }
}

impl NixStore for SystemNixStore {
    fn add_root(&self, link: &Path, store_path: &Path) -> Result<(), AddRootError> {
        let output = Command::new(&self.program)
            .arg("--add-root")
            .arg(link)
            .arg("--indirect")
            .arg("-r")
            .arg(store_path)
            .output()
            .map_err(|error| spawn_error(&self.program, error))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(AddRootError::Failed(command_error(&output)))
        }
    }

    fn roots(&self, store_path: &Path) -> Result<Vec<PathBuf>, String> {
        let output = Command::new(&self.program)
            .arg("--query")
            .arg("--roots")
            .arg(store_path)
            .output()
            .map_err(|error| format!("spawn {}: {error}", self.program.display()))?;
        if !output.status.success() {
            return Err(format!(
                "`nix-store --query --roots {}` failed: {}",
                store_path.display(),
                command_error(&output)
            ));
        }
        Ok(parse_roots(&String::from_utf8_lossy(&output.stdout)))
    }
}

/// 解析 `nix-store --query --roots` 的 `<root> -> <store path>` 输出。
fn parse_roots(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| line.split_once(" -> ").map(|(root, _)| PathBuf::from(root)))
        .collect()
}

fn spawn_error(program: &Path, error: std::io::Error) -> AddRootError {
    if error.kind() == std::io::ErrorKind::NotFound {
        AddRootError::NoNix
    } else {
        AddRootError::Failed(format!("spawn {}: {error}", program.display()))
    }
}

fn command_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("acb-pin-{label}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const STORE: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-acb";
    const STORE_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-acb";

    struct FakeNix {
        add_root: Result<(), String>,
        roots: Result<Vec<PathBuf>, String>,
    }

    impl NixStore for FakeNix {
        fn add_root(&self, link: &Path, store_path: &Path) -> Result<(), AddRootError> {
            match &self.add_root {
                Ok(()) => symlink(store_path, link).map_err(AddRootError::Failed),
                Err(error) => Err(AddRootError::Failed(error.clone())),
            }
        }

        fn roots(&self, _store_path: &Path) -> Result<Vec<PathBuf>, String> {
            self.roots.clone()
        }
    }

    struct NoNix;

    impl NixStore for NoNix {
        fn add_root(&self, _link: &Path, _store_path: &Path) -> Result<(), AddRootError> {
            Err(AddRootError::NoNix)
        }

        fn roots(&self, _store_path: &Path) -> Result<Vec<PathBuf>, String> {
            Err("nix-store unavailable".to_string())
        }
    }

    fn verified_nix(link: &Path) -> FakeNix {
        FakeNix {
            add_root: Ok(()),
            roots: Ok(vec![link.to_path_buf()]),
        }
    }

    #[cfg(unix)]
    #[test]
    fn verified_root_is_reported_as_gcroot() {
        let temp = Temp::new("verified");
        let link = temp.0.join("state/pins/acb-gen-1");

        let outcome = pin_with(Path::new(STORE), &link, &verified_nix(&link)).expect("pin");
        assert_eq!(outcome, PinOutcome::Gcroot(link.clone()));
        assert_eq!(fs::read_link(&link).expect("pin target"), Path::new(STORE));
    }

    #[cfg(unix)]
    #[test]
    fn root_not_listed_by_query_is_unprotected() {
        let temp = Temp::new("unverified");
        let link = temp.0.join("state/pins/acb-gen-2");
        let nix = FakeNix {
            add_root: Ok(()),
            roots: Ok(vec![PathBuf::from("/some/other/root")]),
        };

        let outcome = pin_with(Path::new(STORE), &link, &nix).expect("pin");
        let PinOutcome::Unprotected { path, reason } = outcome else {
            panic!("expected unprotected outcome, got {outcome:?}");
        };
        assert_eq!(path.as_deref(), Some(link.as_path()));
        assert!(
            matches!(reason, PinFallback::VerificationFailed(_)),
            "unexpected reason: {reason:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn query_error_is_reported_as_verification_failure() {
        let temp = Temp::new("query-error");
        let link = temp.0.join("state/pins/acb-gen-3");
        let nix = FakeNix {
            add_root: Ok(()),
            roots: Err("daemon unavailable".to_string()),
        };

        let outcome = pin_with(Path::new(STORE), &link, &nix).expect("pin");
        let PinOutcome::Unprotected { reason, .. } = outcome else {
            panic!("expected unprotected outcome, got {outcome:?}");
        };
        assert!(
            matches!(reason, PinFallback::VerificationFailed(_)),
            "unexpected reason: {reason:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_nix_falls_back_to_local_pin() {
        let temp = Temp::new("nonix");
        let link = temp.0.join("state/pins/acb-gen-4");

        let outcome = pin_with(Path::new(STORE_B), &link, &NoNix).expect("pin");
        let PinOutcome::Unprotected { path, reason } = outcome else {
            panic!("expected unprotected outcome, got {outcome:?}");
        };
        assert_eq!(path.as_deref(), Some(link.as_path()));
        assert_eq!(reason, PinFallback::NoNix);
        assert_eq!(fs::read_link(&link).expect("local pin"), Path::new(STORE_B));
    }

    #[cfg(unix)]
    #[test]
    fn add_root_failure_falls_back_to_local_pin() {
        let temp = Temp::new("addroot");
        let link = temp.0.join("state/pins/acb-gen-5");
        let nix = FakeNix {
            add_root: Err("cannot connect to daemon".to_string()),
            roots: Ok(Vec::new()),
        };

        let outcome = pin_with(Path::new(STORE), &link, &nix).expect("pin");
        let PinOutcome::Unprotected { path, reason } = outcome else {
            panic!("expected unprotected outcome, got {outcome:?}");
        };
        assert_eq!(path.as_deref(), Some(link.as_path()));
        assert!(
            matches!(reason, PinFallback::AddRootFailed(_)),
            "unexpected reason: {reason:?}"
        );
        assert_eq!(fs::read_link(&link).expect("local pin"), Path::new(STORE));
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_pin_dir_reports_no_pin() {
        let temp = Temp::new("unwritable");
        let blocker = temp.0.join("blocker");
        fs::write(&blocker, b"not a directory").expect("blocker");
        let link = blocker.join("pins/acb-gen-6");

        let outcome = pin_with(Path::new(STORE), &link, &verified_nix(&link)).expect("pin");
        let PinOutcome::Unprotected { path, reason } = outcome else {
            panic!("expected unprotected outcome, got {outcome:?}");
        };
        assert!(path.is_none());
        assert!(matches!(reason, PinFallback::DirUnwritable(_)));
    }

    #[test]
    fn parses_real_query_roots_output() {
        let stdout = "\
/nix/var/nix/profiles/system-1-link -> /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-acb
/home/nixos/.local/share/acb/state/pins/acb-gen-9 -> /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-acb
";
        assert_eq!(
            parse_roots(stdout),
            vec![
                PathBuf::from("/nix/var/nix/profiles/system-1-link"),
                PathBuf::from("/home/nixos/.local/share/acb/state/pins/acb-gen-9"),
            ]
        );
    }

    #[test]
    fn non_store_payload_is_skipped() {
        let temp = Temp::new("skip");
        let profile = temp.0.join("profile");
        let outcome = pin_store_path(&profile, "gen-7", Path::new("/tmp/acb-snapshot"))
            .expect("pin decision");
        assert_eq!(outcome, PinOutcome::Skipped);
        assert!(!temp.0.join("state/pins").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unpin_removes_new_and_legacy_pins() {
        let temp = Temp::new("unpin");
        let local = temp.0.join("state/pins");
        let legacy = temp.0.join("legacy-gcroots");
        fs::create_dir_all(&local).expect("local dir");
        fs::create_dir_all(&legacy).expect("legacy dir");
        let local_link = local.join("acb-gen-8");
        let legacy_link = legacy.join("acb-gen-8");
        std::os::unix::fs::symlink(STORE, &local_link).expect("local link");
        std::os::unix::fs::symlink(STORE, &legacy_link).expect("legacy link");

        let removed = unpin_with_dirs("acb-gen-8", &[legacy], &local);
        assert_eq!(removed, vec![legacy_link, local_link]);
    }
}
