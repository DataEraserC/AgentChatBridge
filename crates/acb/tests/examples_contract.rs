//! examples/deploy 的静态约束：示例不内嵌 unit，也不导出白名单外的变量。

use std::fs;
use std::path::Path;

#[test]
fn deploy_examples_do_not_embed_units_or_export_non_whitelist_values() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/deploy");
    let mut checked = 0;
    for entry in fs::read_dir(&dir).expect("read examples/deploy") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sh") {
            continue;
        }
        checked += 1;
        let text = fs::read_to_string(&path).expect("read example");
        for literal in ["[Unit]", "[Service]", "ExecStart="] {
            assert!(
                !text.contains(literal),
                "{} must not contain {literal}",
                path.display()
            );
        }
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(exported) = trimmed.strip_prefix("export ") {
                assert!(
                    exported.starts_with("ACB_UNIT_"),
                    "{} exports non-whitelist value: {trimmed}",
                    path.display()
                );
            }
        }
    }
    assert!(checked >= 4, "expected the deploy examples to be present");
}
