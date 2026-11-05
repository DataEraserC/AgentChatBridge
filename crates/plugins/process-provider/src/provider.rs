//! 把进程通道包装成 Root 认识的 access provider。

use std::sync::{Arc, Mutex};

use acb_adapter_api::{WireAccessRequest, WireResult};
use acb_kernel::{
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Capability, ExtensionPoint,
    Health, ManagerManifest, ManagerPlugin, PLUGIN_API_VERSION, PluginError, PluginMetadata,
    PluginSource, ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions,
};
use serde_json::json;

use crate::channel::ProcessChannel;

pub(crate) struct ProcessAccessManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessAccessProvider>>>,
}

impl ProcessAccessManager {
    pub(crate) fn new(
        provider: ProviderId,
        metadata: PluginMetadata,
        channel: Arc<ProcessChannel>,
    ) -> Self {
        Self {
            provider,
            metadata,
            channel,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for ProcessAccessManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::AccessDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Access,
                PLUGIN_API_VERSION,
                vec![Capability::AccessDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if !matches!(options, ProviderOptions::Access(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process access provider requires Access options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessAccessProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::Access(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessAccessProvider {
    channel: Arc<ProcessChannel>,
}

impl AccessProvider for ProcessAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        let wire = WireAccessRequest {
            session: request.session.to_owned(),
            connection: request.connection.to_owned(),
            room: request.room.map(|room| room.to_string()),
            origin_room: request.origin_room.map(|room| room.to_string()),
            subject: request.subject.as_str().to_owned(),
            place: request.place.as_str().to_owned(),
            action: request.action.as_str().to_owned(),
            subscriptions: request
                .subscriptions
                .iter()
                .map(|room| room.to_string())
                .collect(),
        };
        let request = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self.channel.decide(options, request) {
            Ok(WireResult::Decision { decision, .. }) if decision == "allow" => {
                AccessDecision::Allow
            }
            Ok(WireResult::Decision { decision, .. }) if decision == "deny" => AccessDecision::Deny,
            Ok(WireResult::Decision { decision, .. }) => {
                self.channel
                    .note_invalid_reply(&format!("unknown access decision `{decision}`"));
                AccessDecision::Deny
            }
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected decision reply, got {other:?}"));
                AccessDecision::Deny
            }
            Err(_) => AccessDecision::Deny,
        }
    }
}
