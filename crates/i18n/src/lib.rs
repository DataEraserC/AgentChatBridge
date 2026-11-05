//! Runtime message catalog.
//!
//! Messages live in `locales/<locale>.json` as flat `key -> template` maps.
//! The process default comes from the environment (`LC_ALL`, then
//! `LC_MESSAGES`, then `LANG`). HTTP handlers can translate per request with
//! [`tr_in`] / [`tr_args_in`] so the console follows the language the user
//! picked instead of the process locale.
//!
//! Templates use `{name}` placeholders filled by [`tr_args`] / the [`t!`]
//! macro, or by [`Message`] rendered at the response boundary.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

const EN: &str = include_str!("../locales/en.json");
const ZH_CN: &str = include_str!("../locales/zh-CN.json");

/// Locales the catalog supports. The console switch uses the same list.
pub const LOCALES: &[&str] = &["en", "zh-CN"];

static CATALOGS: OnceLock<HashMap<&'static str, HashMap<String, String>>> = OnceLock::new();
static LOCALE: RwLock<&'static str> = RwLock::new("en");

thread_local! {
    static REQUEST_LOCALE: Cell<Option<&'static str>> = const { Cell::new(None) };
}

/// Run a closure with a request-scoped locale.
///
/// Existing [`t!`] calls inside the closure (including errors built by
/// kernel/plugin helpers) render in that locale without changing their APIs.
/// The previous value is restored when the closure returns.
pub fn with_locale<T>(locale: &str, f: impl FnOnce() -> T) -> T {
    let previous = REQUEST_LOCALE.with(|cell| cell.replace(Some(normalize_locale(locale))));
    let result = f();
    REQUEST_LOCALE.with(|cell| cell.set(previous));
    result
}

/// Active request locale, or the process locale outside a request.
pub fn request_locale() -> &'static str {
    REQUEST_LOCALE.with(Cell::get).unwrap_or_else(locale)
}

fn catalogs() -> &'static HashMap<&'static str, HashMap<String, String>> {
    CATALOGS.get_or_init(|| {
        let mut catalogs = HashMap::new();
        catalogs.insert("en", serde_json::from_str(EN).unwrap_or_default());
        catalogs.insert("zh-CN", serde_json::from_str(ZH_CN).unwrap_or_default());
        catalogs
    })
}

/// Normalize a locale tag to one of [`LOCALES`]. Anything that does not look
/// like Chinese falls back to `en`, matching [`set_locale`].
pub fn normalize_locale(tag: &str) -> &'static str {
    if tag.to_ascii_lowercase().starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

