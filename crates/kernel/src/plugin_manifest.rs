//! 插件 manifest 的配置/资源/动作声明。
//!
//! 形状校验留在 core：控制台要按它渲染表单，桥要在写库前拒绝不合法的值。
//! 但**语义**归插件：声明里写什么字段、什么范围，core 只照着执行，不猜。
//!
//! 密钥规则：字段声明 `secret = true` 才允许存密钥形态的值，并且实际值走
//! `plugin_secret_refs`，不进 `plugin_config`。反过来，普通字段里出现
//! token/password/api_key 这类键或高熵串一律拒绝——宁可让插件作者显式声明，
//! 也不要把明文密码静静写进库。

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 字段类型。和 `plugin_config.type` 的 CHECK 约束一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
}

impl FieldType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "string" => Self::String,
            "integer" => Self::Integer,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            "object" => Self::Object,
            _ => return None,
        })
    }

    fn matches(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Array => value.is_array(),
            Self::Object => value.is_object(),
        }
    }
}

/// 一个配置字段的声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigField {
    pub key: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub ty: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default, rename = "enum")]
    pub enum_values: Vec<Value>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// 声明为密钥字段：值不进 `plugin_config`，改走 `plugin_secret_refs`。
    #[serde(default)]
    pub secret: bool,
}

impl ConfigField {
    /// 校验一个值。返回规范化后的值（例如整数不越界）。
    pub fn validate(&self, value: &Value) -> Result<(), String> {
        if !self.ty.matches(value) {
            return Err(format!(
                "{} 需要 {}，实际是 {}",
                self.key,
                self.ty.as_str(),
                kind_of(value)
            ));
        }
        if !self.enum_values.is_empty() && !self.enum_values.iter().any(|item| item == value) {
            return Err(format!(
                "{} 只能取 {} 之一",
                self.key,
                serde_json::to_string(&self.enum_values).unwrap_or_else(|_| "[]".to_owned())
            ));
        }
        if let Some(number) = value.as_f64() {
            if let Some(min) = self.min {
                if number < min {
                    return Err(format!("{} 不能小于 {min}", self.key));
                }
            }
            if let Some(max) = self.max {
                if number > max {
                    return Err(format!("{} 不能大于 {max}", self.key));
                }
            }
        }
        Ok(())
    }
}

/// 资源（插件自己管的记录）的声明。`id_field` 指向 fields 里做主键的那个。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceDecl {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub id_field: Option<String>,
    #[serde(default)]
    pub fields: Vec<ConfigField>,
}

/// 动作（插件提供的显式操作）的声明。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionDecl {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// low / medium / high；缺省按 high。
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub requires_ticket: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PluginManifest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub config: Vec<ConfigField>,
    #[serde(default)]
    pub resources: Vec<ResourceDecl>,
    #[serde(default)]
    pub actions: Vec<ActionDecl>,
}

impl PluginManifest {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let manifest: Self =
            serde_json::from_str(raw).map_err(|error| format!("manifest JSON: {error}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::BTreeSet::new();
        for field in &self.config {
            if field.key.trim().is_empty() {
                return Err("config 字段 key 不能为空".to_owned());
            }
            if !seen.insert(field.key.clone()) {
                return Err(format!("config 字段重复：{}", field.key));
            }
            if field.secret && field.ty != FieldType::String {
                return Err(format!("密钥字段 {} 只能是 string", field.key));
            }
            if let Some(default) = &field.default {
                field.validate(default)?;
            }
        }
        let mut resources = std::collections::BTreeSet::new();
        for resource in &self.resources {
            if resource.name.trim().is_empty() {
                return Err("resource name 不能为空".to_owned());
            }
            if !resources.insert(resource.name.clone()) {
                return Err(format!("resource 重复：{}", resource.name));
            }
            let mut fields = std::collections::BTreeSet::new();
            for field in &resource.fields {
                if !fields.insert(field.key.clone()) {
                    return Err(format!(
                        "resource {} 的字段重复：{}",
                        resource.name, field.key
                    ));
                }
            }
            if let Some(id_field) = &resource.id_field {
                if !fields.contains(id_field) {
                    return Err(format!(
                        "resource {} 的 id_field {} 不在 fields 里",
                        resource.name, id_field
                    ));
                }
            }
        }
        let mut actions = std::collections::BTreeSet::new();
        for action in &self.actions {
            if action.name.trim().is_empty() {
                return Err("action name 不能为空".to_owned());
            }
            if !actions.insert(action.name.clone()) {
                return Err(format!("action 重复：{}", action.name));
            }
            if let Some(risk) = &action.risk {
                if !matches!(risk.as_str(), "low" | "medium" | "high") {
                    return Err(format!("action {} 的 risk 不合法：{risk}", action.name));
                }
            }
        }
        Ok(())
    }

    pub fn config_field(&self, key: &str) -> Option<&ConfigField> {
        self.config.iter().find(|field| field.key == key)
    }

    pub fn resource(&self, name: &str) -> Option<&ResourceDecl> {
        self.resources.iter().find(|resource| resource.name == name)
    }

    pub fn action(&self, name: &str) -> Option<&ActionDecl> {
        self.actions.iter().find(|action| action.name == name)
    }

    /// 控制台表单用的形状（不含任何实际值）。
    pub fn form_schema(&self) -> Value {
        json!({
            "config": self.config,
            "resources": self.resources,
            "actions": self.actions,
        })
    }
}

/// 密钥形态的键名。用 `contains` 是为了覆盖 `api_key` / `accessToken` 这类写法。
const SECRET_HINTS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "accesskey",
    "private_key",
    "credential",
];

