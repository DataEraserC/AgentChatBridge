//! `[[process_providers]]` 配置：一个外置进程承载一个策略 provider。

use std::collections::BTreeMap;

use acb_kernel::ProviderId;
use serde::Deserialize;

/// 一次外置进程 provider 的声明。
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProcessProviderConfig {
    /// provider id，如 `demo.access_allowlist`。namespace 不能是 `acb`。
    pub id: String,
    /// 扩展点。当前参考实现只支持 `access`。
    #[serde(default = "default_extension")]
    pub extension: String,
    /// 启动命令：第一个元素是程序，其余是参数。
    pub command: Vec<String>,
    /// 子进程环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 子进程工作目录。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 等第一行 manifest 的时间。
    #[serde(default = "default_startup_timeout")]
    pub startup_timeout_secs: u64,
    /// 单次 decide 的等待时间。
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_extension() -> String {
    "access".to_owned()
}

pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_TIMEOUT_MS: u64 = 5000;
pub const SUPPORTED_EXTENSION: &str = "access";

fn default_startup_timeout() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_SECS
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

impl ProcessProviderConfig {
    /// 从整份 config.toml 里读 `[[process_providers]]`；没有就返回空。
    pub fn from_toml(text: &str) -> Result<Vec<Self>, String> {
        #[derive(Deserialize)]
        struct Root {
            #[serde(default)]
            process_providers: Vec<ProcessProviderConfig>,
        }
        let root: Root =
            toml::from_str(text).map_err(|error| format!("process_providers config: {error}"))?;
        let mut namespaces = std::collections::BTreeSet::new();
        for config in &root.process_providers {
            config.validate()?;
            let id = ProviderId::parse(&config.id).expect("validate already checked the id");
            if !namespaces.insert(id.namespace().to_owned()) {
                return Err(format!(
                    "process provider {}: namespace `{}` already has a process provider; \
                     one process provider per namespace",
                    config.id,
                    id.namespace()
                ));
            }
        }
        Ok(root.process_providers)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.command.is_empty() || self.command[0].trim().is_empty() {
            return Err(format!(
                "process provider {}: command must not be empty",
                self.id
            ));
        }
        if self.extension != SUPPORTED_EXTENSION {
            return Err(format!(
                "process provider {}: unsupported extension {} (only {SUPPORTED_EXTENSION})",
                self.id, self.extension
            ));
        }
        let id = ProviderId::parse(&self.id)
            .map_err(|error| format!("process provider id {}: {error}", self.id))?;
        if id.namespace() == "acb" {
            return Err(format!(
                "process provider {}: namespace `acb` is reserved",
                self.id
            ));
        }
        if self.startup_timeout_secs == 0 {
            return Err(format!(
                "process provider {}: startup_timeout_secs must be > 0",
                self.id
            ));
        }
        if self.timeout_ms == 0 {
            return Err(format!(
                "process provider {}: timeout_ms must be > 0",
                self.id
            ));
        }
        Ok(())
    }

    pub fn startup_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(if self.startup_timeout_secs == 0 {
            DEFAULT_STARTUP_TIMEOUT_SECS
        } else {
            self.startup_timeout_secs
        })
    }

    pub fn call_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(if self.timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            self.timeout_ms
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_process_provider_table() {
        let configs = ProcessProviderConfig::from_toml(
            r#"
            [[process_providers]]
            id = "demo.access_allowlist"
            extension = "access"
            command = ["acb-wasm-host", "--module", "access.wasm"]
            env = { RUST_BACKTRACE = "0" }
            startup_timeout_secs = 15
            timeout_ms = 2500
            "#,
        )
        .expect("config parses");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id, "demo.access_allowlist");
        assert_eq!(configs[0].command.len(), 3);
        assert_eq!(configs[0].startup_timeout_secs, 15);
    }

    #[test]
    fn rejects_unknown_extension_and_acb_namespace() {
        let error = ProcessProviderConfig::from_toml(
            r#"
            [[process_providers]]
            id = "demo.wake"
            extension = "wake"
            command = ["echo"]
            "#,
        )
        .expect_err("unsupported extension");
        assert!(error.contains("unsupported extension"), "{error}");

        let error = ProcessProviderConfig::from_toml(
            r#"
            [[process_providers]]
            id = "acb.access"
            extension = "access"
            command = ["echo"]
            "#,
        )
        .expect_err("reserved namespace");
        assert!(error.contains("reserved"), "{error}");
    }

    #[test]
    fn empty_config_is_allowed() {
        assert!(
            ProcessProviderConfig::from_toml("")
                .expect("empty")
                .is_empty()
        );
    }

    #[test]
    fn rejects_duplicate_namespaces() {
        let error = ProcessProviderConfig::from_toml(
            r#"
            [[process_providers]]
            id = "demo.one"
            extension = "access"
            command = ["echo"]

            [[process_providers]]
            id = "demo.two"
            extension = "access"
            command = ["echo"]
            "#,
        )
        .expect_err("one process provider per namespace");
        assert!(error.contains("namespace"), "{error}");
    }
}
