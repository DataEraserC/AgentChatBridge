//! Runtime message catalog.
//!
//! Messages live in `locales/<locale>.json` as flat `key -> template` maps and
//! are selected from the process environment (`LC_ALL`, then `LC_MESSAGES`,
//! then `LANG`). A `zh*` locale uses `zh-CN`; everything else uses `en`.
//! Templates use `{name}` placeholders filled by [`tr_args`] / the [`t!`] macro.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

const EN: &str = include_str!("../locales/en.json");
const ZH_CN: &str = include_str!("../locales/zh-CN.json");

static CATALOGS: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();
static LOCALE: RwLock<&'static str> = RwLock::new("en");

fn catalogs() -> &'static HashMap<&'static str, HashMap<String, String>> {
    CATALOGS.get_or_init(|| {
        let mut catalogs = HashMap::new();
        catalogs.insert("en", serde_json::from_str(EN).unwrap_or_default());
        catalogs.insert("zh-CN", serde_json::from_str(ZH_CN).unwrap_or_default());
        catalogs
    })
}

/// Pick the locale from `LC_ALL` / `LC_MESSAGES` / `LANG`.
pub fn detect_locale() -> &'static str {
    let value = std::env::var("LC_ALL")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::env::var("LC_MESSAGES")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .or_else(|| std::env::var("LANG").ok().filter(|value| !value.is_empty()))
        .unwrap_or_default();
    if value.to_ascii_lowercase().starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

/// Set the active locale; anything that is not `zh*` falls back to `en`.
pub fn set_locale(locale: &str) {
    let locale = if locale.to_ascii_lowercase().starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    };
    *LOCALE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = locale;
}

/// Initialize from the process environment. Call once at process startup.
pub fn init() {
    set_locale(detect_locale());
}

/// Active locale (`en` or `zh-CN`).
pub fn locale() -> &'static str {
    *LOCALE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 当前 locale 与完整 message map 的 JSON，给控制台前端做运行时 i18n。
pub fn catalog_json() -> String {
    let messages = catalogs()
        .get(locale())
        .or_else(|| catalogs().get("en"))
        .cloned()
        .unwrap_or_default();
    serde_json::json!({
        "locale": locale(),
        "messages": messages,
    })
    .to_string()
}

/// Translate one key. Missing keys fall back to `en`, then to the key itself.
pub fn tr(key: &str) -> String {
    tr_args(key, &[])
}

/// Translate one key and fill `{name}` placeholders.
pub fn tr_args(key: &str, args: &[(&str, String)]) -> String {
    let catalogs = catalogs();
    let template = catalogs
        .get(locale())
        .and_then(|catalog| catalog.get(key))
        .or_else(|| catalogs.get("en").and_then(|catalog| catalog.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_owned());
    if args.is_empty() {
        return template;
    }
    let mut out = template;
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Translate a literal key, optionally with `{name}` placeholders.
///
/// ```
/// use acb_i18n::t;
/// let _ = t!("bridge.main.start_failed", error = "boom");
/// ```
#[macro_export]
macro_rules! t {
    ($key:expr $(,)?) => {
        $crate::tr($key)
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        let args: ::std::vec::Vec<(&str, ::std::string::String)> = ::std::vec![
            $((::std::stringify!($name), ::std::format!("{}", $value))),+
        ];
        $crate::tr_args($key, &args)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_json_contains_locale_and_console_messages() {
        let value: serde_json::Value =
            serde_json::from_str(&catalog_json()).expect("catalog JSON 必须合法");
        assert!(value.get("locale").and_then(|v| v.as_str()).is_some());
        assert!(
            value
                .get("messages")
                .and_then(|messages| messages.get("console.plugins.title"))
                .and_then(|v| v.as_str())
                .is_some(),
            "控制台插件页文案必须在两个 locale 里都存在"
        );
    }
}
