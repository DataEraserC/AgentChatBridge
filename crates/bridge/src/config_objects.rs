//! 业务对象的启动对齐：文件 → 库（首次导入）或 库 → 内存（之后每次启动）。
//!
//! 规矩只有一条：**库是唯一源**。文件里那五段只在库为空时被抄一次，之后改文件
//! 不生效——和 `[grants]` 自 schema 13 起的做法一致（2026-11-07 用户裁定）。
//! `config.toml` 从此只承担引导参数：库在哪、监听哪、密钥从哪来。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    Config, ConfigObjectKind, ConfigObjects, LoadedConfig, StorageError,
    storage::ConfigObjectsStore,
};

/// 首次导入/后续对齐。返回**最终生效**的那份配置。
///
/// - 库里没有标记 → 把 `loaded` 里（已合并覆盖层的）五类数组抄进库，再原样返回；
/// - 库里有标记 → 用库里的对象重新加载一遍，文件里那五段不再参与。
///
/// 为什么返回重新加载的配置而不是就地改：五类对象和 `[secrets]` 是连在一起的
/// （`token = { secret = "..." }` 要解析），改结构体字段等于绕开全部校验。
/// 重新走一遍 `Config::load_with_objects` 最省事也最不容易漏。
pub fn sync<S: ConfigObjectsStore>(
    store: &mut S,
    config_path: &Path,
    config_dir: &Path,
    loaded: LoadedConfig,
) -> Result<LoadedConfig, Box<dyn std::error::Error>> {
    match store.config_objects_load()? {
        None => {
            let merged: toml::Value = toml::from_str(&loaded.merged_toml)?;
            let objects = ConfigObjects::from_merged(&merged)?;
            let counts = objects.counts();
            store.config_objects_replace(&objects, now_ms())?;
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "bridge.main.config_objects_imported",
                    connections = counts[0],
                    sessions = counts[1],
                    personas = counts[2],
                    memories = counts[3],
                    harnesses = counts[4]
                )
            );
            Ok(loaded)
        }
        Some(objects) => {
            let counts = objects.counts();
            let reloaded =
                Config::load_with_objects(config_path, Some(config_dir), Some(&objects))?;
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "bridge.main.config_objects_from_store",
                    connections = counts[0],
                    sessions = counts[1],
                    personas = counts[2],
                    memories = counts[3],
                    harnesses = counts[4]
                )
            );
            // 文件里还留着的那几段会被静默忽略。不报出来，运维会以为改文件生效了
            // ——这正是 F1 要根掉的那种「无声」。
            let ignored = ignored_file_ids(&loaded.config, &objects);
            if !ignored.is_empty() {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "bridge.main.config_objects_file_ignored",
                        keys = ignored.join(", ")
                    )
                );
            }
            Ok(reloaded)
        }
    }
}

/// 控制台保存时用：拿库里的当前对象，把一份**新文本**当覆盖层解析一遍。
///
/// 与 [`sync`] 的区别是不写库——保存流程要先校验、后落盘，校验阶段不能有副作用。
pub fn load_with<S: ConfigObjectsStore>(
    config_path: &Path,
    config_dir: &Path,
    overlay_text: &str,
    store: &S,
) -> Result<LoadedConfig, Box<dyn std::error::Error>> {
    let objects = store.config_objects_load()?;
    Ok(Config::load_with_console_overlay(
        config_path,
        config_dir,
        overlay_text,
        objects.as_ref(),
    )?)
}

