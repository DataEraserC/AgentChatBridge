//! 示例插件的 manifest 必须一直合法——它是文档，也是验收脚本的一半。

use std::path::PathBuf;

use acb_kernel::plugin_manifest::PluginManifest;

fn example_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("plugin-demo")
}

#[test]
fn example_manifest_is_valid() {
    let path = example_dir().join("manifest.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let manifest = PluginManifest::parse(&raw).expect("示例 manifest 必须合法");
    assert_eq!(manifest.id.as_deref(), Some("demo.tools"));
    assert!(manifest.config_field("limit").is_some());
    assert!(manifest.config_field("apiToken").expect("有").secret);
    assert!(manifest.resource("notes").is_some());
    assert!(manifest.action("ping").is_some());
}

#[test]
fn example_plugin_script_exists() {
    let path = example_dir().join("plugin.py");
    assert!(path.is_file(), "{} 应该存在", path.display());
}
