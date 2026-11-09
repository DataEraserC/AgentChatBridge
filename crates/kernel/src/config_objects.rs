//! 业务对象的落库形状：`[[connections]]` / `[[sessions]]` / `[[personas]]` /
//! `[[memories]]` / `[[harnesses]]` 这五类数组。
//!
//! 为什么不是给 `Config` 里的结构体加 serde：那些结构体带着解析后的明文 token、
//! 已经求值过的默认值、以及 `ProviderId` 之类只在内存里有意义的字段。把它们写进库
//! 等于把密钥搬进库，还会把「配置怎么写的」和「配置是什么意思」混成一件事。
//!
//! 所以库里存的是**这一条的 TOML 原文**（`[[sessions]]` 开头的那一段），读出来直接
//! 拼回合并文档再走一遍正常解析。好处：
//!
//! - 校验、默认值、密钥引用解析全都不用重写一遍，两端一定一致；
//! - `token = { secret = "..." }` 存的是引用而不是值，密钥真源仍在 env/agenix；
//! - 手工 `sqlite3` 也能看懂库里那一行是什么。
//!
//! 代价是「库里的文本」必须能被解析——写入前一律先解析校验，坏文本进不去。

use std::fmt;

use crate::config::ConfigError;

/// 五类可入库的业务对象。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ConfigObjectKind {
    Connection,
    Session,
    Persona,
    Memory,
    Harness,
}

impl ConfigObjectKind {
    pub const ALL: [Self; 5] = [
        Self::Connection,
        Self::Session,
        Self::Persona,
        Self::Memory,
        Self::Harness,
    ];

    /// 库里 `kind` 列的取值，也是 `--check` 与日志里的写法。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Session => "session",
            Self::Persona => "persona",
            Self::Memory => "memory",
            Self::Harness => "harness",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "connection" => Some(Self::Connection),
            "session" => Some(Self::Session),
            "persona" => Some(Self::Persona),
            "memory" => Some(Self::Memory),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }

    /// 合并文档里的数组键名（`[[sessions]]` 的 `sessions`）。
    pub fn toml_key(self) -> &'static str {
        match self {
            Self::Connection => "connections",
            Self::Session => "sessions",
            Self::Persona => "personas",
            Self::Memory => "memories",
            Self::Harness => "harnesses",
        }
    }
}

impl fmt::Display for ConfigObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条业务对象：`id` + 它的 TOML 原文（含 `[[kind]]` 表头）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigObject {
    pub id: String,
    pub toml: String,
}

/// 五类对象的整份快照。空的 `Vec` 合法：它就是「库里这一类一条都没有」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigObjects {
    pub connections: Vec<ConfigObject>,
    pub sessions: Vec<ConfigObject>,
    pub personas: Vec<ConfigObject>,
    pub memories: Vec<ConfigObject>,
    pub harnesses: Vec<ConfigObject>,
}

impl ConfigObjects {
    pub fn get(&self, kind: ConfigObjectKind) -> &[ConfigObject] {
        match kind {
            ConfigObjectKind::Connection => &self.connections,
            ConfigObjectKind::Session => &self.sessions,
            ConfigObjectKind::Persona => &self.personas,
            ConfigObjectKind::Memory => &self.memories,
            ConfigObjectKind::Harness => &self.harnesses,
        }
    }

    pub fn get_mut(&mut self, kind: ConfigObjectKind) -> &mut Vec<ConfigObject> {
        match kind {
            ConfigObjectKind::Connection => &mut self.connections,
            ConfigObjectKind::Session => &mut self.sessions,
            ConfigObjectKind::Persona => &mut self.personas,
            ConfigObjectKind::Memory => &mut self.memories,
            ConfigObjectKind::Harness => &mut self.harnesses,
        }
    }

    pub fn is_empty(&self) -> bool {
        ConfigObjectKind::ALL
            .iter()
            .all(|kind| self.get(*kind).is_empty())
    }

    /// 每类各几条，给启动日志与 `--check` 用。
    pub fn counts(&self) -> [usize; 5] {
        let mut out = [0usize; 5];
        for (slot, kind) in ConfigObjectKind::ALL.iter().enumerate() {
            out[slot] = self.get(*kind).len();
        }
        out
    }

    /// 从合并后的文档里抽出这五类数组。
    ///
    /// 首次导入用它：此刻文件（含覆盖层）是唯一来源，把每一条的原文抄进库。
    pub fn from_merged(merged: &toml::Value) -> Result<Self, ConfigError> {
        let mut out = Self::default();
        for kind in ConfigObjectKind::ALL {
            let key = kind.toml_key();
            let Some(value) = merged.get(key) else {
                continue;
            };
            let Some(items) = value.as_array() else {
                return Err(ConfigError::ObjectNotArray {
                    kind: kind.as_str().to_owned(),
                });
            };
            for item in items {
                let object = render_object(kind, item)?;
                out.get_mut(kind).push(object);
            }
        }
        Ok(out)
    }

