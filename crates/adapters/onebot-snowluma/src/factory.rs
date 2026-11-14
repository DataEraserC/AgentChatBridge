//! OneBot SnowLuma 适配器的注册入口（ADR-0027）。
//!
//! core 不需要知道 `LinkSlot` / `OneBotSnowLumaOutbound` / `supervise` 怎么拼；它只按
//! `kind = "onebot11"` 从 Root 取一个 `AdapterInstance`。连接线程仍然由适配器
//! 自己启动（`launch`），core 不持有 socket，也不把 `Bridge` 交出去。

use std::borrow::Cow;
use std::time::Duration;

use serde_json::Value;

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
    Cow::Borrowed("group_msg_emoji_like"),
    Cow::Borrowed("group_admin"),
    Cow::Borrowed("group_upload"),
    Cow::Borrowed("group_card"),
    Cow::Borrowed("notify"),
    Cow::Borrowed("request_friend"),
    Cow::Borrowed("request_group"),
];
static EVENT_TAGS: &[Cow<'static, str>] = &[
    Cow::Borrowed("poke"),
    Cow::Borrowed("reaction"),
    Cow::Borrowed("reaction_add"),
    Cow::Borrowed("reaction_remove"),
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
    // 静态声明不知道连接/账号；实例化时补上。
    direct_room: None,
};

/// 异步路径的上传时限：同步路径仍用 45 秒，后台任务给大文件留足时间。
fn async_upload_timeout(connection: &Connection) -> Result<Duration, AdapterError> {
    match connection.options.get("async_upload_timeout_ms") {
        None => Ok(crate::outbound::ASYNC_UPLOAD_TIMEOUT),
        Some(value) => match value.as_u64() {
            Some(ms) if ms > 0 => Ok(Duration::from_millis(ms)),
            _ => Err(AdapterError::Create(
                connection.id.clone(),
                acb_i18n::t!(
                    "adapter.onebot11.async_upload_timeout_invalid",
                    id = connection.id
                ),
            )),
        },
    }
}

/// 自有库（`<state_root>/onebot11/connections.db`）里的连接定义。
///
/// provider 侧的真 schema：连接定义（含 token 与路由）住在适配器自己的库里，
/// 核心零改动。`token` 支持 AcbKey 加密（`v1:` 前缀，ACB_KEY_FILE/ACB_KEY
/// 解密）与明文两种存储；密文解不开的行跳过并告警，不拖垮其它连接。
struct OwnedOneBotConnection {
    id: String,
    transport: String,
    url: Option<String>,
    token: Option<String>,
    account: String,
    admin_qqs: Vec<String>,
    routes_json: String,
    options_json: String,
}

fn connections_db_path(state_root: &std::path::Path) -> std::path::PathBuf {
    state_root.join("onebot11").join("connections.db")
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
                transport   TEXT NOT NULL DEFAULT 'ws-client',
                url         TEXT,
                token       TEXT,
                account     TEXT NOT NULL,
                admin_json  TEXT NOT NULL DEFAULT '[]',
                routes_json TEXT NOT NULL DEFAULT '[]',
                options_json TEXT NOT NULL DEFAULT '{}',
                created_at  INTEGER NOT NULL
            )",
            [],
        )
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

/// 从自有库读连接定义。
fn load_owned_connections(path: &std::path::Path) -> Vec<Connection> {
    let Ok(connection) = open_connections_db(path) else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT id, transport, url, token, account, admin_json, routes_json, options_json
         FROM connections ORDER BY created_at, id",
    ) else {
        return Vec::new();
    };
    let rows = statement.query_map([], |row| {
        Ok(OwnedOneBotConnection {
            id: row.get(0)?,
            transport: row.get(1)?,
            url: row.get(2)?,
            token: row.get(3)?,
            account: row.get(4)?,
            admin_qqs: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
            routes_json: row.get(6)?,
            options_json: row.get(7)?,
        })
    });
    let mut rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!(
                "{}",
                acb_i18n::t!("adapter.onebot11.state_open_failed", error = error.to_string())
            );
            return Vec::new();
        }
    };
    let key = acb_kernel::crypto::AcbKey::load().ok().flatten();
    let mut connections = Vec::new();
    while let Some(Ok(owned)) = rows.next() {
        let token = match owned.token.as_deref() {
            None => None,
            Some(text) if acb_kernel::crypto::is_encrypted(text) => {
                let Some(key) = key.as_ref() else {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("adapter.onebot11.secret_key_missing", id = owned.id)
                    );
                    continue;
                };
                match key.decrypt(text) {
                    Ok(plain) => Some(plain),
                    Err(error) => {
                        eprintln!(
                            "{}",
                            acb_i18n::t!(
                                "adapter.onebot11.secret_decrypt_failed",
                                id = owned.id,
                                error = error
                            )
                        );
                        continue;
                    }
                }
            }
            Some(text) => Some(text.to_owned()),
        };
        // 路由：[{provider, options}] → RoutePolicy（RoutePolicy 无 Deserialize，
        // 手工构造，与 build_routes 同形）。provider 非法的行跳过。
        let mut routes = Vec::new();
        if let Ok(list) = serde_json::from_str::<Value>(&owned.routes_json) {
            let list = list.as_array().cloned().unwrap_or_default();
            for route in list {
                let Some(provider_text) = route.get("provider").and_then(|v| v.as_str()) else {
                    continue;
                };
                let Ok(provider) = acb_kernel::ProviderId::parse(provider_text) else {
                    continue;
                };
                routes.push(acb_kernel::RoutePolicy {
                    provider,
                    options: route.get("options").cloned().unwrap_or(Value::Null),
                });
            }
        }
        let Some(transport) = acb_kernel::Transport::parse(&owned.transport) else {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.onebot11.transport_unknown",
                    id = owned.id,
                    transport = owned.transport
                )
            );
            continue;
        };
        connections.push(Connection {
            id: owned.id.clone(),
            platform: "qq".to_owned(),
            kind: "onebot11".to_owned(),
            transport,
            url: owned.url,
            options: serde_json::from_str(&owned.options_json).unwrap_or_default(),
            token,
            account: owned.account,
            admin_qqs: owned.admin_qqs,
            receive: Default::default(),
            routes,
        });
    }
    connections
}

