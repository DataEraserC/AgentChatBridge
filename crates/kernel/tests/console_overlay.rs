//! 控制台覆盖层不能回灌 base：`[secrets]` 只属于 base/普通分片。
//!
//! 2026-11-10 现场：没有 `90-console.toml` 时，控制台把 `config.toml` 全文当覆盖层
//! 送去校验，`[secrets]` 一读就报「must not declare [secrets]」，于是页面上任何一次
//! 保存都失败。这条测试钉住「空覆盖层是合法的」，同时保留「覆盖层里写 [secrets] 要报错」。

use std::path::PathBuf;

use acb_kernel::Config;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "acb-overlay-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn write_base(dir: &std::path::Path) -> PathBuf {
    let token_file = dir.join("console.token");
    std::fs::write(&token_file, "secret-value\n").expect("write token");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    let config = dir.join("config.toml");
    let text = format!(
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
path = "{path}"
format = "raw"
"#,
        path = token_file.display()
    );
    std::fs::write(&config, text).expect("write config");
    config
}

#[test]
fn empty_overlay_merges_over_base_with_secrets() {
    let dir = temp_dir("empty");
    let config = write_base(&dir);
    let config_dir = dir.join("config.d");
    std::fs::create_dir_all(&config_dir).expect("mkdir config.d");

    let loaded = Config::load_with_console_overlay(&config, &config_dir, "", None)
        .expect("空覆盖层应当能加载");
    assert_eq!(
        loaded.config.console.expect("console").token.as_str(),
        "secret-value"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn overlay_with_secrets_is_still_rejected() {
    let dir = temp_dir("secrets");
    let config = write_base(&dir);
    let config_dir = dir.join("config.d");
    std::fs::create_dir_all(&config_dir).expect("mkdir config.d");

    let overlay =
        "[secrets]\n[[secrets.sources]]\nname = \"x\"\nkind = \"env\"\nvariable = \"X\"\n";
    let error = Config::load_with_console_overlay(&config, &config_dir, overlay, None)
        .expect_err("覆盖层写 [secrets] 必须被拒");
    assert!(
        format!("{error}").contains("secrets"),
        "错误信息要指向 [secrets]：{error}"
    );

    std::fs::remove_dir_all(&dir).ok();
}
