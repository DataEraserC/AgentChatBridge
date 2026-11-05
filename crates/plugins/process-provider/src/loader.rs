//! 外置进程策略 provider 的 loader：把 `[[process_providers]]` 装进 Root。
//!
//! 当前最小面只支持 `access`。载体本身不认识 wasm：它只启动配置里的命令，
//! 通过 stdio 行协议把 `decide` 转发过去。

use std::sync::Arc;

use acb_kernel::{
    ExtensionPoint, LoaderKind, LoaderManifest, PLUGIN_API_VERSION, PluginError, PluginLoader,
    PluginManager, PluginMetadata, ProviderId,
};

use crate::channel::ProcessChannel;
use crate::config::ProcessProviderConfig;
use crate::provider::ProcessAccessManager;

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
        LoaderManifest::new(
            LoaderKind::Process,
            PLUGIN_API_VERSION,
            vec![ExtensionPoint::Access],
        )
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
            let access = Arc::new(ProcessAccessManager::new(id.clone(), metadata, channel));
            manager
                .install_manager(access)
                .map_err(|error| PluginError::Loader(error.to_string()))?;
        }
        Ok(())
    }
}