/// OneBot SnowLuma 的工厂。一个连接一个 `LinkSlot`，`launch` 里起守护线程。
#[derive(Debug, Default)]
pub struct OneBotSnowLumaAdapterFactory;

impl AdapterFactory for OneBotSnowLumaAdapterFactory {
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
        let async_upload_timeout = async_upload_timeout(connection)?;
        let outbound = Box::new(
            OneBotSnowLumaOutbound::new(slot.clone(), connection.id.clone())
                .with_async_upload_timeout(async_upload_timeout),
        );

        // 默认审批路由的一对一房间由适配器提供，core 不再拼 private/{account}。
        let mut manifest = MANIFEST.clone();
        manifest.direct_room = Some(Cow::Owned(format!(
            "{}:private/{}",
            connection.id, connection.account
        )));

        let connection = connection.clone();
        let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
            supervise(slot, connection, host.shutdown, host.events, host.status);
        });

        Ok(AdapterInstance {
            outbound,
            launch,
            manifest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(options: serde_json::Value) -> Connection {
        Connection {
            id: "qq-main".into(),
            platform: "qq".into(),
            kind: "onebot11".into(),
            transport: acb_kernel::Transport::WsClient,
            url: Some("ws://127.0.0.1:6700".into()),
            options,
            token: None,
            account: "10001".into(),
            admin_qqs: Vec::new(),
            receive: Default::default(),
            routes: Vec::new(),
        }
    }

    #[test]
    fn async_upload_timeout_defaults_to_300_seconds() {
        assert_eq!(
            async_upload_timeout(&connection(serde_json::json!({}))).expect("默认合法"),
            crate::outbound::ASYNC_UPLOAD_TIMEOUT
        );
    }

    #[test]
    fn async_upload_timeout_can_be_overridden_per_connection() {
        assert_eq!(
            async_upload_timeout(&connection(
                serde_json::json!({ "async_upload_timeout_ms": 60_000 })
            ))
            .expect("正整数合法"),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn async_upload_timeout_rejects_zero_and_non_integers() {
        for options in [
            serde_json::json!({ "async_upload_timeout_ms": 0 }),
            serde_json::json!({ "async_upload_timeout_ms": -1 }),
            serde_json::json!({ "async_upload_timeout_ms": 1.5 }),
            serde_json::json!({ "async_upload_timeout_ms": "slow" }),
        ] {
            let error = async_upload_timeout(&connection(options)).expect_err("应当报错");
            assert!(matches!(error, AdapterError::Create(id, _) if id == "qq-main"));
        }
    }

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

#[cfg(test)]
mod provider_tests {
    use super::*;

    fn seed(db: &std::path::Path, rows: &[(&str, &str, Option<&str>, &str, &str)]) {
        let connection = open_connections_db(db).expect("建库");
        for (id, transport, url, account, admin) in rows {
            connection
                .execute(
                    "INSERT INTO connections (id, transport, url, token, account, admin_json, created_at)
                     VALUES (?1, ?2, ?3, NULL, ?4, ?5, 1)",
                    rusqlite::params![id, transport, url, account, admin],
                )
                .expect("插入");
        }
    }

    #[test]
    fn provider_reads_ws_client_rows() {
        let root = std::env::temp_dir().join(format!("onebot-provider-{}", std::process::id()));
        let db = connections_db_path(&root);
        std::fs::create_dir_all(db.parent().expect("parent")).expect("目录");
        seed(
            &db,
            &[("qq2", "ws-client", Some("ws://127.0.0.1:53001"), "3683924053", r#"["323622442"]"#)],
        );
        let connections = OneBotSnowLumaAdapterFactory
            .connections(&root)
            .expect("是 provider");
        assert_eq!(connections.len(), 1);
        let connection = &connections[0];
        assert_eq!(connection.id, "qq2");
        assert_eq!(connection.kind, "onebot11");
        assert_eq!(connection.platform, "qq");
        assert_eq!(connection.url.as_deref(), Some("ws://127.0.0.1:53001"));
        assert_eq!(connection.account, "3683924053");
        assert_eq!(connection.admin_qqs, vec!["323622442".to_owned()]);
        assert!(connection.token.is_none());
        assert!(matches!(connection.transport, acb_kernel::Transport::WsClient));
    }

    #[test]
    fn provider_reads_ws_server_rows() {
        let root = std::env::temp_dir().join(format!("onebot-provider2-{}", std::process::id()));
        let db = connections_db_path(&root);
        std::fs::create_dir_all(db.parent().expect("parent")).expect("目录");
        seed(&db, &[("qq3", "ws-server", None, "10002", "[]")]);
        let connections = OneBotSnowLumaAdapterFactory
            .connections(&root)
            .expect("是 provider");
        assert_eq!(connections.len(), 1);
        assert!(matches!(connections[0].transport, acb_kernel::Transport::WsServer));
    }
}