/// 字段名看起来像密钥吗。
pub fn looks_like_secret_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SECRET_HINTS.iter().any(|hint| lower.contains(hint))
}

/// 值看起来像高熵串吗：长度 >= 24 且没有空格、字母数字占比高。
///
/// 这是启发式，不是保证。目的是拦住「把 token 贴进普通输入框」这种最常见的事故。
pub fn looks_like_secret_value(value: &str) -> bool {
    if value.len() < 24 || value.chars().any(char::is_whitespace) {
        return false;
    }
    let alnum = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '=' | '_' | '-' | '.'))
        .count();
    alnum * 10 >= value.chars().count() * 9
}

/// 写库前的检查：普通字段不接受密钥形态的东西；密钥字段必须显式声明。
pub fn check_config_assignment(
    field: Option<&ConfigField>,
    key: &str,
    value: &Value,
) -> Result<(), String> {
    let declared_secret = field.map(|field| field.secret).unwrap_or(false);
    if let Some(field) = field {
        field.validate(value)?;
    }
    if declared_secret {
        return Ok(());
    }
    if looks_like_secret_key(key) {
        return Err(format!(
            "{key} 看起来是密钥字段；请在 manifest 里声明 secret = true，值走密钥库"
        ));
    }
    if let Some(text) = value.as_str() {
        if looks_like_secret_value(text) {
            return Err(format!(
                "{key} 的值像密钥；请改用具名密钥引用，不要把明文写进配置"
            ));
        }
    }
    Ok(())
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        PluginManifest::parse(
            r#"{
              "id": "demo.tools",
              "title": "Demo",
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
              ],
              "actions": [{"name": "sync", "risk": "medium"}]
            }"#,
        )
        .expect("manifest 合法")
    }

    #[test]
    fn parses_and_indexes() {
        let manifest = manifest();
        assert_eq!(manifest.config.len(), 4);
        assert!(manifest.config_field("retries").is_some());
        assert!(manifest.resource("items").is_some());
        assert!(manifest.action("sync").is_some());
        assert!(manifest.config_field("missing").is_none());
    }

    #[test]
    fn field_type_and_range_are_enforced() {
        let manifest = manifest();
        let retries = manifest.config_field("retries").expect("有");
        assert!(retries.validate(&json!(3)).is_ok());
        assert!(retries.validate(&json!(9)).is_err());
        assert!(retries.validate(&json!("3")).is_err());
        let mode = manifest.config_field("mode").expect("有");
        assert!(mode.validate(&json!("fast")).is_ok());
        assert!(mode.validate(&json!("other")).is_err());
    }

    #[test]
    fn duplicate_and_dangling_declarations_are_rejected() {
        let duplicate = r#"{"config":[{"key":"a","type":"string"},{"key":"a","type":"string"}]}"#;
        assert!(PluginManifest::parse(duplicate).is_err());
        let dangling = r#"{"resources":[{"name":"x","id_field":"nope","fields":[]}]}"#;
        assert!(PluginManifest::parse(dangling).is_err());
    }

    #[test]
    fn secret_fields_must_be_string() {
        let raw = r#"{"config":[{"key":"apiToken","type":"integer","secret":true}]}"#;
        assert!(PluginManifest::parse(raw).is_err());
    }

    #[test]
    fn secret_shapes_are_rejected_outside_declared_fields() {
        let manifest = manifest();
        // endpoint 未声明 secret，但值是高熵串 → 拒绝。
        let endpoint = manifest.config_field("endpoint").expect("有");
        let token = "abcdefghijklmnopqrstuvwxyz0123456789";
        assert!(check_config_assignment(Some(endpoint), "endpoint", &json!(token)).is_err());

        // 声明了 secret 的字段可以存密钥形态的值。
        let api = manifest.config_field("apiToken").expect("有");
        assert!(check_config_assignment(Some(api), "apiToken", &json!(token)).is_ok());

        // 键名像密钥、又没声明 → 拒绝。
        assert!(check_config_assignment(None, "api_password", &json!("x")).is_err());
    }

    #[test]
    fn ordinary_text_is_not_flagged() {
        assert!(!looks_like_secret_value("这是普通的一句话，不是密钥"));
        assert!(!looks_like_secret_value("short"));
        assert!(looks_like_secret_value(
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"
        ));
    }
}
