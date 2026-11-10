//! 插件注册表落库（schema 17）。
//!
//! 盯四件事：
//! 1. 写下去读回来还是同一份（命令、参数、策略、存储、能力声明）；
//! 2. 「导入过但是空的」不能被当成「还没导入」——否则删光插件后重启会复活；
//! 3. 只翻转 enabled 时别的字段不动；
//! 4. 删掉主体时策略/存储/能力一起走（外键 CASCADE）。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::plugin_registry::{
    CommandKind, DataClass, Lifecycle, LocationKind, PluginCapabilityDecl, PluginKind,
    PluginPolicyConfig, PluginStorageConfig, RegistryPlugin, ScopeKind, StorageKind,
};
use acb_kernel::storage::{PluginsStore, SqliteStore};

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
        let dir =
            std::env::temp_dir().join(format!("acb-plugins-{}-{name}-{nanos}", std::process::id()));
        Self {
            path: dir.join("acb.db"),
        }
    }

    fn open(&self) -> SqliteStore {
        SqliteStore::open(&self.path).expect("能打开")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn sample() -> RegistryPlugin {
    RegistryPlugin {
        id: "issue.tools".to_owned(),
        kind: PluginKind::Tool,
        trust_tier: "tool".to_owned(),
        command: "/nix/store/x-acb-issue-tools/bin/acb-issue-tools".to_owned(),
        args: vec!["--fast".to_owned()],
        timeout_ms: 10_000,
        command_kind: CommandKind::NixStore,
        command_fingerprint: Some("sha256:abc".to_owned()),
        manifest_json: r#"{"title":"issue tools"}"#.to_owned(),
        schema_version: 1,
        source_seed: Some("config.d/80-issue-tools.toml".to_owned()),
        enabled: true,
        created_at: AT,
        updated_at: AT,
        version: 1,
        policy: PluginPolicyConfig {
            enabled: true,
            scope_kind: ScopeKind::Room,
            scope_id: Some("qq:group/1".to_owned()),
            capabilities: vec!["tool_call".to_owned()],
            rate_limit: Some(serde_json::json!({"per_minute": 20})),
            lifecycle: Lifecycle::Resident,
            allow_unknown: false,
        },
        storage: PluginStorageConfig {
            kind: StorageKind::External,
            location_kind: LocationKind::Path,
            location: Some("/var/lib/acb/issue.db".to_owned()),
            data_class: DataClass::Domain,
            backup: true,
            retention: Some("30d".to_owned()),
        },
        capabilities: vec![PluginCapabilityDecl {
            capability_id: "plugin:issue.tools:close".to_owned(),
            risk: "high".to_owned(),
            requires_ticket: true,
            scopes: vec!["room".to_owned()],
            description: "关闭 issue".to_owned(),
        }],
    }
}

#[test]
fn plugin_roundtrip_keeps_every_layer() {
    let db = TempDb::new("roundtrip");
    let mut store = db.open();
    store.plugin_put(&sample(), AT).expect("能写插件");
    drop(store);

    let store = db.open();
    let all = store.plugins_load().expect("能读").expect("导入过");
    assert_eq!(all.len(), 1);
    let plugin = &all[0];
    assert_eq!(plugin.id, "issue.tools");
    assert_eq!(plugin.command_kind, CommandKind::NixStore);
    assert_eq!(plugin.args, vec!["--fast".to_owned()]);
    assert_eq!(plugin.policy.scope_kind, ScopeKind::Room);
    assert_eq!(plugin.policy.scope_id.as_deref(), Some("qq:group/1"));
    assert_eq!(
        plugin.policy.rate_limit,
        Some(serde_json::json!({"per_minute": 20}))
    );
    assert_eq!(plugin.storage.kind, StorageKind::External);
    assert_eq!(plugin.storage.data_class, DataClass::Domain);
    assert!(plugin.storage.backup);
    assert_eq!(plugin.capabilities.len(), 1);
    assert_eq!(
        plugin.capabilities[0].capability_id,
        "plugin:issue.tools:close"
    );
    assert!(plugin.capabilities[0].requires_ticket);
}

#[test]
fn empty_after_import_is_not_the_same_as_never_imported() {
    let db = TempDb::new("empty");
    let store = db.open();
    assert!(
        store.plugins_load().expect("能读").is_none(),
        "新库必须返回 None"
    );
    drop(store);

    let mut store = db.open();
    store.plugins_replace(&[], AT).expect("能导入空清单");
    drop(store);

    let store = db.open();
    let all = store.plugins_load().expect("能读").expect("导入过");
    assert!(all.is_empty(), "导入过空清单后必须还是空");
}

#[test]
fn set_enabled_only_touches_enabled() {
    let db = TempDb::new("enabled");
    let mut store = db.open();
    store.plugin_put(&sample(), AT).expect("能写");
    assert!(
        store
            .plugin_set_enabled("issue.tools", false, AT + 1)
            .expect("能改")
    );
    assert!(
        !store
            .plugin_set_enabled("missing.tools", false, AT + 1)
            .expect("能问")
    );

    let plugin = store
        .plugins_load()
        .expect("能读")
        .expect("导入过")
        .pop()
        .expect("有一条");
    assert!(!plugin.enabled);
    assert!(!plugin.policy.enabled);
    assert_eq!(plugin.command, sample().command);
    assert_eq!(plugin.storage.location, sample().storage.location);
}

#[test]
fn delete_cascades_children() {
    let db = TempDb::new("cascade");
    let mut store = db.open();
    store.plugin_put(&sample(), AT).expect("能写");
    assert!(store.plugin_delete("issue.tools").expect("能删"));
    assert!(
        store
            .plugins_load()
            .expect("能读")
            .expect("导入过")
            .is_empty()
    );

    // 再写一次：若子表没跟着删，主键冲突或残留会在这里暴露。
    store.plugin_put(&sample(), AT + 1).expect("能再写");
    assert_eq!(store.plugins_load().expect("能读").expect("有").len(), 1);
}

#[test]
fn replace_is_whole_sale() {
    let db = TempDb::new("replace");
    let mut store = db.open();
    store.plugin_put(&sample(), AT).expect("能写");
    let mut other = sample();
    other.id = "other.tools".to_owned();
    other.capabilities.clear();
    store.plugins_replace(&[other], AT + 1).expect("能整份替换");
    let all = store.plugins_load().expect("能读").expect("有");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, "other.tools");
}