/// Pick the first supported language from an `Accept-Language` header.
///
/// Quality values are ignored: the first supported tag wins. `*` and tags
/// outside the catalog are skipped.
pub fn locale_from_accept_language(header: &str) -> Option<&'static str> {
    header.split(',').find_map(|part| {
        let tag = part.split(';').next().unwrap_or("").trim();
        let lower = tag.to_ascii_lowercase();
        if lower.starts_with("zh") {
            Some("zh-CN")
        } else if lower.starts_with("en") {
            Some("en")
        } else {
            None
        }
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
    normalize_locale(&value)
}

/// Set the active process locale.
pub fn set_locale(locale: &str) {
    *LOCALE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = normalize_locale(locale);
}

/// Initialize from the process environment. Call once at process startup.
pub fn init() {
    set_locale(detect_locale());
}

/// Active process locale (`en` or `zh-CN`).
pub fn locale() -> &'static str {
    *LOCALE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Whether `locale` contains `key`.
pub fn has_key(locale: &str, key: &str) -> bool {
    catalogs()
        .get(normalize_locale(locale))
        .is_some_and(|catalog| catalog.contains_key(key))
}

/// Full message map for one locale, as JSON for the console front-end.
pub fn catalog_json_for(locale: &str) -> String {
    let locale = normalize_locale(locale);
    let messages = catalogs().get(locale).cloned().unwrap_or_default();
    serde_json::json!({
        "locale": locale,
        "messages": messages,
    })
    .to_string()
}

/// 当前进程 locale 与完整 message map 的 JSON，给控制台前端做运行时 i18n。
/// 请求级调用请用 [`catalog_json_for`]。
pub fn catalog_json() -> String {
    catalog_json_for(locale())
}

/// Translate one key for the process locale.
pub fn tr(key: &str) -> String {
    tr_args(key, &[])
}

/// Translate one key and fill `{name}` placeholders for the process locale.
pub fn tr_args(key: &str, args: &[(&str, String)]) -> String {
    tr_args_in(request_locale(), key, args)
}

/// Translate one key for an explicit locale.
pub fn tr_in(locale: &str, key: &str) -> String {
    tr_args_in(locale, key, &[])
}

/// Translate one key and fill `{name}` placeholders for an explicit locale.
/// Missing keys fall back to `en`, then to the key itself.
pub fn tr_args_in(locale: &str, key: &str, args: &[(&str, String)]) -> String {
    let locale = normalize_locale(locale);
    let catalogs = catalogs();
    let template = catalogs
        .get(locale)
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

/// Translate a literal key, optionally with `{name}` placeholders, using the
/// process locale.
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

/// Translate a literal key for an explicit locale.
///
/// ```
/// use acb_i18n::t_in;
/// let _ = t_in!("en", "bridge.main.start_failed", error = "boom");
/// ```
#[macro_export]
macro_rules! t_in {
    ($locale:expr, $key:expr $(,)?) => {
        $crate::tr_in($locale, $key)
    };
    ($locale:expr, $key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        let args: ::std::vec::Vec<(&str, ::std::string::String)> = ::std::vec![
            $((::std::stringify!($name), ::std::format!("{}", $value))),+
        ];
        $crate::tr_args_in($locale, $key, &args)
    }};
}

/// One argument of a [`Message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Arg {
    Text(String),
    Message(Box<Message>),
}

/// A translation key plus its arguments, rendered later with a request locale.
///
/// Error constructors build a `Message`; the HTTP response boundary calls
/// [`Message::render`] with the locale parsed from `?locale=` or
/// `Accept-Language`. Nested messages are rendered recursively with the same
/// locale, so a wrapper key can include an inner keyed error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    key: &'static str,
    args: Vec<(&'static str, Arg)>,
}

impl Message {
    pub fn new(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, name: &'static str, value: impl MessageArg) -> Self {
        self.args.push((name, value.into_arg()));
        self
    }

    /// The catalog key this message was built from.
    pub fn key(&self) -> &'static str {
        self.key
    }

    /// Render the message with an explicit locale.
    pub fn render(&self, locale: &str) -> String {
        let locale = normalize_locale(locale);
        let template = {
            let catalogs = catalogs();
            catalogs
                .get(locale)
                .and_then(|catalog| catalog.get(self.key))
                .or_else(|| catalogs.get("en").and_then(|catalog| catalog.get(self.key)))
                .cloned()
                .unwrap_or_else(|| self.key.to_owned())
        };
        if self.args.is_empty() {
            return template;
        }
        let mut out = template;
        for (name, arg) in &self.args {
            let value = match arg {
                Arg::Text(text) => text.clone(),
                Arg::Message(message) => message.render(locale),
            };
            out = out.replace(&format!("{{{name}}}"), &value);
        }
        out
    }
}

/// Values that can be passed to [`Message::arg`].
pub trait MessageArg {
    fn into_arg(self) -> Arg;
}

impl MessageArg for String {
    fn into_arg(self) -> Arg {
        Arg::Text(self)
    }
}

impl MessageArg for &str {
    fn into_arg(self) -> Arg {
        Arg::Text(self.to_owned())
    }
}

impl MessageArg for &String {
    fn into_arg(self) -> Arg {
        Arg::Text(self.clone())
    }
}

impl MessageArg for Message {
    fn into_arg(self) -> Arg {
        Arg::Message(Box::new(self))
    }
}

impl MessageArg for &Message {
    fn into_arg(self) -> Arg {
        Arg::Message(Box::new(self.clone()))
    }
}

