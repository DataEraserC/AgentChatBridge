//! P2.5：库内密钥快照顶替文件来源。
//!
//! 这条测试守的是「删掉 `/run/agenix/acb.env`、删掉 session-tokens.env 之后还能启动」：
//! 只要库里有这个名字，`Config::load` 就不该再去读那个文件。

use std::path::PathBuf;

use acb_kernel::Config;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "acb-secrets-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn config_text(missing: &str) -> String {
    format!(
        r#"
[storage]
path = "state/acb.db"

[console]
listen = "127.0.0.1:3100"
token = {{ secret = "console-token" }}

[secrets]
[[secrets.sources]]
name = "console-token"
kind = "file"
path = "{missing}"
format = "raw"
"#
    )
}

#[test]
fn preset_replaces_missing_file_source() {
    let dir = temp_dir("preset");
    let missing = dir.join("definitely-absent.token");
    let config = dir.join("config.toml");
    std::fs::write(&config, config_text(&missing.display().to_string())).expect("write config");

    // 没有快照：文件不存在 → 加载失败。这是 fail-closed 的基线。
    let without = Config::load(&config, None);
    assert!(without.is_err(), "缺文件且无快照时必须失败");

    // 装快照后同一条配置能加载，且值来自快照。
    let mut preset = acb_kernel::secrets_db::SecretPreset::new();
    preset.insert("console-token".to_owned(), "from-database".to_owned());
    acb_kernel::secrets_db::install(preset);

    let loaded = Config::load(&config, None).expect("库内有值时应能加载");
    let console = loaded.config.console.expect("console 配置");
    assert_eq!(console.token.as_str(), "from-database");

    std::fs::remove_dir_all(&dir).ok();
}
