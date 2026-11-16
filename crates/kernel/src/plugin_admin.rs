//! 插件配置与资源的读写服务：把 manifest 声明和库里的数据对起来。
//!
//! 三条规矩：
//! 1. **写之前先校验**：类型、枚举、范围、未知键一律拒；密钥形态的值不进普通配置。
//! 2. **读的时候补默认值**：库里没有的字段用 manifest 的 `default`，控制台不用各写一份。
//! 3. **资源按声明校验**：字段不在 manifest 里就拒，`id_field` 必须存在。
//!
//! 这里是桥内唯一入口：控制台、acbctl、将来的 agent 工具都调同一套函数，
//! 不新增第二条写库路径。

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::plugin_manifest::{PluginManifest, check_config_assignment};
use crate::plugin_registry::RegistryPlugin;
use crate::storage::{PluginDataStore, StorageError, StoredPluginConfig, StoredPluginRecord};

/// 插件配置的三个视图：已存值、补过默认值的生效值、manifest 形状。
pub struct PluginConfigView {
    pub schema: Value,
    pub stored: BTreeMap<String, Value>,
    pub effective: BTreeMap<String, Value>,
}

/// 读配置。库里没有的字段用默认值补齐；`secret` 字段只报「有没有绑定」，不回显值。
pub fn config_view<S: PluginDataStore>(
    store: &S,
    plugin: &RegistryPlugin,
) -> Result<PluginConfigView, String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    let stored_rows = store
        .plugin_config_load(&plugin.id)
        .map_err(|error| error.to_string())?;
    let mut stored = BTreeMap::new();
    for row in stored_rows {
        stored.insert(row.key, row.value);
    }
    let mut effective = BTreeMap::new();
    if manifest.config_schema.is_some() {
        // JSON Schema 模式：effective 来自 properties（密钥项走 secret_refs，
        // 不进明文视图）。
        if let Some(properties) = manifest.schema_properties() {
            for (key, property) in properties {
                if manifest.schema_is_secret(key) {
                    continue;
                }
                if let Some(value) = stored.get(key) {
                    effective.insert(key.clone(), value.clone());
                } else if let Some(default) = property.get("default") {
                    effective.insert(key.clone(), default.clone());
                }
            }
        }
    } else {
        for field in &manifest.config {
            if field.secret {
                continue;
            }
            if let Some(value) = stored.get(&field.key) {
                effective.insert(field.key.clone(), value.clone());
            } else if let Some(default) = &field.default {
                effective.insert(field.key.clone(), default.clone());
            }
        }
    }
    Ok(PluginConfigView {
        schema: manifest.form_schema(),
        stored,
        effective,
    })
}

/// 写配置。`assignments` 是「改哪些键」，不是整份替换：没提到的键保持原样。
///
/// 返回实际写入的键。密钥字段不在这里写：声明 `secret = true` 的字段必须走
/// [`bind_secret`]，否则会得到一条明确的错误，而不是悄悄存明文。
pub fn set_config<S: PluginDataStore>(
    store: &mut S,
    plugin: &RegistryPlugin,
    assignments: &Map<String, Value>,
    at: i64,
) -> Result<Vec<String>, String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    let mut written = Vec::new();
    for (key, value) in assignments {
        if manifest.config_schema.is_some() {
            // JSON Schema 模式：密钥项与旧字段列表一样只能走 bind_secret。
            if manifest.schema_is_secret(key) {
                return Err(format!(
                    "{key} 是密钥项（x-secret-ref）：请用 plugin_secret_refs 绑定具名密钥，不要写明文"
                ));
            }
            manifest.schema_validate_value(key, value)?;
            // 形态启发式照旧：密钥长相的键/值不接受明文。
            check_config_assignment(None, key, value)?;
            let ty = manifest
                .schema_properties()
                .and_then(|properties| properties.get(key))
                .and_then(|property| property.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("string");
            store
                .plugin_config_put(&plugin.id, key, value, ty, false, at)
                .map_err(|error| error.to_string())?;
            written.push(key.clone());
            continue;
        }
        let field = manifest.config_field(key);
        if field.is_none() {
            return Err(format!("未知配置项：{key}（manifest 里没有声明）"));
        }
        let field = field.expect("刚判过");
        if field.secret {
            return Err(format!(
                "{key} 是密钥字段：请用 plugin_secret_refs 绑定具名密钥，不要写明文"
            ));
        }
        check_config_assignment(Some(field), key, value)?;
        store
            .plugin_config_put(&plugin.id, key, value, field.ty.as_str(), false, at)
            .map_err(|error| error.to_string())?;
        written.push(key.clone());
    }
    Ok(written)
}

/// 必需的配置项都齐了吗。缺哪些就报哪些（控制台提交前也要能问）。
pub fn missing_required<S: PluginDataStore>(
    store: &S,
    plugin: &RegistryPlugin,
) -> Result<Vec<String>, String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    let stored: Vec<StoredPluginConfig> = store
        .plugin_config_load(&plugin.id)
        .map_err(|error| error.to_string())?;
    let present: std::collections::BTreeSet<&str> =
        stored.iter().map(|row| row.key.as_str()).collect();
    if manifest.config_schema.is_some() {
        return Ok(manifest.schema_missing(&present));
    }
    Ok(manifest
        .config
        .iter()
        .filter(|field| field.required)
        .filter(|field| field.default.is_none())
        .filter(|field| !field.secret && !present.contains(field.key.as_str()))
        .map(|field| field.key.clone())
        .collect())
}

