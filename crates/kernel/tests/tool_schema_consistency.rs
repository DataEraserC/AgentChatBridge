//! 跨层一致性：harness 的工具 schema 与内核工具清单、票据词表必须是同一份事实。
//!
//! 这里只做轻量文本解析，不引入 JS 运行时；失败信息直接列出两侧差集。

use std::collections::BTreeSet;
use std::path::PathBuf;

use acb_kernel::TOOL_NAMES;
use acb_kernel::engine::TICKET_OPS;
use acb_kernel::{ON_DEMAND_TOOLS, RESIDENT_TOOLS};

fn tools_mjs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("harnesses/shared/tools.mjs");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

/// 顶层工具条目固定是 4 空格缩进的 `name: "..."`；嵌套 schema 里的 `name:` 不是字符串。
fn js_tool_names(source: &str) -> BTreeSet<String> {
    source
        .lines()
        .filter_map(|line| {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim_start();
            (indent == 4)
                .then(|| trimmed.strip_prefix("name: \""))
                .flatten()
                .and_then(|rest| rest.strip_suffix("\","))
                .map(str::to_owned)
        })
        .collect()
}

/// 解析 `export const RESIDENT_TOOL_NAMES = [...]` 里的字符串字面量。
fn js_resident_names(source: &str) -> BTreeSet<String> {
    js_array_names(source, "RESIDENT_TOOL_NAMES")
}

/// 解析 `export const ON_DEMAND_TOOL_NAMES = [...]` 里的字符串字面量。
fn js_on_demand_names(source: &str) -> BTreeSet<String> {
    js_array_names(source, "ON_DEMAND_TOOL_NAMES")
}

fn js_array_names(source: &str, export: &str) -> BTreeSet<String> {
    let marker = format!("export const {export} = [");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("tools.mjs 里没有 {export}"));
    let block = source[start..]
        .split(']')
        .next()
        .unwrap_or_else(|| panic!("{export} 有闭合括号"));
    block
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix("\","))
                .map(str::to_owned)
        })
        .collect()
}

fn access_request_ops(source: &str) -> BTreeSet<String> {
    let start = source
        .find("name: \"access_request\"")
        .expect("tools.mjs 里没有 access_request");
    let block = &source[start..];
    let block = block
        .split("schema:")
        .next()
        .expect("access_request 有描述");
    let text = block
        .split("op 列表要具体：")
        .nth(1)
        .expect("access_request 描述里要有 op 词表");
    let text = text.split("批量最多").next().unwrap_or(text);
    // `platform_call:<op>` 是模式，不是具体 op；它的例子也一并切掉。
    let text = text.split("或 platform_call").next().unwrap_or(text);
    text.split(|ch: char| ch.is_whitespace() || matches!(ch, '，' | '、' | '：' | '；' | '`'))
        .filter(|token| {
            !token.is_empty()
                && token
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_lowercase())
                && token
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | ':'))
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn tool_names_match_the_shared_schema() {
    let js: BTreeSet<String> = js_tool_names(&tools_mjs());
    let kernel: BTreeSet<String> = TOOL_NAMES.iter().map(|name| (*name).to_owned()).collect();

    let only_js: Vec<&String> = js.difference(&kernel).collect();
    let only_kernel: Vec<&String> = kernel.difference(&js).collect();
    assert!(
        only_js.is_empty() && only_kernel.is_empty(),
        "工具清单漂移：\n  harness TOOLS 有、内核 TOOL_NAMES 没有：{only_js:?}\n  \
         内核 TOOL_NAMES 有、harness TOOLS 没有：{only_kernel:?}"
    );
}

#[test]
fn resident_names_match_the_shared_schema() {
    let js = js_resident_names(&tools_mjs());
    let kernel: BTreeSet<String> = RESIDENT_TOOLS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();

    let only_js: Vec<&String> = js.difference(&kernel).collect();
    let only_kernel: Vec<&String> = kernel.difference(&js).collect();
    assert!(
        only_js.is_empty() && only_kernel.is_empty(),
        "常驻白名单漂移：\n  harness RESIDENT_TOOL_NAMES 有、内核没有：{only_js:?}\n  \
         内核 RESIDENT_TOOLS 有、harness 没有：{only_kernel:?}"
    );
    assert_eq!(kernel.len(), 8, "常驻集合固定 8 个");
}

#[test]
fn on_demand_names_match_the_shared_schema() {
    let js = js_on_demand_names(&tools_mjs());
    let kernel: BTreeSet<String> = ON_DEMAND_TOOLS
        .iter()
        .map(|name| (*name).to_owned())
        .collect();

    let only_js: Vec<&String> = js.difference(&kernel).collect();
    let only_kernel: Vec<&String> = kernel.difference(&js).collect();
    assert!(
        only_js.is_empty() && only_kernel.is_empty(),
        "按需注入清单漂移：\n  harness ON_DEMAND_TOOL_NAMES 有、内核没有：{only_js:?}\n  \
         内核 ON_DEMAND_TOOLS 有、harness 没有：{only_kernel:?}"
    );
    assert_eq!(kernel.len(), 2, "按需集合固定 2 个");
}

/// issue 工具只走 process-tools 插件面，不占 core 的工具位。
#[test]
fn issue_tools_stay_off_the_core_tool_surface() {
    const ISSUE_TOOLS: [&str; 8] = [
        "issue_create",
        "issue_get",
        "issue_search",
        "issue_update",
        "issue_subscribe",
        "issue_unsubscribe",
        "issue_report_result",
        "issue_my_work",
    ];

    assert_eq!(TOOL_NAMES.len(), 33, "core 工具数固定 33（含管理面 4 个）");
    let js = js_tool_names(&tools_mjs());
    assert_eq!(js.len(), 33, "harness 共享工具面也要是 33");

    for name in ISSUE_TOOLS {
        assert!(
            !TOOL_NAMES.contains(&name),
            "{name} 不能进 core TOOL_NAMES（会挤占 core 工具位）"
        );
        assert!(!RESIDENT_TOOLS.contains(&name), "{name} 不能进常驻层");
        assert!(!ON_DEMAND_TOOLS.contains(&name), "{name} 不能进按需层");
        assert!(!js.contains(name), "{name} 只能由 plugin_tools 发现");
    }
}

#[test]
fn access_request_description_lists_exactly_the_kernel_ticket_ops() {
    let described = access_request_ops(&tools_mjs());
    let kernel: BTreeSet<String> = TICKET_OPS
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect();

    let only_description: Vec<&String> = described.difference(&kernel).collect();
    let only_kernel: Vec<&String> = kernel.difference(&described).collect();
    assert!(
        only_description.is_empty() && only_kernel.is_empty(),
        "票据 op 词表漂移：\n  access_request 描述里宣传、内核不接受：{only_description:?}\n  \
         内核接受、access_request 描述没提：{only_kernel:?}"
    );
}
