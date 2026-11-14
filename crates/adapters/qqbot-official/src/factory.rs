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

/// 自有库（`<state_root>/qqbot/connections.db`）里的连接定义。
///
/// 这是 provider 侧的真 schema：连接定义、凭据、群管理员都住在适配器自己的
/// 库里，核心零改动。AppSecret 支持 AcbKey 加密（`v1:` 前缀，ACB_KEY_FILE/
/// ACB_KEY 解密）与明文两种存储。
struct OwnedConnection {
    id: String,
    app_id: String,
    app_secret: String,
    api_base: Option<String>,
    ws_url: Option<String>,
    admin_qqs: Vec<String>,
}

fn connections_db_path(state_root: &std::path::Path) -> std::path::PathBuf {
    state_root.join("qqbot").join("connections.db")
}

fn open_connections_db(path: &std::path::Path) -> Result<rusqlite::Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = rusqlite::Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute(
            "CREATE TABLE IF NOT EXISTS connections (
                id          TEXT PRIMARY KEY,
                app_id      TEXT NOT NULL,
                app_secret  TEXT NOT NULL,
                api_base    TEXT,
                ws_url      TEXT,
                admin_json  TEXT NOT NULL DEFAULT '[]',
                created_at  INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

/// 从自有库读连接定义。解不开的密文（密钥缺失/换 kid）跳过并告警——
/// 单行故障不拖垮其它连接。
fn load_owned_connections(
    path: &std::path::Path,
) -> Vec<Connection> {
    let Ok(connection) = open_connections_db(path) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT id, app_id, app_secret, api_base, ws_url, admin_json
         FROM connections ORDER BY created_at, id",
    ) else {
        return Vec::new();
    };
    let key = acb_kernel::crypto::AcbKey::load().ok().flatten();
    let rows = statement.query_map([], |row| {
        Ok(OwnedConnection {
            id: row.get(0)?,
            app_id: row.get(1)?,
            app_secret: row.get(2)?,
            api_base: row.get(3)?,
            ws_url: row.get(4)?,
            admin_qqs: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
        })
    });
    let mut rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!(
                "{}",
                acb_i18n::t!("adapter.qqbot.state_open_failed", error = error.to_string())
            );
            return Vec::new();
        }
    };
    let mut connections = Vec::new();
    while let Some(Ok(owned)) = rows.next() {
        let secret = if acb_kernel::crypto::is_encrypted(&owned.app_secret) {
            let Some(key) = key.as_ref() else {
                eprintln!(
                    "{}",
                    acb_i18n::t!(
                        "adapter.qqbot.secret_key_missing",
                        id = owned.id
                    )
                );
                continue;
            };
            match key.decrypt(&owned.app_secret) {
                Ok(plain) => plain,
                Err(error) => {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "adapter.qqbot.secret_decrypt_failed",
                            id = owned.id,
                            error = error
                        )
                    );
                    continue;
                }
            }
        } else {
            owned.app_secret.clone()
        };
        let mut options = serde_json::Map::new();
        if let Some(base) = owned.api_base {
            options.insert("api_base".to_owned(), serde_json::Value::String(base));
        }
        connections.push(Connection {
            id: owned.id.clone(),
            platform: "qq".to_owned(),
            kind: "qqbot".to_owned(),
            transport: acb_kernel::Transport::WsClient,
            url: owned.ws_url,
            options: serde_json::Value::Object(options),
            token: Some(secret),
            account: owned.app_id,
            admin_qqs: owned.admin_qqs,
            receive: Default::default(),
            routes: Vec::new(),
        });
    }
    connections
}

#[derive(Debug, Default)]
pub struct QqBotOfficialAdapterFactory;

impl AdapterFactory for QqBotOfficialAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        MANIFEST.clone()
    }

    fn connections(
        &self,
        state_root: &std::path::Path,
    ) -> Option<Vec<Connection>> {
        Some(load_owned_connections(&connections_db_path(state_root)))
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
            // 被动窗口锚点进自有库：重启后仍能回复（桥重启不丢 msg_id）。
            reply_state.bind(&host.state_dir.join("adapter.db"));
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

#[cfg(test)]
mod provider_tests {
    use super::*;

    fn seed(db: &std::path::Path, rows: &[(&str, &str, &str, Option<&str>)]) {
        let connection = open_connections_db(db).expect("建库");
        let now = 1_000_000;
        for (id, app_id, secret, api_base) in rows {
            connection
                .execute(
                    "INSERT INTO connections (id, app_id, app_secret, api_base, admin_json, created_at)
                     VALUES (?1, ?2, ?3, ?4, '[]', ?5)",
                    rusqlite::params![id, app_id, secret, api_base, now],
                )
                .expect("插入");
        }
    }

    #[test]
    fn provider_reads_plaintext_rows() {
        let root = std::env::temp_dir().join(format!("qqbot-provider-{}", std::process::id()));
        let db = connections_db_path(&root);
        std::fs::create_dir_all(db.parent().expect("parent")).expect("目录");
        seed(
            &db,
            &[
                ("qqbot-a", "1111", "secret-a", Some("https://sandbox.example")),
                ("qqbot-b", "2222", "secret-b", None),
            ],
        );
        let connections = QqBotOfficialAdapterFactory
            .connections(&root)
            .expect("是 provider");
        assert_eq!(connections.len(), 2);
        let a = connections.iter().find(|c| c.id == "qqbot-a").expect("A");
        assert_eq!(a.account, "1111");
        assert_eq!(a.kind, "qqbot");
        assert_eq!(a.platform, "qq");
        assert_eq!(
            a.options.get("api_base"),
            Some(&serde_json::json!("https://sandbox.example"))
        );
        assert_eq!(a.token.as_deref(), Some("secret-a"));
        let b = connections.iter().find(|c| c.id == "qqbot-b").expect("B");
        assert!(b.options.get("api_base").is_none(), "None 不写 options");
    }

    #[test]
    fn provider_skips_undecryptable_rows() {
        let root = std::env::temp_dir().join(format!("qqbot-provider2-{}", std::process::id()));
        let db = connections_db_path(&root);
        std::fs::create_dir_all(db.parent().expect("parent")).expect("目录");
        seed(
            &db,
            &[
                ("qqbot-enc", "3333", "v1:deadbeef:notarealciphertext", None),
                ("qqbot-plain", "4444", "plain-secret", None),
            ],
        );
        // 无 ACB_KEY 的环境：密文行跳过，明文行照常。
        let connections = QqBotOfficialAdapterFactory.connections(&root).expect("是 provider");
        assert_eq!(connections.len(), 1);
        assert_eq!(connections[0].id, "qqbot-plain");
    }
}
