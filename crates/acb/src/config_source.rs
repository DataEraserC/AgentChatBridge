//! `acbctl` 各子命令共用的配置加载：**文件 + 库对象 + 库策略**，与桥同一语义。
//!
//! 为什么要有这一层：`sessions` 与 `secrets resolve` 是驱动脚本（`drivers_acb.sh`）
//! 的两条命脉——会话清单决定起几个驱动、token 决定驱动能不能连上桥。业务对象
//! 进库（schema 15）之后，只读文件会得到一份空清单：驱动一个都不起，dsh 会话
//! 全部失联，而且脚本自己不报错（「零个会话」是合法输出）。2026-11-09 真机实测
//! 踩中过一次。`grants explain/matrix` 同理：库里的策略才是生效的那一份，读文件
//! 会给出「为什么这条权限不生效」的错误答案。
//!
//! 语义与 `acb-bridge` 的 `config_objects::sync` / `sync_policy_from_store` 对齐：
//! - 库已初始化（`meta.config_objects_initialized` 存在）→ 用库里的五段**整类替换**
//!   文件里的，文件里写没写都不参与；
//! - 库里有过策略（`policy_*` 非空）→ 整份替换文件里的 `[grants]`；
//! - 库还没初始化（首次部署、还没跑过桥）→ 按文件加载（迁移前的行为）；
//! - 库打不开、或库里的对象解析失败 → **报错退出**。宁可驱动起不来让人看见，
//!   也不要悄悄按一份过期的文件清单起一堆连不上的驱动。
use std::path::{Path, PathBuf};

use acb_kernel::storage::{ConfigObjectsStore, Policies, SecretsStore, SettingsStore};
use acb_kernel::{Config, LoadedConfig};

/// 加载配置：先按文件解析，再（若库已初始化）用库对象与库策略覆盖。
pub fn load(config: &Path, config_dir: Option<&Path>) -> Result<LoadedConfig, String> {
    let dir_label = config_dir
        .map(|dir| dir.display().to_string())
        .unwrap_or_else(|| "-".to_owned());
    // 密钥入库（P2.5）：先取库内快照，文件/环境来源被删掉也能加载。
    let preset = acb_kernel::secrets_db::preload(config, config_dir)
        .map_err(|error| format!("preload secrets: {error}"))?;
    if !preset.is_empty() {
        acb_kernel::secrets_db::install(preset);
    }
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
    let mut loaded = match store
        .config_objects_load()
        .map_err(|error| format!("read config objects: {error}"))?
    {
        // 库还没初始化：首次部署，按文件走（桥第一次启动会把文件导入库）。
        None => loaded,
        Some(objects) => {
            Config::load_with_objects(config, config_dir, Some(&objects)).map_err(|error| {
                format!(
                    "load config {} with store objects: {error}",
                    config.display()
                )
            })?
        }
    };
    // 策略以库为准（schema 13 起 `[grants]` 只是导入源）。库里还没有策略时保留文件那份，
    // 这是首次部署、桥还没跑过的状态。
    if let Some(grants) = store
        .policy_load()
        .map_err(|error| format!("read policy: {error}"))?
    {
        loaded.config.grants = grants;
    }
    overlay_db_secrets(&mut loaded, &store)?;
    loaded.config.reset_runtime_defaults();
    loaded.config.apply_settings(
        &store
            .settings_load()
            .map_err(|error| format!("read settings: {error}"))?,
    )?;
    Ok(loaded)
}

/// 库内密钥覆盖文件来源。密文缺密钥直接报错，不静默按明文读。
pub fn overlay_db_secrets(
    loaded: &mut LoadedConfig,
    store: &impl SecretsStore,
) -> Result<(), String> {
    let rows = store
        .secrets_load()
        .map_err(|error| format!("read secrets: {error}"))?;
    if rows.is_empty() {
        return Ok(());
    }
    let key = acb_kernel::crypto::AcbKey::load()?;
    for row in rows {
        let value = if row.encrypted {
            let key = key.as_ref().ok_or_else(|| {
                format!("secret {} is encrypted but ACB_KEY is not set", row.name)
            })?;
            key.decrypt(&row.value)
                .map_err(|error| format!("decrypt secret {}: {error}", row.name))?
        } else {
            row.value
        };
        loaded.secrets.insert_runtime(row.name, value);
    }
    Ok(())
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
    use acb_kernel::storage::{ConfigObjectsStore, Policies};
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

    /// 策略也是「库为准」：文件里的 `[grants]` 只是导入源。
    #[test]
    fn 库里的策略优先于文件的_grants() {
        unsafe { std::env::set_var("ACB_TEST_CFG_SRC", "s3cret") };
        let dir = TempDir::new("policy");
        let base = dir.0.join("config.toml");
        let db = dir.0.join("acb.db");
        std::fs::write(
            &base,
            format!(
                "{}[[grants.rules]]\nid = \"from-file\"\nsubject = \"system_admin\"\nplace = \"group\"\nscope = \"room\"\ntarget = {{ kind = \"room\", id = \"qq:group/1\" }}\ncapability = [\"plugin\"]\n",
                bootstrap(&db)
            ),
        )
        .expect("写 base");

        let seed_path = dir.0.join("seed.toml");
        std::fs::write(
            &seed_path,
            format!(
                "{}[[grants.rules]]\nid = \"from-store\"\nsubject = \"system_admin\"\nplace = \"group\"\nscope = \"room\"\ntarget = {{ kind = \"room\", id = \"qq:group/2\" }}\ncapability = [\"plugin\"]\n",
                bootstrap(&db)
            ),
        )
        .expect("写 seed");
        let seed = Config::load(&seed_path, None)
            .expect("解析种子策略")
            .config
            .grants;

        let mut store = acb_kernel::SqliteStore::open(&db).expect("开库");
        store.policy_replace(&seed).expect("写入策略");

        let loaded = load(&base, None).expect("加载");
        let ids: Vec<&str> = loaded
            .config
            .grants
            .rules
            .iter()
            .map(|rule| rule.id.as_str())
            .collect();
        assert_eq!(ids, vec!["from-store"], "权限判定必须来自库");
    }

    /// 库里还没策略（首次部署、桥没跑过）时文件仍然是唯一来源。
    #[test]
    fn 库没策略时保留文件的_grants() {
        unsafe { std::env::set_var("ACB_TEST_CFG_SRC", "s3cret") };
        let dir = TempDir::new("policy-empty");
        let base = dir.0.join("config.toml");
        let db = dir.0.join("acb.db");
        std::fs::write(
            &base,
            format!(
                "{}[[grants.rules]]\nid = \"from-file\"\nsubject = \"system_admin\"\nplace = \"group\"\nscope = \"room\"\ntarget = {{ kind = \"room\", id = \"qq:group/1\" }}\ncapability = [\"plugin\"]\n",
                bootstrap(&db)
            ),
        )
        .expect("写 base");

        let loaded = load(&base, None).expect("加载");
        assert_eq!(loaded.config.grants.rules[0].id, "from-file");
    }
}
