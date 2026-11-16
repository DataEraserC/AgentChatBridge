//! 把进程通道包装成 Root 认识的 access provider。

use std::sync::{Arc, Mutex};

use acb_adapter_api::{WireAccessRequest, WireResult, WireWakeRequest};
use acb_kernel::{
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, Capability, ExtensionPoint,
    Health, ManagerManifest, ManagerPlugin, PLUGIN_API_VERSION, PluginError, PluginMetadata,
    PluginSource, ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, WakeContext,
    WakeDecision, WakePolicy, WakeProvider, WakeVerdict,
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
        match self.channel.decide("access", options, request) {
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

/// Wake 扩展点的进程实现（ADR-0033 第二层）。
///
/// 与 access 完全同构：同一个子进程、同一套 `decide` 请求，只是 `point`
/// 换成 `"wake"`。这也是「Process 载体补齐扩展点不需要新协议」的实证——
/// `CoreMessage::Decide` 的 `point` 本来就是字符串。
pub(crate) struct ProcessWakeManager {
    provider: ProviderId,
    metadata: PluginMetadata,
    channel: Arc<ProcessChannel>,
    cached: Mutex<Option<Arc<ProcessWakeProvider>>>,
}

impl ProcessWakeManager {
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

impl ManagerPlugin for ProcessWakeManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
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
        if !matches!(options, ProviderOptions::Wake(_)) {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: "process wake provider requires Wake options".to_owned(),
            });
        }
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let provider = cached
            .get_or_insert_with(|| {
                Arc::new(ProcessWakeProvider {
                    channel: Arc::clone(&self.channel),
                })
            })
            .clone();
        Ok(ProviderInstance::Wake(provider))
    }

    fn health(&self) -> Health {
        self.channel.health()
    }

    fn shutdown(&self) {
        self.channel.shutdown();
    }
}

struct ProcessWakeProvider {
    channel: Arc<ProcessChannel>,
}

impl WakeProvider for ProcessWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeVerdict {
        if !matches!(self.channel.health(), Health::Healthy) {
            // 子进程挂了：按「不唤醒」处理，让桥走内置默认，而不是把消息吞掉。
            return WakeVerdict {
                decision: WakeDecision::Stay,
                reason: Some("process provider is down".to_owned()),
                delay_ms: 0,
                batch: false,
            };
        }
        let wire = WireWakeRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            connection: context.connection.to_owned(),
            platform: context.platform.to_owned(),
            sender_id: context.sender_id.to_owned(),
            text: context.text.to_owned(),
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            has_body: context.has_body,
            event: context.event.map(str::to_owned),
            tags: context.tags.iter().map(|tag| (*tag).to_owned()).collect(),
            unread: context.unread,
            last_wake_at: context.last_wake_at,
            now: context.now,
        };
        let request = serde_json::to_value(wire).unwrap_or(serde_json::Value::Null);
        let options_json = json!({
            "provider": options.provider.to_string(),
            "extra": options.options,
        });
        match self.channel.decide("wake", options_json, request) {
            Ok(WireResult::Decision {
                decision,
                reason,
                delay_ms,
                batch,
            }) => {
                let parsed = match decision.as_str() {
                    "wake" => WakeDecision::Wake,
                    other => {
                        if other != "stay" {
                            self.channel
                                .note_invalid_reply(&format!("unknown wake decision `{other}`"));
                        }
                        WakeDecision::Stay
                    }
                };
                WakeVerdict {
                    decision: parsed,
                    reason,
                    delay_ms,
                    batch,
                }
            }
            Ok(other) => {
                self.channel
                    .note_invalid_reply(&format!("expected decision reply, got {other:?}"));
                WakeVerdict {
                    decision: WakeDecision::Stay,
                    reason: Some("process provider replied with an unexpected shape".to_owned()),
                    delay_ms: 0,
                    batch: false,
                }
            }
            Err(detail) => WakeVerdict {
                decision: WakeDecision::Stay,
                reason: Some(detail),
                delay_ms: 0,
                batch: false,
            },
        }
    }
}