    /// 用这份快照替换合并文档里的五类数组。
    ///
    /// 注入是**整类替换**，不是追加：库是唯一源，文件里那几段从此不参与。
    pub fn splice_into(&self, merged: &mut toml::Value) -> Result<(), ConfigError> {
        let Some(table) = merged.as_table_mut() else {
            return Err(ConfigError::ObjectNotArray {
                kind: "config".to_owned(),
            });
        };
        for kind in ConfigObjectKind::ALL {
            let key = kind.toml_key();
            let items = self.get(kind);
            if items.is_empty() && !table.contains_key(key) {
                // 两边都没有这一类：不写空数组，省得和「没配」混起来。
                continue;
            }
            let mut array = Vec::with_capacity(items.len());
            for item in items {
                let parsed = parse_object(kind, &item.toml)?;
                if parsed.len() != 1 {
                    return Err(ConfigError::ObjectSnippetShape {
                        kind: kind.as_str().to_owned(),
                        id: item.id.clone(),
                    });
                }
                array.push(parsed.into_iter().next().expect("len checked"));
            }
            table.insert(key.to_owned(), toml::Value::Array(array));
        }
        Ok(())
    }
}

/// 把一条对象渲染成 `[[kind]]` 原文。`id` 取自对象自身，缺了就是配置错误。
pub fn render_object(
    kind: ConfigObjectKind,
    value: &toml::Value,
) -> Result<ConfigObject, ConfigError> {
    let id = value
        .get("id")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ConfigError::ObjectMissingId {
            kind: kind.as_str().to_owned(),
        })?
        .to_owned();
    let mut doc = toml::map::Map::new();
    doc.insert(
        kind.toml_key().to_owned(),
        toml::Value::Array(vec![value.clone()]),
    );
    let text = toml::to_string(&toml::Value::Table(doc)).map_err(|error| {
        ConfigError::ObjectRenderFailed {
            kind: kind.as_str().to_owned(),
            id: id.clone(),
            detail: error.to_string(),
        }
    })?;
    Ok(ConfigObject { id, toml: text })
}

/// 把一段 `[[kind]]` 原文解析回数组元素。段里必须是恰好一条，否则视为坏数据。
pub fn parse_object(kind: ConfigObjectKind, text: &str) -> Result<Vec<toml::Value>, ConfigError> {
    let value: toml::Value =
        toml::from_str(text).map_err(|error| ConfigError::ObjectParseFailed {
            kind: kind.as_str().to_owned(),
            detail: error.to_string(),
        })?;
    let items = value
        .get(kind.toml_key())
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merged(text: &str) -> toml::Value {
        toml::from_str(text).expect("能解析")
    }

    #[test]
    fn 从合并文档抽出五类对象并保留密钥引用() {
        let doc = merged(
            r#"
[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
transport = "ws-client"
url = "ws://127.0.0.1:53001"
account = "1"
token = { secret = "qq-token" }

[[sessions]]
id = "s1"
harness = "dsh"
persona = "p1"
token = { secret = "s1-token" }

[[personas]]
id = "p1"
name = "人格"
text = "你好"

[[harnesses]]
id = "dsh"
name = "DSH"
"#,
        );
        let objects = ConfigObjects::from_merged(&doc).expect("抽得出");
        assert_eq!(objects.counts(), [1, 1, 1, 0, 1]);
        assert_eq!(objects.sessions[0].id, "s1");
        // 原文里是引用，不是明文值——密钥不进库靠这一条。
        assert!(objects.sessions[0].toml.contains(r#"secret = "s1-token""#));
        assert!(!objects.sessions[0].toml.contains("token = \""));
    }

    #[test]
    fn 注入整类替换而不是追加() {
        let mut doc = merged(
            r#"
[[sessions]]
id = "old"
harness = "dsh"
persona = "p1"
token = { secret = "t" }
"#,
        );
        let objects = ConfigObjects {
            sessions: vec![ConfigObject {
                id: "new".to_owned(),
                toml: "[[sessions]]\nid = \"new\"\nharness = \"dsh\"\npersona = \"p1\"\ntoken = { secret = \"t\" }\n"
                    .to_owned(),
            }],
            ..Default::default()
        };
        objects.splice_into(&mut doc).expect("注入成功");
        let items = doc
            .get("sessions")
            .and_then(toml::Value::as_array)
            .expect("还是数组");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].get("id").and_then(toml::Value::as_str),
            Some("new")
        );
    }

    #[test]
    fn 缺_id_的对象拒绝渲染() {
        let doc = merged("[[personas]]\nname = \"没有 id\"\n");
        let error = ConfigObjects::from_merged(&doc).expect_err("应当报错");
        assert!(error.to_string().contains("id"), "{error}");
    }

    #[test]
    fn 段里多于一条视为坏数据() {
        let two = "[[sessions]]\nid = \"a\"\n\n[[sessions]]\nid = \"b\"\n";
        let parsed = parse_object(ConfigObjectKind::Session, two).expect("能解析");
        assert_eq!(parsed.len(), 2);
        let mut doc = merged("[[sessions]]\nid = \"x\"\n");
        let objects = ConfigObjects {
            sessions: vec![ConfigObject {
                id: "a".to_owned(),
                toml: two.to_owned(),
            }],
            ..Default::default()
        };
        let error = objects.splice_into(&mut doc).expect_err("必须拒绝");
        assert!(matches!(error, ConfigError::ObjectSnippetShape { .. }));
    }
}
