//! 插件配置与资源服务（schema 17 数据层）。
//!
//! 这些测试只盯「写之前有没有拦住」：
//! 1. 未知键、类型/范围/枚举不符 → 拒；
//! 2. 普通字段塞密钥形态的值 → 拒；声明 secret 的字段 → 只能绑引用；
//! 3. 资源记录缺主键、主键不一致、未知字段 → 拒；
//! 4. 读配置时默认值补齐，且不回显密钥。

use acb_kernel::plugin_admin::{
    bind_secret, config_view, delete_record, list_records, missing_required, put_record, set_config,
};
use acb_kernel::plugin_registry::{
    CommandKind, PluginKind, PluginPolicyConfig, PluginStorageConfig, RegistryPlugin,
};
use acb_kernel::storage::{MemoryStore, PluginDataStore, PluginsStore, SecretsStore};
use serde_json::{Value, json};

const AT: i64 = 1_793_000_000_000;

fn plugin() -> RegistryPlugin {
    RegistryPlugin {
        id: "demo.tools".to_owned(),
        kind: PluginKind::Tool,
        trust_tier: "tool".to_owned(),
        command: "/bin/sh".to_owned(),
        args: vec![],
        timeout_ms: 5000,
        command_kind: CommandKind::Absolute,
        command_fingerprint: None,
        manifest_json: json!({
            "id": "demo.tools",
            "config": [
                {"key": "endpoint", "type": "string", "required": true},
                {"key": "retries", "type": "integer", "min": 0, "max": 5, "default": 2},
                {"key": "mode", "type": "string", "enum": ["fast", "safe"], "default": "safe"},
                {"key": "apiToken", "type": "string", "secret": true}
            ],
            "resources": [
                {"name": "items", "id_field": "id", "fields": [
                    {"key": "id", "type": "string"},
                    {"key": "title", "type": "string"}
                ]}
            ]
        })
        .to_string(),
        schema_version: 1,
        source_seed: None,
        enabled: true,
        created_at: AT,
        updated_at: AT,
        version: 1,
        policy: PluginPolicyConfig::default(),
        storage: PluginStorageConfig::default(),
        capabilities: vec![],
    }
}

fn store_with_plugin() -> (MemoryStore, RegistryPlugin) {
    let mut store = MemoryStore::new();
    let plugin = plugin();
    store.plugin_put(&plugin, AT).expect("能写插件");
    (store, plugin)
}

#[test]
fn config_view_applies_defaults_and_hides_secrets() {
    let (mut store, plugin) = store_with_plugin();
    let mut assignments = serde_json::Map::new();
    assignments.insert("endpoint".to_owned(), json!("http://localhost"));
    set_config(&mut store, &plugin, &assignments, AT).expect("能写");

    let view = config_view(&store, &plugin).expect("能读");
    assert_eq!(
        view.effective.get("endpoint"),
        Some(&json!("http://localhost"))
    );
    assert_eq!(view.effective.get("retries"), Some(&json!(2)));
    assert_eq!(view.effective.get("mode"), Some(&json!("safe")));
    assert!(
        !view.effective.contains_key("apiToken"),
        "密钥字段不能出现在普通配置视图里"
    );
}

#[test]
fn rejects_unknown_key_and_bad_value() {
    let (mut store, plugin) = store_with_plugin();
    let mut unknown = serde_json::Map::new();
    unknown.insert("nope".to_owned(), json!(1));
    assert!(set_config(&mut store, &plugin, &unknown, AT).is_err());

    let mut bad_type = serde_json::Map::new();
    bad_type.insert("retries".to_owned(), json!("three"));
    assert!(set_config(&mut store, &plugin, &bad_type, AT).is_err());

    let mut out_of_range = serde_json::Map::new();
    out_of_range.insert("retries".to_owned(), json!(99));
    assert!(set_config(&mut store, &plugin, &out_of_range, AT).is_err());

    let mut bad_enum = serde_json::Map::new();
    bad_enum.insert("mode".to_owned(), json!("turbo"));
    assert!(set_config(&mut store, &plugin, &bad_enum, AT).is_err());
}

#[test]
fn secret_fields_only_accept_refs() {
    let (mut store, plugin) = store_with_plugin();
    let mut plaintext = serde_json::Map::new();
    plaintext.insert("apiToken".to_owned(), json!("abc"));
    let error = set_config(&mut store, &plugin, &plaintext, AT).unwrap_err();
    assert!(error.contains("密钥字段"));

    // 绑定引用可以；未声明 secret 的字段不能绑。
    store
        .secret_put("demo-token", "v1:kid:x", true, Some("kid"), None, AT)
        .expect("能写密钥");
    bind_secret(&mut store, &plugin, "apiToken", 1).expect("能绑");
    assert!(bind_secret(&mut store, &plugin, "endpoint", 1).is_err());

    let refs = store.plugin_secret_refs_load("demo.tools").expect("能读");
    assert_eq!(refs, vec![("apiToken".to_owned(), 1)]);
}

#[test]
fn missing_required_reports_unset_fields() {
    let (store, plugin) = store_with_plugin();
    assert_eq!(
        missing_required(&store, &plugin).expect("能算"),
        vec!["endpoint".to_owned()]
    );
}

#[test]
fn record_roundtrip_and_validation() {
    let (mut store, plugin) = store_with_plugin();
    put_record(
        &mut store,
        &plugin,
        "items",
        "a1",
        &json!({"id": "a1", "title": "hello"}),
        AT,
    )
    .expect("能写");
    let records = list_records(&store, &plugin, "items").expect("能读");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].record_id, "a1");

    // 未知资源 / 未知字段 / 主键不一致都要拒。
    assert!(put_record(&mut store, &plugin, "nope", "a1", &json!({"id": "a1"}), AT).is_err());
    assert!(
        put_record(
            &mut store,
            &plugin,
            "items",
            "a2",
            &json!({"id": "a2", "extra": true}),
            AT
        )
        .is_err()
    );
    assert!(
        put_record(
            &mut store,
            &plugin,
            "items",
            "a3",
            &json!({"id": "other", "title": "x"}),
            AT
        )
        .is_err()
    );

    assert!(delete_record(&mut store, &plugin, "items", "a1").expect("能删"));
    assert!(
        list_records(&store, &plugin, "items")
            .expect("能读")
            .is_empty()
    );
}

#[test]
fn record_values_are_not_allowed_to_look_like_secrets() {
    let (mut store, plugin) = store_with_plugin();
    let token: Value = json!("abcdefghijklmnopqrstuvwxyz0123456789");
    let error = put_record(
        &mut store,
        &plugin,
        "items",
        "a1",
        &json!({"id": "a1", "title": token}),
        AT,
    )
    .unwrap_err();
    assert!(error.contains("密钥"));
}