/// 绑定一个密钥引用。`name` 必须对应 manifest 里声明为 secret 的字段。
pub fn bind_secret<S: PluginDataStore>(
    store: &mut S,
    plugin: &RegistryPlugin,
    name: &str,
    secret_id: i64,
) -> Result<(), String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    if manifest.config_schema.is_some() {
        // JSON Schema 模式：x-secret-ref: true 的项才能绑定。
        if !manifest.schema_is_secret(name) {
            return Err(format!(
                "{name} 不是密钥项（config_schema 里没有 x-secret-ref），不能绑定密钥"
            ));
        }
    } else {
        match manifest.config_field(name) {
            Some(field) if field.secret => {}
            Some(_) => return Err(format!("{name} 没有声明 secret = true，不能绑定密钥")),
            None => return Err(format!("未知配置项：{name}")),
        }
    }
    if secret_id <= 0 {
        return Err("secret id 必须为正数".to_owned());
    }
    store
        .plugin_secret_ref_put(&plugin.id, name, secret_id)
        .map_err(|error| error.to_string())
}

/// 资源记录列表。返回 `(record_id, value)`，按 id 排序。
pub fn list_records<S: PluginDataStore>(
    store: &S,
    plugin: &RegistryPlugin,
    resource: &str,
) -> Result<Vec<StoredPluginRecord>, String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    let decl = manifest
        .resource(resource)
        .ok_or_else(|| format!("未知资源：{resource}"))?;
    let _ = decl;
    store
        .plugin_records_load(&plugin.id, resource)
        .map_err(|error| error.to_string())
}

/// 写一条资源记录。`record_id` 由调用方给（控制台表单或插件自己）。
pub fn put_record<S: PluginDataStore>(
    store: &mut S,
    plugin: &RegistryPlugin,
    resource: &str,
    record_id: &str,
    value: &Value,
    at: i64,
) -> Result<(), String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    let decl = manifest
        .resource(resource)
        .ok_or_else(|| format!("未知资源：{resource}"))?;
    if record_id.trim().is_empty() {
        return Err("record_id 不能为空".to_owned());
    }
    let object = value
        .as_object()
        .ok_or_else(|| format!("{resource} 的记录必须是 JSON 对象"))?;
    for (key, item) in object {
        let field = decl
            .fields
            .iter()
            .find(|field| field.key == *key)
            .ok_or_else(|| format!("{resource} 没有声明字段 {key}"))?;
        check_config_assignment(Some(field), key, item)?;
    }
    if let Some(id_field) = &decl.id_field {
        let declared = object
            .get(id_field)
            .ok_or_else(|| format!("{resource} 的记录缺少主键字段 {id_field}"))?;
        let matches_id = declared
            .as_str()
            .map(|text| text == record_id)
            .unwrap_or(false);
        if !matches_id && declared.to_string().trim_matches('"') != record_id {
            return Err(format!(
                "{resource} 的 {id_field} 必须等于 record_id（{record_id}）"
            ));
        }
    }
    store
        .plugin_record_put(&plugin.id, resource, record_id, value, at)
        .map_err(|error| error.to_string())
}

pub fn delete_record<S: PluginDataStore>(
    store: &mut S,
    plugin: &RegistryPlugin,
    resource: &str,
    record_id: &str,
) -> Result<bool, String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    manifest
        .resource(resource)
        .ok_or_else(|| format!("未知资源：{resource}"))?;
    store
        .plugin_record_delete(&plugin.id, resource, record_id)
        .map_err(|error| error.to_string())
}

/// 插件页需要的整块视图：策略、存储、配置形状、已存值、资源计数。
pub fn admin_view<S: PluginDataStore>(store: &S, plugin: &RegistryPlugin) -> Result<Value, String> {
    let manifest = PluginManifest::parse(&plugin.manifest_json)?;
    let view = config_view(store, plugin)?;
    let mut resources = Vec::new();
    for decl in &manifest.resources {
        let records = store
            .plugin_records_load(&plugin.id, &decl.name)
            .map_err(|error| error.to_string())?;
        resources.push(json!({
            "name": decl.name,
            "label": decl.label,
            "idField": decl.id_field,
            "fields": decl.fields,
            "count": records.len(),
        }));
    }
    let secret_refs = store
        .plugin_secret_refs_load(&plugin.id)
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "id": plugin.id,
        "enabled": plugin.enabled,
        "manifest": manifest.form_schema(),
        "config": view.effective,
        "storedConfig": view.stored,
        "missingRequired": missing_required(store, plugin)?,
        "resources": resources,
        "secretRefs": secret_refs
            .into_iter()
            .map(|(name, secret_id)| json!({"name": name, "secretId": secret_id}))
            .collect::<Vec<_>>(),
    }))
}

/// 把库里的配置转成注入插件进程的环境变量形状。密钥不在这里（走 SecretRef）。
pub fn config_env<S: PluginDataStore>(
    store: &S,
    plugin: &RegistryPlugin,
) -> Result<BTreeMap<String, String>, String> {
    let view = config_view(store, plugin)?;
    let mut env = BTreeMap::new();
    for (key, value) in view.effective {
        let text = match value {
            Value::String(text) => text,
            other => other.to_string(),
        };
        env.insert(format!("ACB_PLUGIN_{}", key.to_ascii_uppercase()), text);
    }
    Ok(env)
}

/// 存储层错误转成一句话。非法配置不是 500，是 400。
pub fn is_bad_input(error: &StorageError) -> bool {
    matches!(error, StorageError::Backend(_))
}
