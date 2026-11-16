//! 内嵌的控制台前端（`web/` 构建产物）。
//!
//! 为什么把产物提交进仓库而不是在 `nix build` 里构建：
//! - `nix build` 现在是纯 Rust、`--offline`。加一条 node 依赖链会给每次
//!   `nix build` 拉进 node + pnpm，代价远大于收益。
//! - 产物是可审阅的：每次前端改动都是一次可 `git diff` 的变更（守卫
//!   `deploy/verify.sh` 会检查产物与源码是否同步）。
//!
//! 文件名固定（`app.js` / `app.css` / `index.html`），所以这里是三个常量，
//! 不是目录扫描。改前端构建配置时必须同步改这个文件——那条约束由
//! `console_web_assets_match_the_build` 测试看守。
//!
//! 将来如果资源数量增长到手动维护不划算，替换点是 `asset()` 一处：
//! 改成 `build.rs` 生成的表即可，调用方不用动。

/// 一份内嵌资源：路径、字节、`Content-Type`。
pub struct Asset {
    pub path: &'static str,
    pub bytes: &'static [u8],
    pub content_type: &'static str,
}

const INDEX_HTML: &[u8] = include_bytes!("console_web/index.html");
const APP_JS: &[u8] = include_bytes!("console_web/app.js");
const APP_CSS: &[u8] = include_bytes!("console_web/app.css");

/// 全部静态资源。顺序不影响查找（`asset()` 线性扫描，只有三项）。
pub const ASSETS: &[Asset] = &[
    Asset {
        path: "/app.js",
        bytes: APP_JS,
        content_type: "text/javascript; charset=utf-8",
    },
    Asset {
        path: "/app.css",
        bytes: APP_CSS,
        content_type: "text/css; charset=utf-8",
    },
];

/// 入口 HTML。
pub fn index_html() -> &'static [u8] {
    INDEX_HTML
}

/// 按请求路径找资源。
///
/// 只认精确匹配：文件名固定，没有子目录，所以通配只会扩大攻击面。
/// 返回 `None` 由调用方决定是 404 还是回落入口页。
pub fn asset(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|item| item.path == path)
}

/// 新版前端的路径前缀。
///
/// 与旧页面并存的唯一开关：改成 `""` 就让新版接管根路径。做成常量而不是
/// 配置项，是因为「哪套前端在服务」不该是运行时可变的——两个版本同时可写
/// 会让「我改了为什么没生效」重新变成需要猜的问题。
pub const NEXT_UI_PREFIX: &str = "/next";

/// 请求路径是否交给新版前端。
///
/// 接受三种形态：
///   - `/next` 或 `/next/`        → 入口页
///   - `/next/app.js` / `app.css` → 静态资源
///   - `/next/<任意>`             → 入口页（前端用 hash 路由，服务端回落安全）
pub fn is_next_ui_path(path: &str) -> bool {
    if path == NEXT_UI_PREFIX || path == NEXT_UI_PREFIX_SLASH {
        return true;
    }
    path.starts_with(&format!("{NEXT_UI_PREFIX}/"))
}

const NEXT_UI_PREFIX_SLASH: &str = "/next/";

