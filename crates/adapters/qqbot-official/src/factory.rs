//! QQ 官方机器人适配器的注册入口（与 onebot-snowluma 同构，ADR-0027）。
//!
//! core 按 `kind = "qqbot"` 从 Root 取 `AdapterInstance`；连接线程由适配器
//! 自己启动（`launch`），core 不持有 socket。
//!
//! 连接配置约定（provider 模式，见 kernel 的 `ADAPTER_CONNECTIONS_SCHEMA_SQL`）：
//! - 连接定义住在适配器自有库 `state/adapters/qqbot/connections.db`
//!   （标准 schema，`launch` 时确保存在），运维直接维护行；
//! - `account` = 机器人的 AppID；`token_secret` = secrets 体系里的引用名
//!   （AppSecret 值存控制台/agenix，适配器库不存明文）；
//! - `options_json` 可带 `api_base` / `token_url` 覆盖默认端点。

use acb_kernel::{
    ADAPTER_API_VERSION, ADAPTER_CONNECTIONS_SCHEMA_SQL, AdapterCapability, AdapterError,
    AdapterFactory, AdapterHost, AdapterInstance, AdapterManifest, Connection, EventQueue,
    PluginEffect, PluginMetadata,
};
use std::borrow::Cow;

use crate::{QqBotOfficialOutbound, TokenManager, supervise};

// 专用 transport `qq-gateway`：适配器直连官方网关（wss://api.sgroup.qq.com，
// 固定端点），不走通用 ws-client 语义——后者在 config 解析层强制要求 ws://
// url，而本适配器的 url 字段无效。 manifested transports 驱动控制台表单的
// transport 下拉与 url 行显隐。
static TRANSPORTS: &[Cow<'static, str>] = &[Cow::Borrowed("qq-gateway")];
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

/// 确保 provider 侧的标准连接库存在（schema 由 core 定义，见
/// `ADAPTER_CONNECTIONS_SCHEMA_SQL`）。行由运维修护
/// （sqlite3 INSERT），适配器只保证文件与表就位。
fn ensure_connections_db(path: &std::path::Path) {
    let Ok(connection) = rusqlite::Connection::open(path) else {
        return;
    };
    let _ = connection.execute(ADAPTER_CONNECTIONS_SCHEMA_SQL, []);
}

#[derive(Debug, Default)]
pub struct QqBotOfficialAdapterFactory;

/// 连接表单的适配器自定义注解 + 端点覆盖项（JSON Schema 子集）：
/// x-account 修正「账号」字段的语义（这里填 AppID，不是 QQ 号）；
/// properties 与 factory 实际读取的 options 键（api_base / token_url）一致。
/// static 里不能调 json!（非 const），manifest() 运行时注入。
fn qqbot_options_schema() -> serde_json::Value {
    serde_json::json!({
        "x-account": {
            "placeholder": "机器人的 AppID",
            "description": "填官方开放平台的 AppID（不是 QQ 号）；AppSecret 走上面的密钥来源"
        },
        "type": "object",
        "properties": {
            "api_base": {
                "type": "string",
                "description": "覆盖 API 端点（默认 https://api.sgroup.qq.com）；自建网关/调试才需要"
            },
            "token_url": {
                "type": "string",
                "description": "覆盖取 token 的端点（默认 https://api.sgroup.qq.com/app/getAppAccessToken）"
            }
        }
    })
}

impl AdapterFactory for QqBotOfficialAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        let mut manifest = MANIFEST.clone();
        manifest.metadata.options_schema = Some(qqbot_options_schema());
        manifest
    }

    fn create(
        &self,
        connection: &Connection,
        _events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        if connection.transport.as_str() != "qq-gateway" {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!(
                    "adapter.qqbot.transport_unsupported",
                    transport = connection.transport.as_str()
                ),
            ));
        }
        if connection.account.trim().is_empty() {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!("adapter.qqbot.appid_required", id = connection.id),
            ));
        }
        if connection
            .token
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!("adapter.qqbot.secret_required", id = connection.id),
            ));
        }
        let secret = connection
            .token
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_owned();
        let token_url = option_str(connection, "token_url");
        let token_manager = match token_url {
            Some(url) => TokenManager::with_token_url(url, connection.account.trim(), secret),
            None => TokenManager::new(connection.account.trim(), secret),
        };
        let api_base = option_str(connection, "api_base")
            .unwrap_or_else(|| crate::outbound::default_api_base().to_owned());
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
            // 被动窗口锚点进自有库：重启后仍能回复（桥重启不丢 msg_id）。
            reply_state.bind(&host.state_dir.join("adapter.db"));
            // provider 侧连接库（标准 schema，core 定义）：确保存在，运维在此
            // 维护连接行。ensure 要拿库文件路径（与注入扫描的
            // `state/adapters/<目录>/connections.db` 同形），传目录会建错位置。
            ensure_connections_db(
                &host
                    .state_dir
                    .parent()
                    .expect("state_dir 有父目录")
                    .join("qqbot")
                    .join("connections.db"),
            );
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
            transport: acb_kernel::Transport::parse("qq-gateway").expect("合法"),
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
        assert!(MANIFEST.transports.iter().any(|item| item == "qq-gateway"));
        assert!(
            MANIFEST
                .capabilities
                .iter()
                .any(|c| matches!(c, AdapterCapability::Send))
        );
        assert!(
            MANIFEST
                .capabilities
                .iter()
                .any(|c| matches!(c, AdapterCapability::ReceiveEvents))
        );
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
    fn create_rejects_foreign_transport() {
        let mut connection = connection(Some("secret"));
        connection.transport = acb_kernel::Transport::Process;
        let error = QqBotOfficialAdapterFactory
            .create(&connection, EventQueue::channel().0)
            .expect_err("只支持 qq-gateway");
        assert!(matches!(error, AdapterError::Create(_, _)));
    }

    #[test]
    fn ensure_connections_db_creates_standard_schema() {
        let dir = std::env::temp_dir().join(format!("qqbot-ensure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let path = dir.join("connections.db");
        ensure_connections_db(&path);
        let connection = rusqlite::Connection::open(&path).expect("打开");
        let columns: Vec<String> = connection
            .prepare("PRAGMA table_info(connections)")
            .expect("pragma")
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query")
            .filter_map(Result::ok)
            .collect();
        for expected in [
            "id",
            "platform",
            "kind",
            "transport",
            "url",
            "account",
            "token_secret",
            "admin_json",
            "routes_json",
            "options_json",
            "created_at",
        ] {
            assert!(columns.contains(&expected.to_owned()), "缺列 {expected}");
        }
    }
}
