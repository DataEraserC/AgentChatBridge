//! `acbctl` 各子命令共用的配置加载：**文件 + 库对象**，与桥同一语义。
//!
//! 为什么要有这一层：`sessions` 与 `secrets resolve` 是驱动脚本（`drivers_acb.sh`）
//! 的两条命脉——会话清单决定起几个驱动、token 决定驱动能不能连上桥。业务对象
//! 进库（schema 15）之后，只读文件会得到一份空清单：驱动一个都不起，dsh 会话
//! 全部失联，而且脚本自己不报错（「零个会话」是合法输出）。2026-11-09 真机实测
//! 踩中过一次。
//!
//! 语义与 `acb-bridge` 的 `config_objects::sync` 对齐：
//! - 库已初始化（`meta.config_objects_initialized` 存在）→ 用库里的五段**整类替换**
//!   文件里的，文件里写没写都不参与；
//! - 库还没初始化（首次部署、还没跑过桥）→ 按文件加载（迁移前的行为）；
//! - 库打不开、或库里的对象解析失败 → **报错退出**。宁可驱动起不来让人看见，
//!   也不要悄悄按一份过期的文件清单起一堆连不上的驱动。
use std::path::{Path, PathBuf};

use acb_kernel::storage::ConfigObjectsStore;
use acb_kernel::{Config, LoadedConfig};

/// 加载配置：先按文件解析，再（若库已初始化）用库对象重载一遍。
pub fn load(config: &Path, config_dir: Option<&Path>) -> Result<LoadedConfig, String> {
    let dir_label = config_dir
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|| "-".to_owned());
    let loaded = Config::load(config, config_dir).map_err(|error| {
        format!(
            "load config {} (dir {dir_label}): {error}",
            config.display()
        )
    })?;
    let store = acb_kernel::SqliteStore::open(&loaded.config.storage.path).map_err(|error| {
        format!(
            "open store {} (from {}): {error}",
            loaded.config.storage.path,
            config.display()
        )
    })?;
    let Some(objects) = store
        .config_objects_load()
        .map_err(|error| format!("read config objects: {error}"))?
    else {
        // 库还没初始化：首次部署，按文件走（桥第一次启动会把文件导入库）。
        return Ok(loaded);
    };
    Config::load_with_objects(config, config_dir, Some(&objects)).map_err(|error| {
        format!(
            "load config {} with store objects: {error}",
            config.display()
        )
    })
}

/// 与两个子命令原先各自实现的默认值保持一致：`<config 所在目录>/config.d`。
pub fn default_config_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("config.d")
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::storage::ConfigObjectsStore;
    use acb_kernel::{ConfigObject, ConfigObjectKind, ConfigObjects};
    use std::path::PathBuf;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "acb-config-source-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn objects() -> ConfigObjects {
        ConfigObjects {
            connections: vec![ConfigObject {
                id: "qq".to_owned(),
                toml: "[[connections]]\nid = \"qq\"\nplatform = \"qq\"\nkind = \"onebot11\"\ntransport = \"ws-client\"\nurl = \"ws://127.0.0.1:3001\"\naccount = \"10001\"\n"
                    .to_owned(),
            }],
            personas: vec![ConfigObject {
                id: "default".to_owned(),
                toml: "[[personas]]\nid = \"default\"\nname = \"默认\"\ntext = \"你是桥。\"\n"
                    .to_owned(),
            }],
            harnesses: vec![ConfigObject {
                id: "dsh".to_owned(),
                toml: "[[harnesses]]\nid = \"dsh\"\nname = \"DSH\"\n".to_owned(),
            }],
            sessions: vec![ConfigObject {
                id: "from-store".to_owned(),
                toml: "[[sessions]]\nid = \"from-store\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t\" }\n"
                    .to_owned(),
            }],
            ..Default::default()
        }
    }

    /// 引导文件：只有 [secrets]（env 来源）与 [storage]（指向临时库）。
    fn bootstrap(db: &Path) -> String {
        format!(
            "[secrets]\n[[secrets.sources]]\nname = \"t\"\nkind = \"env\"\nvariable = \"ACB_TEST_CFG_SRC\"\n\n[storage]\npath = \"{}\"\n",
            db.display()
        )
    }

    #[test]
    fn 库初始化过就会话来自库而不是文件() {
        unsafe { std::env::set_var("ACB_TEST_CFG_SRC", "s3cret") };
        let dir = TempDir::new("store");
        let base = dir.0.join("config.toml");
        let db = dir.0.join("acb.db");
        // 文件里故意写一个不同的会话：证明它被库里的整类替换掉。
        std::fs::write(
            &base,
            format!(
                "{}[[harnesses]]\nid = \"dsh\"\nname = \"DSH\"\n\n[[personas]]\nid = \"default\"\nname = \"默认\"\ntext = \"你是桥。\"\n\n[[sessions]]\nid = \"from-file\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = {{ secret = \"t\" }}\n",
                bootstrap(&db)
            ),
        )
        .expect("写 base");

        let mut store = acb_kernel::SqliteStore::open(&db).expect("开库");
        store
            .config_objects_replace(&objects(), 1)
            .expect("写入对象");

        let loaded = load(&base, None).expect("加载");
        let ids: Vec<&str> = loaded
            .config
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, vec!["from-store"], "驱动清单必须来自库");
    }

    #[test]
    fn 库没初始化时按文件加载() {
        unsafe { std::env::set_var("ACB_TEST_CFG_SRC", "s3cret") };
        let dir = TempDir::new("fresh");
        let base = dir.0.join("config.toml");
        let db = dir.0.join("acb.db");
        std::fs::write(
            &base,
            format!(
                "{}[[harnesses]]\nid = \"dsh\"\nname = \"DSH\"\n\n[[personas]]\nid = \"default\"\nname = \"默认\"\ntext = \"你是桥。\"\n\n[[sessions]]\nid = \"from-file\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = {{ secret = \"t\" }}\n",
                bootstrap(&db)
            ),
        )
        .expect("写 base");
        // 注意：不初始化库——文件那份就是首次部署的清单。

        let loaded = load(&base, None).expect("加载");
        assert_eq!(loaded.config.sessions[0].id, "from-file");
    }
}
