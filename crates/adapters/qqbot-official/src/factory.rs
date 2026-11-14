//! QQ 官方机器人适配器的注册入口（与 onebot-snowluma 同构，ADR-0027）。
//!
//! core 按 `kind = "qqbot"` 从 Root 取 `AdapterInstance`；连接线程由适配器
//! 自己启动（`launch`），core 不持有 socket。
//!
//! 连接配置约定：
//! - `account` = 机器人的 AppID（开放平台控制台的机器人 ID）；
//! - `token` = AppSecret（用于换 access token；官方已废弃直接用 Bot Token）；
//! - `url`（可选）= 网关地址，默认 `wss://api.sgroup.qq.com/websocket`；
//! - `options.api_base`（可选）= REST 基地址（沙箱用 sandbox.api.sgroup.qq.com）；
//! - `options.token_url`（可选）= token 接口（默认 api.bot.qq.com）。

use std::borrow::Cow;
use acb_kernel::{
    ADAPTER_API_VERSION, AdapterCapability, AdapterError, AdapterFactory, AdapterHost,
    AdapterInstance, AdapterManifest, Connection, EventQueue, PluginEffect, PluginMetadata,
};

use crate::{QqBotOfficialOutbound, TokenManager, supervise};

static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("ws-client")];
static CAPABILITIES: &[AdapterCapability] = &[
    AdapterCapability::ReceiveEvents,
    // 只能被动回复，但回复仍然是「发送」：桥的出站通道按它走。
    AdapterCapability::Send,
];
static EVENTS: &[Cow<'static, str>] = &[
    Cow::Borrowed("GROUP_AT_MESSAGE_CREATE"),
    Cow::Borrowed("C2C_MESSAGE_CREATE"),
];
static EVENT_TAGS: &[Cow<'static, str>] = &[];

/// 这条适配器的静态声明。`kind` 必须和 config.toml 里的 `kind` 一致。
pub static MANIFEST: AdapterManifest = AdapterManifest {
    id: Cow::Borrowed("acb.adapter.qqbot"),
    platform: Cow::Borrowed("qq"),
    kind: Cow::Borrowed("qqbot"),
    transports: Cow::Borrowed(TRANSPORTS),
    api_version: ADAPTER_API_VERSION,
    capabilities: Cow::Borrowed(CAPABILITIES),
    events: Cow::Borrowed(EVENTS),
    event_tags: Cow::Borrowed(EVENT_TAGS),
    // 群里只有 @ 机器人才有事件，at_self 可信。
    supports_at: true,
    // 平台没有消息引用能力。
    supports_reply: false,
    text_limit: None,
    metadata: PluginMetadata {
        title: Some(Cow::Borrowed("QQ 官方机器人")),
        summary: Some(Cow::Borrowed(
            "QQ 官方开放平台机器人：群 @ 消息与好友单聊（只能被动回复）",
        )),
        description: None,
        tags: Cow::Borrowed(&[]),
        effects: Cow::Borrowed(&[PluginEffect::ExternalCall, PluginEffect::Nondeterministic]),
        docs_url: None,
        options_schema: None,
    },
    // 单聊窗口按 openid 开，连接配置拿不到固定对端；审批路由的直聊房间由
    // 使用方显式配置，不猜默认值。
    direct_room: None,
};

#[derive(Debug, Default)]
pub struct QqBotOfficialAdapterFactory;

impl AdapterFactory for QqBotOfficialAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        MANIFEST.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        _events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        if connection.transport.as_str() != "ws-client" {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!("adapter.qqbot.transport_unsupported", transport = connection.transport.as_str()),
            ));
        }
        if connection.account.trim().is_empty() {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!("adapter.qqbot.appid_required", id = connection.id),
            ));
        }
        if connection.token.as_deref().unwrap_or_default().trim().is_empty() {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!("adapter.qqbot.secret_required", id = connection.id),
            ));
        }
        let secret = connection.token.as_deref().unwrap_or_default().trim().to_owned();
        let token_url = option_str(connection, "token_url");
        let token_manager = match token_url {
            Some(url) => TokenManager::with_token_url(url, connection.account.trim(), secret),
            None => TokenManager::new(connection.account.trim(), secret),
        };
        let api_base = option_str(connection, "api_base").unwrap_or_else(|| {
            crate::outbound::default_api_base().to_owned()
        });
        // 出站与入站共享一份被动回复状态：入站记 msg_id 锚点，出站取用。
        let reply_state = crate::outbound::ReplyState::new();
        let outbound = Box::new(QqBotOfficialOutbound::new(
            connection.id.clone(),
            token_manager.clone(),
            reply_state.clone(),
            api_base,
        ));

        let mut manifest = MANIFEST.clone();
        manifest.metadata.title = Some(Cow::Owned(format!(
            "QQ 官方机器人 ({})",
            connection.account.trim()
        )));

        let connection = connection.clone();
        let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
            supervise(
                connection,
                host.shutdown,
                host.events,
                host.status,
                token_manager,
                reply_state,
            );
        });

        Ok(AdapterInstance {
            outbound,
            launch,
            manifest,
        })
    }
}

fn option_str(connection: &Connection, key: &str) -> Option<String> {
    connection
        .options
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(token: Option<&str>) -> Connection {
        Connection {
            id: "qqbot-main".into(),
            platform: "qq".into(),
            kind: "qqbot".into(),
            transport: acb_kernel::Transport::WsClient,
            url: None,
            options: serde_json::json!({}),
            token: token.map(str::to_owned),
            account: "1234567890".into(),
            admin_qqs: Vec::new(),
            receive: Default::default(),
            routes: Vec::new(),
        }
    }

    #[test]
    fn manifest_keeps_persistent_ids() {
        assert_eq!(MANIFEST.id.as_ref(), "acb.adapter.qqbot");
        assert_eq!(MANIFEST.kind.as_ref(), "qqbot");
        assert_eq!(MANIFEST.platform.as_ref(), "qq");
        assert_eq!(MANIFEST.metadata.title.as_deref(), Some("QQ 官方机器人"));
        assert!(MANIFEST.transports.iter().any(|item| item == "ws-client"));
        assert!(MANIFEST.capabilities.iter().any(|c| matches!(c, AdapterCapability::Send)));
        assert!(MANIFEST.capabilities.iter().any(|c| matches!(c, AdapterCapability::ReceiveEvents)));
    }

    #[test]
    fn create_requires_appid() {
        let mut connection = connection(Some("secret"));
        connection.account = "  ".into();
        let error = QqBotOfficialAdapterFactory
            .create(&connection, EventQueue::channel().0)
            .expect_err("缺 AppID 应报错");
        assert!(matches!(error, AdapterError::Create(id, _) if id == "qqbot-main"));
    }

    #[test]
    fn create_requires_client_secret() {
        let error = QqBotOfficialAdapterFactory
            .create(&connection(None), EventQueue::channel().0)
            .expect_err("缺 AppSecret 应报错");
        assert!(matches!(error, AdapterError::Create(id, _) if id == "qqbot-main"));
    }

    #[test]
    fn create_rejects_non_ws_client_transport() {
        let mut connection = connection(Some("secret"));
        connection.transport = acb_kernel::Transport::Process;
        let error = QqBotOfficialAdapterFactory
            .create(&connection, EventQueue::channel().0)
            .expect_err("只支持 ws-client");
        assert!(matches!(error, AdapterError::Create(_, _)));
    }
}
