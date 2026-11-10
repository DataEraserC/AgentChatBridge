//! 外置进程工具的 loader：把配置里的 `[[tool_plugins]]` 注册成 Tool provider。
//!
//! 只承载第三方工具；内置工具仍由 `ToolLoader` 注册，热路径不变。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use acb_kernel::{
    Capability, ExtensionPoint, LoaderKind, LoaderManifest, PLUGIN_API_VERSION, PluginActionRunner,
    PluginError, PluginLoader, PluginManager, PluginMetadata, PluginSource, ProviderDescriptor,
    ProviderId, ToolPluginConfig, ToolProvider,
};

use crate::provider::ProcessToolProvider;

pub struct ProcessToolLoader {
    configs: Vec<ToolPluginConfig>,
    providers: Mutex<HashMap<ProviderId, Arc<ProcessToolProvider>>>,
}

impl ProcessToolLoader {
    pub fn new(configs: &[ToolPluginConfig]) -> Self {
        Self {
            configs: configs.to_vec(),
            providers: Mutex::new(HashMap::new()),
        }
    }

    /// 动作执行器列表。控制台按 plugin id 找；没起来的插件不会有。
    pub fn action_runners(&self) -> Vec<Arc<dyn PluginActionRunner>> {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .map(|(id, provider)| {
                Arc::new(crate::action::ProcessActionRunner::new(
                    id.to_string(),
                    Arc::clone(provider),
                )) as Arc<dyn PluginActionRunner>
            })
            .collect()
    }

    /// 按 id 取已启动的 provider（控制台/诊断用）。
    pub fn provider(&self, id: &ProviderId) -> Option<Arc<ProcessToolProvider>> {
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
    }
}

impl PluginLoader for ProcessToolLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Process,
            PLUGIN_API_VERSION,
            vec![ExtensionPoint::Tool],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        let mut installed: Vec<ProviderId> = Vec::new();
        for config in &self.configs {
            let id = ProviderId::parse(&config.id).map_err(|error| {
                PluginError::Loader(format!("invalid tool plugin id {}: {error}", config.id))
            })?;
            let provider =
                Arc::new(ProcessToolProvider::spawn(config.clone()).map_err(|error| {
                    PluginError::Loader(format!(
                        "tool plugin {} failed to start: {error}",
                        config.id
                    ))
                })?);
            let summary = provider
                .tools()
                .first()
                .map(|tool| tool.summary.clone())
                .unwrap_or_else(|| "external process tools".to_owned());
            let descriptor = ProviderDescriptor::new(
                id.clone(),
                ExtensionPoint::Tool,
                PLUGIN_API_VERSION,
                vec![Capability::ToolCall],
            )
            .with_source(PluginSource::Dynamic)
            .with_metadata(PluginMetadata {
                title: Some(config.id.clone().into()),
                summary: Some(summary.into()),
                ..PluginMetadata::EMPTY
            });
            if let Err(error) = manager.register_tools(descriptor, provider.clone()) {
                // 安装是批量的：一个冲突不能让前面几个留下半套注册。
                for installed_id in installed {
                    let _ = manager.remove_tool(&installed_id);
                }
                return Err(error);
            }
            self.providers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), provider);
            installed.push(id);
        }
        Ok(())
    }
}