/// 库里的对象快照；还没有就返回空集（首次导入前控制台也得能读配置）。
pub fn snapshot<S: ConfigObjectsStore>(store: &S) -> Result<ConfigObjects, StorageError> {
    Ok(store.config_objects_load()?.unwrap_or_default())
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 把五类对象渲染成一份只含这五段的 TOML 文本。
///
/// 控制台的结构化表单就在这份文本上打补丁：它和配置文件形状一样，所以
/// `config_patch::apply_patch` 不用改；改完再拆回一条一条写库。
pub fn document(objects: &ConfigObjects) -> String {
    let mut out = String::new();
    for kind in acb_kernel::ConfigObjectKind::ALL {
        for object in objects.get(kind) {
            out.push_str(&object.toml);
            if !object.toml.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// [`document`] 的反向：从补丁后的文本拆回五类对象。
///
/// 走 `from_merged` 而不是自己遍历，是为了让校验只存在一处：坏 id、缺 id、
/// 形状不对都在那里报。
pub fn document_to_objects(text: &str) -> Result<ConfigObjects, Box<dyn std::error::Error>> {
    let merged: toml::Value = if text.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(text)?
    };
    Ok(ConfigObjects::from_merged(&merged)?)
}

/// 这份文本里出现了哪些「已经入库、文件写了也不算」的顶层键。
///
/// 控制台保存 TOML 前用它拦一刀：以前覆盖层能整类覆盖，所以用户改了没生效也
/// 不知道；现在宁可报错，也不要再来一次静默吞掉。
pub fn migrated_keys_in(text: &str) -> Result<Vec<&'static str>, Box<dyn std::error::Error>> {
    let merged: toml::Value = match toml::from_str(text) {
        Ok(value) => value,
        // 解析不了就交给后面的校验去报，这里只负责「能不能判断」。
        Err(_) => return Ok(Vec::new()),
    };
    let mut found = Vec::new();
    if merged.get("grants").is_some() {
        found.push("grants");
    }
    for kind in acb_kernel::ConfigObjectKind::ALL {
        if merged.get(kind.toml_key()).is_some() {
            found.push(kind.toml_key());
        }
    }
    Ok(found)
}

/// 文件里有、库里没有的 id，按大类统计成人话（只用于启动警告）。
///
/// 比的是 id 集合，不比内容：内容差异本来就该被忽略，id 不同才是「你以为改生效了」。
fn ignored_file_ids(config: &Config, objects: &ConfigObjects) -> Vec<String> {
    let mut out = Vec::new();
    let pairs: [(ConfigObjectKind, Vec<&str>); 5] = [
        (
            ConfigObjectKind::Connection,
            config
                .connections
                .iter()
                .map(|item| item.id.as_str())
                .collect(),
        ),
        (
            ConfigObjectKind::Session,
            config
                .sessions
                .iter()
                .map(|item| item.id.as_str())
                .collect(),
        ),
        (
            ConfigObjectKind::Persona,
            config
                .personas
                .iter()
                .map(|item| item.id.as_str())
                .collect(),
        ),
        (
            ConfigObjectKind::Memory,
            config
                .memories
                .iter()
                .map(|item| item.id.as_str())
                .collect(),
        ),
        (
            ConfigObjectKind::Harness,
            config
                .harnesses
                .iter()
                .map(|item| item.id.as_str())
                .collect(),
        ),
    ];
    for (kind, file_ids) in pairs {
        let stored: Vec<&str> = objects
            .get(kind)
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let extra = file_ids.iter().filter(|id| !stored.contains(id)).count();
        if extra > 0 {
            out.push(format!("{}×{}", kind.as_str(), extra));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use acb_kernel::storage::MemoryStore;

    /// 密钥在测试里走 env：`sync` 的生产路径是 Strict 模式，引用必须在文件里声明过。
    const TOKEN_ENV: &str = "ACB_TEST_OBJECT_TOKEN";

    fn set_token_env() {
        unsafe { std::env::set_var(TOKEN_ENV, "s3cret") };
    }

    const BASE: &str = r#"
[secrets]
[[secrets.sources]]
name = "t-main"
kind = "env"
variable = "ACB_TEST_OBJECT_TOKEN"

[[personas]]
id = "default"
name = "默认"
text = "你是桥。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "10001"

[[sessions]]
id = "main"
harness = "dsh"
persona = "default"
token = { secret = "t-main" }
"#;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "acb-objects-{label}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).expect("创建临时目录");
            Self(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(label: &str, text: &str) -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new(label);
        let base = dir.join("base.toml");
        let config_dir = dir.join("config.d");
        std::fs::create_dir_all(&config_dir).expect("创建 config.d");
        std::fs::write(&base, text).expect("写 base");
        (dir, base, config_dir)
    }

    #[test]
    fn 首次启动导入一次之后以库为准() {
        set_token_env();
        let (_dir, base, config_dir) = fixture("import", BASE);
        let mut store = MemoryStore::new();
        let loaded = Config::load_fixture(&base, Some(&config_dir)).expect("能加载");
        let synced = sync(&mut store, &base, &config_dir, loaded).expect("首次同步");
        assert_eq!(synced.config.sessions.len(), 1);
        assert_eq!(synced.config.sessions[0].id, "main");

        let stored = store
            .config_objects_load()
            .expect("能读")
            .expect("已初始化");
        assert_eq!(stored.counts(), [1, 1, 1, 0, 1]);

        // 文件被改（加一条会话），第二次启动仍然只认库里那一份。
        let changed = BASE.replace(
            "[[sessions]]",
            "[[sessions]]\nid = \"from-file\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t-main\" }\n\n[[sessions]]",
        );
        std::fs::write(&base, &changed).expect("改 base");
        let loaded = Config::load_fixture(&base, Some(&config_dir)).expect("能加载");
        let synced = sync(&mut store, &base, &config_dir, loaded).expect("第二次同步");
        let ids: Vec<&str> = synced
            .config
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(ids, vec!["main"], "库是唯一源，文件里新加的会话不生效");
    }

    #[test]
    fn 文档与对象互转不丢字段() {
        set_token_env();
        let (_dir, base, config_dir) = fixture("roundtrip", BASE);
        let loaded = Config::load_fixture(&base, Some(&config_dir)).expect("能加载");
        let merged: toml::Value = toml::from_str(&loaded.merged_toml).expect("能解析");
        let objects = ConfigObjects::from_merged(&merged).expect("抽得出");
        let text = document(&objects);
        let back = document_to_objects(&text).expect("拆得回");
        assert_eq!(back.counts(), objects.counts());
        assert_eq!(back.sessions[0].id, objects.sessions[0].id);
        // 密钥仍是引用，不是明文。
        assert!(back.sessions[0].toml.contains("t-main"));
    }

    #[test]
    fn 已入库的段会被识别出来() {
        let keys = migrated_keys_in(BASE).expect("能判断");
        assert!(keys.contains(&"sessions"));
        assert!(keys.contains(&"connections"));
        assert!(!keys.contains(&"grants"));

        let only_bootstrap = "delivery_limit = 10\n\n[console]\nlisten = \"127.0.0.1:3100\"\ntoken = { secret = \"c\" }\n";
        assert!(migrated_keys_in(only_bootstrap).expect("能判断").is_empty());
    }

    #[test]
    fn 覆盖层里的会话也要进首次导入() {
        // 真机上那 6 个会话全在 90-console 覆盖层里，base 里那份是死的。
        set_token_env();
        let (_dir, base, config_dir) = fixture("overlay", BASE);
        std::fs::write(
            config_dir.join(acb_kernel::CONSOLE_CONFIG_FILENAME),
            "[[sessions]]\nid = \"overlay-only\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t-main\" }\n",
        )
        .expect("写覆盖层");
        let mut store = MemoryStore::new();
        let loaded = Config::load_fixture(&base, Some(&config_dir)).expect("能加载");
        sync(&mut store, &base, &config_dir, loaded).expect("首次同步");
        let stored = store
            .config_objects_load()
            .expect("能读")
            .expect("已初始化");
        assert_eq!(
            stored.sessions[0].id, "overlay-only",
            "导入要按合并后的结果"
        );
    }
}
