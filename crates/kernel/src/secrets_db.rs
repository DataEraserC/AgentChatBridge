//! 启动期把库里的密钥预取出来，供 `Config::load_with_secrets` 注入。
//!
//! 顺序问题：`Config::load` 会在解析 `{ secret = "name" }` 时就读来源；而库路径
//! 写在配置里。所以这里先用一个只认 `[storage].path` 的轻量解析拿到库路径，
//! 打开库读出密文，用 `ACB_KEY*` 解开，再拿这份表去加载完整配置。
//!
//! 读不到（库文件不存在、还没建表、没有密钥行）都返回空表：这时仍按
//! `[secrets]` 声明解析，行为与入库前一致。读到了但缺解密钥则报错——
//! 密文当明文读是事故，不是降级。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub use crate::config::SecretPreset;
use crate::crypto::AcbKey;
use crate::storage::{SecretsStore, SqliteStore};

/// 启动期装进来的那份快照。`Config::load` 的每条重载路径（库对象、控制台覆盖层）
/// 都会走到它，不必给每个入口都加参数；`load_with_secrets` 的显式参数优先。
static SNAPSHOT: OnceLock<SecretPreset> = OnceLock::new();

pub fn install(preset: SecretPreset) {
    // 只装一次：进程内密钥不热更，改了库要重启。
    let _ = SNAPSHOT.set(preset);
}

pub fn current() -> Option<&'static SecretPreset> {
    SNAPSHOT.get()
}

/// 从配置所在目录读 `[storage].path`（后读的分片覆盖先读的）。
///
/// 不追求与完整合并完全等价：`[storage]` 只会出现在 base 或普通分片里，
/// 控制台覆盖层不允许写它。
fn storage_path(base_path: &Path, config_dir: Option<&Path>) -> Option<PathBuf> {
    fn read_into(path: &Path, out: &mut Option<PathBuf>) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        let Ok(value) = text.parse::<toml::Value>() else {
            return;
        };
        if let Some(path) = value
            .get("storage")
            .and_then(|storage| storage.get("path"))
            .and_then(|path| path.as_str())
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            *out = Some(PathBuf::from(path));
        }
    }

    // 没写 `[storage]` 就用默认值：与 `SqliteStore::open` 一样按进程 cwd 解析。
    let mut found = Some(PathBuf::from(crate::config::DEFAULT_STORAGE_PATH));
    read_into(base_path, &mut found);
    if let Some(dir) = config_dir.filter(|dir| dir.is_dir()) {
        let mut fragments: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .collect();
        fragments.sort();
        for fragment in fragments {
            read_into(&fragment, &mut found);
        }
    }
    found
}

/// 预取库内密钥。返回值直接喂给 `Config::load_with_secrets`。
pub fn preload(base_path: &Path, config_dir: Option<&Path>) -> Result<SecretPreset, String> {
    let Some(path) = storage_path(base_path, config_dir) else {
        return Ok(SecretPreset::new());
    };
    if !path.exists() {
        return Ok(SecretPreset::new());
    }
    let store = SqliteStore::open(&path)
        .map_err(|error| format!("open store {} for secrets: {error}", path.display()))?;
    let rows = store
        .secrets_load()
        .map_err(|error| format!("read secrets: {error}"))?;
    if rows.is_empty() {
        return Ok(SecretPreset::new());
    }
    let key = AcbKey::load()?;
    let mut preset = BTreeMap::new();
    for row in rows {
        let value = if row.encrypted {
            let key = key.as_ref().ok_or_else(|| {
                format!(
                    "secret {} is encrypted but ACB_KEY_FILE / ACB_KEY is not set",
                    row.name
                )
            })?;
            key.decrypt(&row.value)
                .map_err(|error| format!("decrypt secret {}: {error}", row.name))?
        } else {
            row.value
        };
        preset.insert(row.name, value);
    }
    Ok(preset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_storage_path_from_base_and_fragments() {
        let dir = std::env::temp_dir().join(format!(
            "acb-secret-preload-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));
        let config_dir = dir.join("config.d");
        std::fs::create_dir_all(&config_dir).expect("mkdir");
        let base = dir.join("config.toml");
        std::fs::write(&base, "[storage]\npath = \"state/a.db\"\n").expect("write base");
        let fragment = config_dir.join("10-storage.toml");
        std::fs::write(&fragment, "[storage]\npath = \"state/b.db\"\n").expect("write fragment");

        let found = storage_path(&base, Some(&config_dir)).expect("path");
        assert_eq!(found, PathBuf::from("state/b.db"));

        // 库文件不存在时返回空表，不报错。
        let preset = preload(&base, Some(&config_dir)).expect("preload");
        assert!(preset.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn absent_storage_falls_back_to_default_path() {
        // 现场 `config.active.toml` 没有 `[storage]`，库走默认 `state/acb.db`
        // （相对进程 cwd）。这里断言回落到默认值，而不是「没有存储」。
        let path = storage_path(
            Path::new("/nonexistent/acb/config.toml"),
            Some(Path::new("/nonexistent/acb/config.d")),
        )
        .expect("默认路径");
        assert_eq!(path, PathBuf::from(crate::config::DEFAULT_STORAGE_PATH));
    }
}
