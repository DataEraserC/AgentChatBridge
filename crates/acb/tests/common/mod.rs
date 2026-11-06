//! 集成测试共用：生成不依赖 `/usr/bin/env`、`/bin/sh` 的可执行脚本。
//! 解释器路径从 `BASH` 环境变量或 PATH 里的 `bash` 解析，适配 Nix 构建沙箱。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn bash_path() -> PathBuf {
    static BASH: OnceLock<PathBuf> = OnceLock::new();
    BASH.get_or_init(|| {
        if let Some(path) = std::env::var_os("BASH") {
            let candidate = PathBuf::from(path);
            if is_executable(&candidate) {
                return candidate;
            }
        }
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                let candidate = dir.join("bash");
                if is_executable(&candidate) {
                    return candidate;
                }
            }
        }
        panic!("could not find bash via BASH or PATH; cannot create test shims");
    })
    .clone()
}

pub fn write_script(path: &Path, body: &str) {
    let bash = bash_path();
    let content = format!("#!{}\n{}", bash.display(), body.trim_start_matches('\n'));
    fs::write(path, content).expect("write test script");
    let mut permissions = fs::metadata(path).expect("metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("chmod test script");
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
