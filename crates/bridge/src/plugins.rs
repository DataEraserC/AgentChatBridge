//! 插件清单的启动对齐：文件 → 库（首次 seed）或 库 → 运行时（之后每次启动）。
//!
//! 规矩和五类业务对象一致：**库是唯一源**。`[[tool_plugins]]` 只在库为空时被
//! 抄一次；之后改文件不生效，只在日志里说清楚「这条已经不算数」。
//!
//! 命令能不能跑是运行时事实，不是配置事实：nix store 路径会被 GC，profile 包会被
//! 换掉。所以这里只做「检查 + 标记 degraded」，不因为一条命令不可执行就拒绝启动
//! ——那会把「插件坏了」升级成「桥起不来」。

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::plugin_registry::{
    CommandKind, CommandStatus, PluginKind, PluginPolicyConfig, PluginStorageConfig,
    RegistryPlugin, ScopeKind, check_command, command_fingerprint,
};
use acb_kernel::storage::PluginsStore;

/// 一条插件的运行状态：能不能装、为什么不能装。
#[derive(Debug, Clone)]
pub struct PluginRuntime {
    pub plugin: RegistryPlugin,
    pub status: CommandStatus,
    pub fingerprint_ok: bool,
}

impl PluginRuntime {
    /// 只有启用、命令可执行、指纹对得上才装进 PluginManager。
    pub fn callable(&self) -> bool {
        self.plugin.enabled && self.status.is_ok() && self.fingerprint_ok
    }

    pub fn reason(&self) -> Option<String> {
        if !self.plugin.enabled {
            return Some("disabled".to_owned());
        }
        if !self.fingerprint_ok {
            return Some("command fingerprint drift".to_owned());
        }
        if !self.status.is_ok() {
            return Some(self.status.describe());
        }
        None
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 把配置里的 `[[tool_plugins]]` 变成库记录。只在首次 seed 用。
fn seed_from_config(
    configs: &[acb_kernel::ToolPluginConfig],
    config_path: &Path,
) -> Vec<RegistryPlugin> {
    let at = now_ms();
    configs
        .iter()
        .map(|config| RegistryPlugin {
            id: config.id.clone(),
            kind: PluginKind::Tool,
            trust_tier: "tool".to_owned(),
            command: config.command.clone(),
            args: config.args.clone(),
            timeout_ms: config.timeout(),
            command_kind: CommandKind::infer(&config.command),
            command_fingerprint: Some(command_fingerprint(&config.command, &config.args)),
            manifest_json: "{}".to_owned(),
            schema_version: 0,
            source_seed: Some(config_path.display().to_string()),
            enabled: true,
            created_at: at,
            updated_at: at,
            version: 1,
            policy: PluginPolicyConfig {
                enabled: true,
                scope_kind: ScopeKind::Global,
                scope_id: None,
                capabilities: vec!["tool_call".to_owned()],
                rate_limit: None,
                lifecycle: acb_kernel::plugin_registry::Lifecycle::Resident,
                allow_unknown: false,
            },
            storage: PluginStorageConfig::default(),
            capabilities: Vec::new(),
        })
        .collect()
}

/// 对齐并返回运行状态。`write=false`（`--check`）只读不写。
pub fn sync<S: PluginsStore>(
    store: &mut S,
    configs: &[acb_kernel::ToolPluginConfig],
    config_path: &Path,
    config_dir: &Path,
    write: bool,
) -> Result<Vec<PluginRuntime>, Box<dyn std::error::Error>> {
    let plugins = match store.plugins_load()? {
        None => {
            let seeded = seed_from_config(configs, config_path);
            if write && !seeded.is_empty() {
                store.plugins_replace(&seeded, now_ms())?;
                eprintln!(
                    "plugin registry: seeded {} plugin(s) from config",
                    seeded.len()
                );
            } else if write {
                // 空 seed 也要打标记：否则下次启动还会把文件里后来加的抄进来。
                store.plugins_replace(&[], now_ms())?;
            }
            seeded
        }
        Some(plugins) => {
            if !configs.is_empty() {
                eprintln!(
                    "plugin registry: {} [[tool_plugins]] entr(ies) in config are ignored; database is authoritative",
                    configs.len()
                );
            }
            plugins
        }
    };

    let runtimes = plugins
        .into_iter()
        .map(|plugin| {
            let status = check_command(&plugin.command, plugin.command_kind, Some(config_dir));
            PluginRuntime {
                fingerprint_ok: plugin.fingerprint_matches(),
                plugin,
                status,
            }
        })
        .collect();
    Ok(runtimes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::storage::MemoryStore;

    fn plugin_config() -> acb_kernel::ToolPluginConfig {
        acb_kernel::ToolPluginConfig {
            id: "issue.tools".to_owned(),
            command: "/bin/sh".to_owned(),
            args: vec![],
            timeout_ms: Some(1000),
            env: Vec::new(),
        }
    }

    fn config_path() -> std::path::PathBuf {
        std::path::PathBuf::from("/etc/acb/config.toml")
    }

    #[test]
    fn first_sync_seeds_and_second_reads_store() {
        let mut store = MemoryStore::new();
        let first = sync(
            &mut store,
            &[plugin_config()],
            &config_path(),
            Path::new("/etc/acb"),
            true,
        )
        .expect("能对齐");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].plugin.id, "issue.tools");
        assert!(first[0].fingerprint_ok);
        assert!(first[0].status.is_ok(), "/bin/sh 应该可执行");

        // 第二次：文件里换成别的插件，库应当仍然只有原来的那条。
        let second = sync(
            &mut store,
            &[acb_kernel::ToolPluginConfig {
                id: "other.tools".to_owned(),
                command: "/bin/sh".to_owned(),
                args: vec![],
                timeout_ms: None,
                env: Vec::new(),
            }],
            &config_path(),
            Path::new("/etc/acb"),
            true,
        )
        .expect("能对齐");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].plugin.id, "issue.tools");
    }

    #[test]
    fn missing_command_is_degraded_not_fatal() {
        let mut store = MemoryStore::new();
        let mut config = plugin_config();
        config.command = "/nonexistent/xyz".to_owned();
        let runtimes = sync(
            &mut store,
            &[config],
            &config_path(),
            Path::new("/etc/acb"),
            true,
        )
        .expect("缺命令不该失败");
        assert_eq!(runtimes.len(), 1);
        assert!(!runtimes[0].callable());
        assert_eq!(runtimes[0].status, CommandStatus::Missing);
        assert!(runtimes[0].reason().is_some());
    }

    #[test]
    fn read_only_sync_does_not_write() {
        let mut store = MemoryStore::new();
        let runtimes = sync(
            &mut store,
            &[plugin_config()],
            &config_path(),
            Path::new("/etc/acb"),
            false,
        )
        .expect("能只读对齐");
        assert_eq!(runtimes.len(), 1);
        assert!(
            store.plugins_load().expect("能读").is_none(),
            "--check 不能写库"
        );
    }
}