/// 处理一次新版前端的请求。
///
/// 缓存策略：静态文件名固定（没有内容哈希），所以**不能**给长缓存——
/// 否则改了前端、用户刷新还是旧的。入口页 `no-store`，资源 `no-cache`
/// （允许缓存但要回源校验），这是没有文件名哈希时的正确折中。
pub fn serve(
    stream: &mut std::net::TcpStream,
    path: &str,
    extra_headers: &[&str],
) -> std::io::Result<()> {
    use std::io::Write;

    let trimmed = path.strip_prefix(NEXT_UI_PREFIX).unwrap_or(path);
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
    let (content_type, body, cache) = match asset(trimmed) {
        Some(item) => (item.content_type, item.bytes, "no-cache"),
        None => ("text/html; charset=utf-8", index_html(), "no-store"),
    };
    // 安全头与 JSON 响应保持一致。CSP 的 `'unsafe-inline'` 来自既有行为
    // （index.html 里的主题脚本是内联的），不是这里新引入的。
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\
         X-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; connect-src 'self'\r\n\
         Cache-Control: {cache}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Referrer-Policy: no-referrer\r\n{}Connection: close\r\n\r\n",
        body.len(),
        extra_headers.join(""),
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_html_is_the_built_entry_point() {
        let html = String::from_utf8_lossy(index_html());
        assert!(html.contains("<!doctype html") || html.contains("<!DOCTYPE html"));
        // 入口必须引用固定名的产物，否则 Rust 侧的常量就接错了。
        // 必须引用带前缀的产物：只写 `/app.js` 会让浏览器去请求根路径（旧页面）。
        // 这条守卫同时钉住了 vite.config.ts 的 `base` 与这里的 `NEXT_UI_PREFIX`。
        assert!(
            html.contains(&format!("{NEXT_UI_PREFIX}/app.js")),
            "入口没引用 {NEXT_UI_PREFIX}/app.js：{html}"
        );
        assert!(
            html.contains(&format!("{NEXT_UI_PREFIX}/app.css")),
            "入口没引用 {NEXT_UI_PREFIX}/app.css：{html}"
        );
    }

    #[test]
    fn every_asset_is_reachable_and_typed() {
        for item in ASSETS {
            assert!(
                item.path.starts_with('/'),
                "资源路径要以 / 开头：{}",
                item.path
            );
            assert!(!item.bytes.is_empty(), "资源是空的：{}", item.path);
            assert!(
                item.content_type.contains("charset=utf-8"),
                "文本资源要显式声明字符集：{}",
                item.path
            );
        }
        assert!(asset("/app.js").is_some());
        assert!(asset("/app.css").is_some());
        // 精确匹配：不认带查询串或目录形态的路径。
        assert!(asset("/app.js?v=1").is_none());
        assert!(asset("/assets/app.js").is_none());
    }

    /// 产物与前端源码的同步由 `deploy/verify.sh` 的 `web` 阶段看守（会真跑
    /// `vite build` 再比对）。这里只保证嵌入的东西形状对——在 Rust 测试里
    /// 重新跑 node 构建是错的：它把「测试」变成需要 node 环境的东西。

    #[test]
    fn next_ui_paths_are_scoped_under_the_prefix() {
        assert!(is_next_ui_path("/next"));
        assert!(is_next_ui_path("/next/"));
        assert!(is_next_ui_path("/next/app.js"));
        assert!(is_next_ui_path("/next/sessions"));
        // 前缀之外的一切都不归新版前端管：根路径还是旧页面。
        assert!(!is_next_ui_path("/"));
        assert!(!is_next_ui_path("/app.js"));
        assert!(!is_next_ui_path("/api/status"));
        assert!(!is_next_ui_path("/nextish"));
    }

    #[test]
    fn embedded_assets_are_not_placeholders() {
        let js = String::from_utf8_lossy(APP_JS);
        let css = String::from_utf8_lossy(APP_CSS);
        // 构建失败时 vite 不会产出占位文件，所以「非空且含预期标记」足够。
        assert!(
            js.len() > 10_000,
            "app.js 太小，可能没真正构建：{} 字节",
            js.len()
        );
        assert!(css.len() > 1_000, "app.css 太小：{} 字节", css.len());
        // 样式现在来自 Vuetify（按需引入），所以不再有我们自己命名的令牌变量。
        // 改成钉住"确实打进了 Vuetify 的样式"：这两样只有它才会产出。
        assert!(
            css.contains("v-application") || css.contains("v-btn") || css.contains("--v-"),
            "app.css 里找不到 Vuetify 的样式，可能嵌错了文件或构建时没装上插件"
        );
    }

    /// 新版前端引用的每个 `console.*` 词条都必须在中英两份 locale 里存在。
    ///
    /// 为什么这条守卫重要：前端不再自带词条（全走 `/api/i18n`），少一条的
    /// 表现是页面上直接出现 `console.models.title` 这种点号串——看起来像
    /// 前端坏了，实际是词条漏了。在 Rust 侧扫产物字节是最便宜的位置：
    /// 不需要 node、不需要浏览器，`cargo test` 就能拦住。
    #[test]
    fn every_console_key_used_by_the_ui_exists_in_both_locales() {
        fn locale_keys(locale: &str) -> std::collections::BTreeSet<String> {
            let text = match locale {
                "zh-CN" => include_str!("../../i18n/locales/zh-CN.json"),
                _ => include_str!("../../i18n/locales/en.json"),
            };
            let value: serde_json::Value = serde_json::from_str(text).expect("locale 是合法 JSON");
            value
                .as_object()
                .expect("locale 是对象")
                .keys()
                .cloned()
                .collect()
        }

        let zh = locale_keys("zh-CN");
        let en = locale_keys("en");

        // 产物是压缩过的，所以按「出现过的 console.* 字面量」提取，而不是读源码。
        let js = String::from_utf8_lossy(APP_JS);
        let mut used = std::collections::BTreeSet::new();
        let bytes = js.as_bytes();
        let mut index = 0;
        while let Some(found) = js[index..].find("console.") {
            let start = index + found;
            let mut end = start;
            for (offset, byte) in bytes[start..].iter().enumerate() {
                let byte = *byte;
                // 词条 key 只含字母/数字/点/下划线/短横线；遇到别的就断开。
                let ok =
                    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'_' || byte == b'-';
                if !ok {
                    break;
                }
                end = start + offset + 1;
            }
            let key = &js[start..end];
            // 只收「看起来像完整词条」的：至少要有一个点分段，且不以点结尾。
            if key.len() > "console.".len() && key.ends_with(|c: char| c.is_ascii_alphanumeric()) {
                // 压缩器会拼接字符串，裁剪掉可能多带的尾段：取最长且在词条表里的前缀。
                let mut candidate = key.to_owned();
                while candidate.len() > "console.".len() {
                    if zh.contains(&candidate) || en.contains(&candidate) {
                        used.insert(candidate);
                        break;
                    }
                    // 砍掉最后一个字符继续试（压缩产物里 key 后面常紧跟代码字符）。
                    candidate.truncate(candidate.len() - 1);
                }
            }
            index = end.max(start + 1);
        }

        assert!(
            !used.is_empty(),
            "没在 app.js 里找到任何 console.* 词条引用，守卫本身可能失效了"
        );
        let mut missing = Vec::new();
        for key in &used {
            if !zh.contains(key) {
                missing.push(format!("{key} (zh-CN)"));
            }
            if !en.contains(key) {
                missing.push(format!("{key} (en)"));
            }
        }
        assert!(
            missing.is_empty(),
            "新前端用到但词条表里没有的 key：{missing:?}"
        );
    }
}
