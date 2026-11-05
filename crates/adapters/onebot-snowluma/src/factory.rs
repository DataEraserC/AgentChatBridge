//! OneBot SnowLuma 适配器的注册入口（ADR-0027）。
//!
//! core 不需要知道 `LinkSlot` / `OneBotSnowLumaOutbound` / `supervise` 怎么拼；它只按
//! `kind = "onebot11"` 从 Root 取一个 `AdapterInstance`。连接线程仍然由适配器
//! 自己启动（`launch`），core 不持有 socket，也不把 `Bridge` 交出去。

use std::borrow::Cow;

use acb_kernel::{
    ADAPTER_API_VERSION, AdapterCapability, AdapterError, AdapterFactory, AdapterHost,
    AdapterInstance, AdapterManifest, Connection, EventQueue, PluginEffect, PluginMetadata,
};

use crate::{LinkSlot, OneBotSnowLumaOutbound, supervise};

static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("ws-client"), Cow::Borrowed("ws-server")];
static CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    AdapterCapability::Send,
    AdapterCapability::Catalog,
    // 真实实现：msg.recall（delete_msg）、At 元素、image/record/video/file 段。
    // 编辑消息当前协议不支持，不声明。
    AdapterCapability::Recall,
    AdapterCapability::Mention,
    AdapterCapability::RichMedia,
];
/// 已知会进流水的事件名（事件里的 `event` 字段）。OneBot 的 notice_type 是开放的，
/// 没列出来的也会原样记，所以这是「常见事件」清单，不是白名单。
static EVENTS: &[Cow<'static, str>] = &[
    Cow::Borrowed("poke"),
    Cow::Borrowed("group_increase"),
    Cow::Borrowed("group_decrease"),
    Cow::Borrowed("group_ban"),
    Cow::Borrowed("group_recall"),
    Cow::Borrowed("friend_recall"),
    Cow::Borrowed("group_admin"),
    Cow::Borrowed("group_upload"),
    Cow::Borrowed("group_card"),
    Cow::Borrowed("notify"),
    Cow::Borrowed("request_friend"),
    Cow::Borrowed("request_group"),
];
static EVENT_TAGS: &[Cow<'static, str>] = &[
    Cow::Borrowed("poke"),
    Cow::Borrowed("notice"),
    Cow::Borrowed("request"),
];

/// 这条适配器的静态声明。`kind` 必须和 config.toml 里的 `kind` 一致。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: Cow::Borrowed("acb.adapter.onebot11"),
    platform: Cow::Borrowed("qq"),
    kind: Cow::Borrowed("onebot11"),
    transports: Cow::Borrowed(TRANSPORTS),
    api_version: ADAPTER_API_VERSION,
    capabilities: Cow::Borrowed(CAPABILITIES),
    events: Cow::Borrowed(EVENTS),
    event_tags: Cow::Borrowed(EVENT_TAGS),
    supports_at: true,
    supports_reply: true,
    // QQ 客户端/协议实现的长消息上限不统一，不谎报一个数字；等有可验证来源再填。
    text_limit: None,
    metadata: PluginMetadata {
        title: Some(Cow::Borrowed("OneBot SnowLuma")),
        summary: Some(Cow::Borrowed("QQ 群与私聊：收消息、发消息、平台方法目录")),
        description: None,
        tags: Cow::Borrowed(&[]),
        effects: Cow::Borrowed(&[PluginEffect::ExternalCall, PluginEffect::Nondeterministic]),
        docs_url: None,
        options_schema: None,
    },
};

/// OneBot SnowLuma 的工厂。一个连接一个 `LinkSlot`，`launch` 里起守护线程。
#[derive(Debug, Default)]
pub struct OneBotSnowLumaAdapterFactory;

impl AdapterFactory for OneBotSnowLumaAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        MANIFEST.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        _events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        if connection.transport.as_str() == "ws-server" {
            if crate::link::server_listen(connection).is_none() {
                return Err(AdapterError::Create(
                    connection.id.clone(),
                    acb_i18n::t!("adapter.onebot11.ws_server_requires_listen"),
                ));
            }
        } else if connection.url.is_none() {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!("adapter.onebot11.url_required"),
            ));
        }
        let http_events = connection
            .options
            .get("http_listen")
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.trim().is_empty());
        if http_events
            && connection
                .token
                .as_deref()
                .unwrap_or_default()
                .trim()
                .is_empty()
        {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!(
                    "adapter.onebot11.http_listen_requires_token",
                    id = connection.id
                ),
            ));
        }
        let slot = LinkSlot::new();
        let outbound = Box::new(OneBotSnowLumaOutbound::new(
            slot.clone(),
            connection.id.clone(),
        ));

        let connection = connection.clone();
        let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
            supervise(slot, connection, host.shutdown, host.events, host.status);
        });

        Ok(AdapterInstance {
            outbound,
            launch,
            manifest: MANIFEST.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_keeps_persistent_ids_and_uses_the_snowluma_display_name() {
        assert_eq!(MANIFEST.id.as_ref(), "acb.adapter.onebot11");
        assert_eq!(MANIFEST.kind.as_ref(), "onebot11");
        assert_eq!(MANIFEST.platform.as_ref(), "qq");
        assert_eq!(MANIFEST.metadata.title.as_deref(), Some("OneBot SnowLuma"));
        assert!(
            MANIFEST.transports.iter().any(|item| item == "ws-server"),
            "反向 WS 必须在 manifest 里声明"
        );
        assert!(MANIFEST.transports.iter().any(|item| item == "ws-client"));
    }
}