macro_rules! text_message_arg {
    ($($ty:ty),* $(,)?) => {
        $(
            impl MessageArg for $ty {
                fn into_arg(self) -> Arg {
                    Arg::Text(self.to_string())
                }
            }
        )*
    };
}

text_message_arg!(
    i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, bool, f32, f64
);

/// Build a [`Message`] from a literal key and optional `{name}` arguments.
///
/// ```
/// use acb_i18n::msg;
/// let message = msg!("bridge.console.session_not_found", session = "main");
/// ```
#[macro_export]
macro_rules! msg {
    ($key:expr $(,)?) => {
        $crate::Message::new($key)
    };
    ($key:expr, $($name:ident = $value:expr),+ $(,)?) => {{
        $crate::Message::new($key)
            $(.arg(::std::stringify!($name), $value))+
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

    #[test]
    fn catalog_for_locale_returns_that_locale() {
        let zh: serde_json::Value = serde_json::from_str(&catalog_json_for("zh-CN")).unwrap();
        assert_eq!(zh["locale"], "zh-CN");
        let en: serde_json::Value = serde_json::from_str(&catalog_json_for("en-US")).unwrap();
        assert_eq!(en["locale"], "en");
    }

    #[test]
    fn locale_tags_normalize_to_two_catalog_names() {
        assert_eq!(normalize_locale("zh"), "zh-CN");
        assert_eq!(normalize_locale("zh-Hant"), "zh-CN");
        assert_eq!(normalize_locale("en-US"), "en");
        assert_eq!(normalize_locale("fr-FR"), "en");
    }

    #[test]
    fn accept_language_picks_the_first_supported_tag() {
        assert_eq!(
            locale_from_accept_language("zh-CN,zh;q=0.9,en;q=0.8"),
            Some("zh-CN")
        );
        assert_eq!(locale_from_accept_language("en-US,en;q=0.9"), Some("en"));
        assert_eq!(locale_from_accept_language("fr-FR,fr;q=0.9"), None);
        assert_eq!(locale_from_accept_language("*"), None);
        assert_eq!(locale_from_accept_language(""), None);
    }

    #[test]
    fn message_renders_in_the_requested_locale() {
        let message = msg!("bridge.console.session_not_found", session = "main");
        assert_eq!(message.render("en"), "no such session main");
        assert_eq!(message.render("zh-CN"), "没有会话 main");
        assert_eq!(message.key(), "bridge.console.session_not_found");
    }

    #[test]
    fn nested_message_renders_with_the_same_locale() {
        let inner = msg!("bridge.console.session_not_found", session = "main");
        let outer = msg!(
            "bridge.patch.op_failed",
            context = "op 1 (session)",
            error = inner
        );
        assert!(outer.render("en").contains("op 1 (session)"));
        assert!(outer.render("en").contains("no such session main"));
        assert!(outer.render("zh-CN").contains("没有会话 main"));
    }

    #[test]
    fn catalogs_have_the_same_keys_and_placeholders() {
        let catalogs = catalogs();
        let en = catalogs.get("en").expect("en");
        let zh = catalogs.get("zh-CN").expect("zh-CN");
        assert_eq!(en.len(), zh.len(), "两个 locale 的 key 数量必须一致");
        for (key, en_text) in en {
            let zh_text = zh
                .get(key)
                .unwrap_or_else(|| panic!("zh-CN 缺少 key：{key}"));
            assert_eq!(
                placeholders(en_text),
                placeholders(zh_text),
                "key {key} 的 placeholder 不一致"
            );
        }
    }

    fn placeholders(text: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch != '{' {
                continue;
            }
            let mut name = String::new();
            for inner in chars.by_ref() {
                if inner == '}' {
                    break;
                }
                name.push(inner);
            }
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                names.push(name);
            }
        }
        names.sort();
        names
    }

    #[test]
    fn has_key_covers_both_locales() {
        assert!(has_key("en", "console.deps.title"));
        assert!(has_key("zh-CN", "console.deps.title"));
        assert!(!has_key("en", "console.missing.key"));
    }
}
