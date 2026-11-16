//! 外置进程策略 provider 的 loader：把 `[[process_providers]]` 装进 Root。
//!
//! 当前最小面只支持 `access`。载体本身不认识 wasm：它只启动配置里的命令，
//! 通过 stdio 行协议把 `decide` 转发过去。

use std::sync::Arc;

use acb_kernel::{
    ExtensionPoint, LoaderKind, LoaderManifest, ManagerPlugin, PLUGIN_API_VERSION, PluginError,
    PluginLoader, PluginManager, PluginMetadata, ProviderId,
};

use crate::channel::ProcessChannel;
use crate::config::ProcessProviderConfig;
use crate::provider::{
    ProcessAccessManager, ProcessClockManager, ProcessRouteManager, ProcessTurnErrorManager,
    ProcessWakeManager,
};

pub struct ProcessProviderLoader {
    configs: Vec<ProcessProviderConfig>,
}

impl ProcessProviderLoader {
    pub fn new(configs: &[ProcessProviderConfig]) -> Self {
        Self {
            configs: configs.to_vec(),
        }
    }
}

impl PluginLoader for ProcessProviderLoader {
    fn manifest(&self) -> LoaderManifest {
        // 声明本 loader 能提供的扩展点：按配置里出现过的去重。
        // 写死成 `vec![Access]` 会让 describe_plugins 永远显示「只有 access」，
        // 与实际能装的东西不符（ADR-0033）。
        let mut points: Vec<ExtensionPoint> = Vec::new();
        for config in &self.configs {
            let point = match config.extension.as_str() {
                "wake" => ExtensionPoint::Wake,
                "clock" => ExtensionPoint::Clock,
                "route" => ExtensionPoint::Route,
                "turn_error" => ExtensionPoint::TurnError,
                _ => ExtensionPoint::Access,
            };
            if !points.contains(&point) {
                points.push(point);
            }
        }
        if points.is_empty() {
            points.push(ExtensionPoint::Access);
        }
        LoaderManifest::new(LoaderKind::Process, PLUGIN_API_VERSION, points)
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        for config in &self.configs {
            let id = ProviderId::parse(&config.id).map_err(|error| {
                PluginError::Loader(format!(
                    "invalid process provider id {}: {error}",
                    config.id
                ))
            })?;
            let (channel, manifest) = ProcessChannel::spawn(config).map_err(|detail| {
                PluginError::Loader(format!("process provider {} failed: {detail}", config.id))
            })?;
            if manifest.provider != config.id {
                channel.shutdown();
                return Err(PluginError::Loader(format!(
                    "process provider {} reported provider {}",
                    config.id, manifest.provider
                )));
            }
            let metadata = PluginMetadata::try_from(manifest.metadata).map_err(|detail| {
                channel.shutdown();
                PluginError::Loader(format!(
                    "process provider {} has invalid metadata: {detail}",
                    config.id
                ))
            })?;
            if metadata
                .summary
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
            {
                channel.shutdown();
                return Err(PluginError::Loader(format!(
                    "process provider {} must declare a non-empty metadata.summary",
                    config.id
                )));
            }
            // 按配置的扩展点选 manager。同一个子进程承载一种策略，
            // 这也是 `ProcessProviderConfig` 里 `extension` 是单值而非列表的原因。
            let installed: Arc<dyn ManagerPlugin> = match config.extension.as_str() {
                "wake" => Arc::new(ProcessWakeManager::new(id.clone(), metadata, channel)),
                "clock" => Arc::new(ProcessClockManager::new(id.clone(), metadata, channel)),
                "route" => Arc::new(ProcessRouteManager::new(id.clone(), metadata, channel)),
                "turn_error" => {
                    Arc::new(ProcessTurnErrorManager::new(id.clone(), metadata, channel))
                }
                _ => Arc::new(ProcessAccessManager::new(id.clone(), metadata, channel)),
            };
            manager
                .install_manager(installed)
                .map_err(|error| PluginError::Loader(error.to_string()))?;
        }
        Ok(())
    }
}
