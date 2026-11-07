//! store generation 的 GC pin。
//!
//! `store` 策略的 generation 只保存指向 `/nix/store` 的软链。nix GC 不知道
//! `current.json` / `lkg.json` 引用了哪条 store path，所以要在可写的 nix gcroot
//! 目录里放一个软链。没有可写 gcroot（非 nix 环境、权限不足）时降级为
//! `<profile>/../state/pins` 的本地 pin，并明确告知它挡不住 nix GC。

use std::fs;
use std::path::{Path, PathBuf};

/// pin 的结果。`Skipped` 表示 payload 不在 `/nix/store`，不需要 pin。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PinOutcome {
    Gcroot(PathBuf),
    Local(PathBuf),
    Skipped,
}

pub(crate) fn pin_store_path(
    root: &Path,
    id: &str,
    store_path: &Path,
) -> Result<PinOutcome, String> {
    if !store_path.starts_with("/nix/store") {
        return Ok(PinOutcome::Skipped);
    }
    pin_with_dirs(
        store_path,
        &pin_name(id),
        &gcroot_candidates(),
        &local_pin_dir(root),
    )
}

/// 删除该 generation 的所有 pin（gcroot + 本地）。不存在的 pin 忽略。
pub(crate) fn unpin(root: &Path, id: &str) -> Vec<PathBuf> {
    unpin_with_dirs(&pin_name(id), &gcroot_candidates(), &local_pin_dir(root))
}

fn pin_name(id: &str) -> String {
    format!("acb-{id}")
}

/// 用户可操作的 gcroot 目录，按优先级排列。
///
/// - `ACB_GCROOT_DIR`：显式覆盖（测试或部署方指定）。
/// - `$XDG_STATE_HOME/nix/gcroots`（默认 `~/.local/state/nix/gcroots`）：
///   nix 为普通用户登记的 gcroots 目录，可创建。
/// - `/nix/var/nix/gcroots/per-user/<user>`：部分部署存在，通常 root 才能写。
/// - `/nix/var/nix/gcroots`：系统级，通常 root 才能写。
fn gcroot_candidates() -> Vec<PathBuf> {
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

fn local_pin_dir(root: &Path) -> PathBuf {
    root.parent()
        .map(|parent| parent.join("state").join("pins"))
        .unwrap_or_else(|| PathBuf::from("state/pins"))
}

fn pin_with_dirs(
    store_path: &Path,
    name: &str,
    gcroot_dirs: &[PathBuf],
    local_dir: &Path,
) -> Result<PinOutcome, String> {
    for dir in gcroot_dirs {
        if fs::create_dir_all(dir).is_err() {
            continue;
        }
        let link = dir.join(name);
        let _ = fs::remove_file(&link);
        if symlink(store_path, &link).is_ok() {
            return Ok(PinOutcome::Gcroot(link));
        }
    }

    fs::create_dir_all(local_dir)
        .map_err(|error| format!("create {}: {error}", local_dir.display()))?;
    let link = local_dir.join(name);
    let _ = fs::remove_file(&link);
    symlink(store_path, &link)?;
    Ok(PinOutcome::Local(link))
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

#[cfg(test)]
mod tests {
    use super::*;

    struct Temp(PathBuf);

    impl Temp {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "acb-pin-{label}-{}",
                std::process::id()
            ));
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

    #[test]
    fn pin_prefers_writable_gcroot_and_unpin_removes_it() {
        let temp = Temp::new("gcroot");
        let gcroot = temp.0.join("gcroots");
        let local = temp.0.join("state/pins");
        let store = Path::new("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-acb");

        let outcome = pin_with_dirs(store, "acb-gen-1", &[gcroot.clone()], &local).expect("pin");
        let PinOutcome::Gcroot(link) = outcome else {
            panic!("expected gcroot pin, got {outcome:?}");
        };
        assert_eq!(fs::read_link(&link).expect("pin target"), store);

        let removed = unpin_with_dirs("acb-gen-1", &[gcroot.clone()], &local);
        assert_eq!(removed, vec![link]);
        assert!(local.join("acb-gen-1").symlink_metadata().is_err());
    }

    #[test]
    fn pin_falls_back_to_local_when_gcroot_is_unwritable() {
        let temp = Temp::new("local");
        let blocker = temp.0.join("blocker");
        fs::write(&blocker, b"not a directory").expect("blocker");
        let local = temp.0.join("state/pins");
        let store = Path::new("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-acb");

        let outcome =
            pin_with_dirs(store, "acb-gen-2", &[blocker.join("gcroots")], &local).expect("pin");
        let PinOutcome::Local(link) = outcome else {
            panic!("expected local pin, got {outcome:?}");
        };
        assert_eq!(fs::read_link(&link).expect("pin target"), store);

        let removed = unpin_with_dirs("acb-gen-2", &[], &local);
        assert_eq!(removed, vec![link]);
    }

    #[test]
    fn non_store_payload_is_skipped() {
        let temp = Temp::new("skip");
        let profile = temp.0.join("profile");
        let outcome = pin_store_path(&profile, "gen-3", Path::new("/tmp/acb-snapshot"))
            .expect("pin decision");
        assert_eq!(outcome, PinOutcome::Skipped);
        assert!(!temp.0.join("state/pins").exists());
    }
}
