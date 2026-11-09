//! 业务对象落库（schema 15）：连接、会话、人格、记忆、harness。
//!
//! 盯三件事：
//! 1. 写下去读回来还是同一份（含顺序、含中文、含密钥引用）；
//! 2. 「导入过但是空的」不能被当成「还没导入」——否则清空会话后重启会复活；
//! 3. 单条增删只动该动的那一条。
//!
//! 这三条对应 `90-console.toml` 那套覆盖层踩过的坑：当时没有任何地方能回答
//! 「这家店到底谁说了算」，两边各写一份，谁也发现不了对方。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    ConfigObject, ConfigObjectKind, ConfigObjects,
    storage::{ConfigObjectsStore, SqliteStore},
};

const AT: i64 = 1_793_000_000_000;

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "acb-config-objects-{}-{name}-{nanos}",
            std::process::id()
        ));
        Self {
            path: dir.join("acb.db"),
        }
    }

    fn open(&self) -> SqliteStore {
        SqliteStore::open(&self.path).expect("能开库")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn object(id: &str, body: &str) -> ConfigObject {
    ConfigObject {
        id: id.to_owned(),
        toml: format!("[[sessions]]\nid = \"{id}\"\n{body}"),
    }
}

fn sample() -> ConfigObjects {
    ConfigObjects {
        connections: vec![ConfigObject {
            id: "qq".to_owned(),
            toml: "[[connections]]\nid = \"qq\"\nplatform = \"qq\"\nkind = \"onebot11\"\naccount = \"10001\"\n"
                .to_owned(),
        }],
        sessions: vec![
            object(
                "main",
                "harness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t-main\" }\n",
            ),
            object(
                "副会话",
                "harness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t-main\" }\n",
            ),
        ],
        personas: vec![ConfigObject {
            id: "default".to_owned(),
            toml: "[[personas]]\nid = \"default\"\nname = \"默认\"\ntext = \"你是桥。\"\n"
                .to_owned(),
        }],
        memories: Vec::new(),
        harnesses: vec![ConfigObject {
            id: "dsh".to_owned(),
            toml: "[[harnesses]]\nid = \"dsh\"\nname = \"DSH\"\n".to_owned(),
        }],
    }
}

#[test]
fn write_read_back_is_identical_and_survives_reopen() {
    let db = TempDb::new("roundtrip");
    let mut store = db.open();
    assert!(
        store.config_objects_load().expect("能读").is_none(),
        "新库应当报「还没导入」"
    );

    store.config_objects_replace(&sample(), AT).expect("能写");

    let loaded = store
        .config_objects_load()
        .expect("能读")
        .expect("已初始化");
    assert_eq!(loaded, sample(), "读回来必须一模一样（含顺序）");

    drop(store);
    let reopened = db.open();
    let again = reopened
        .config_objects_load()
        .expect("能读")
        .expect("已初始化");
    assert_eq!(again, sample(), "关掉再开还在");
}

#[test]
fn empty_set_is_not_the_same_as_never_imported() {
    let db = TempDb::new("empty");
    let mut store = db.open();
    store
        .config_objects_replace(&ConfigObjects::default(), AT)
        .expect("能写空集");
    let loaded = store.config_objects_load().expect("能读");
    assert!(
        matches!(loaded, Some(ref set) if set.is_empty()),
        "清空之后重启不能当成首次导入"
    );
}

#[test]
fn single_row_writes_touch_only_that_row() {
    let db = TempDb::new("single");
    let mut store = db.open();
    store.config_objects_replace(&sample(), AT).expect("能写");

    store
        .config_object_put(
            ConfigObjectKind::Session,
            "main",
            "[[sessions]]\nid = \"main\"\nharness = \"dsh\"\npersona = \"default\"\ntoken = { secret = \"t-main\" }\nname = \"改过\"\n",
            0,
            AT,
        )
        .expect("能改");
    let loaded = store
        .config_objects_load()
        .expect("能读")
        .expect("已初始化");
    assert_eq!(loaded.sessions.len(), 2, "只改一条，不动别的");
    assert!(loaded.sessions[0].toml.contains("改过"));

    assert!(
        store
            .config_object_delete(ConfigObjectKind::Session, "副会话")
            .expect("能删"),
        "存在的行删除要返回 true"
    );
    assert!(
        !store
            .config_object_delete(ConfigObjectKind::Session, "不存在")
            .expect("能删"),
        "不存在的行返回 false"
    );
    let loaded = store
        .config_objects_load()
        .expect("能读")
        .expect("已初始化");
    assert_eq!(loaded.sessions.len(), 1);
    assert_eq!(loaded.sessions[0].id, "main");
    assert_eq!(loaded.personas.len(), 1, "别的类不动");
}
