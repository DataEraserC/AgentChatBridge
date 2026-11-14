//! OneBot SnowLuma 适配器的注册入口（ADR-0027）。
//!
//! core 不需要知道 `LinkSlot` / `OneBotSnowLumaOutbound` / `supervise` 怎么拼；它只按
//! `kind = "onebot11"` 从 Root 取一个 `AdapterInstance`。连接线程仍然由适配器
//! 自己启动（`launch`），core 不持有 socket，也不把 `Bridge` 交出去。

use std::borrow::Cow;
use std::time::Duration;

use acb_kernel::{
    ADAPTER_API_VERSION, AdapterCapability, AdapterError, AdapterFactory, AdapterHost,
    AdapterInstance, AdapterManifest, Connection, EventQueue, PluginEffect, PluginMetadata,
    ProviderConnections,
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

/// 确保 provider 侧的标准连接库存在（schema 由 core 定义，见
/// `acb_kernel::ADAPTER_CONNECTIONS_SCHEMA_SQL`）。行由运维修护
/// （sqlite3 INSERT），适配器只保证文件与表就位。
fn ensure_connections_db(path: &std::path::Path) {
    let Ok(connection) = rusqlite::Connection::open(path) else {
        return;
    };
    let _ = connection.execute(acb_kernel::ADAPTER_CONNECTIONS_SCHEMA_SQL, []);
}

/// 把提供者库里的连接行读成 wire 注册载荷（重启回放，B期协议 ADR-031）。
///
/// 只做形态转换，不做语义校验——core 侧注册表有自己的校验与 config 赢规则。
/// `token_secret` 引用名原样透传（core 在应用时查 secrets 解析）；库打不开
/// /表不存在/行不合法的行跳过，返回空 = 注册表保持启动注入的基线不动。
fn read_provider_connections(db_path: &std::path::Path) -> Vec<acb_adapter_api::WireConnection> {
    let Ok(connection) =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
    else {
        return Vec::new();
    };
    let Ok(mut statement) = connection.prepare(
        "SELECT id, platform, kind, transport, url, account, token_secret,
                admin_json, routes_json, options_json
         FROM connections ORDER BY created_at, id",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
        ))
    }) else {
        return Vec::new();
    };
    rows.flatten()
        .filter_map(
            |(
                id,
                platform,
                kind,
                transport,
                url,
                account,
                token_secret,
                admin_json,
                routes_json,
                options_json,
            )| {
                if id.trim().is_empty()
                    || platform.trim().is_empty()
                    || kind.trim().is_empty()
                    || account.trim().is_empty()
                {
                    return None;
                }
                let parse_or = |text: &str, default: serde_json::Value| {
                    serde_json::from_str::<serde_json::Value>(text).unwrap_or(default)
                };
                Some(acb_adapter_api::WireConnection {
                    id,
                    platform,
                    kind,
                    transport,
                    url: url.filter(|url| !url.trim().is_empty()),
                    token: None,
                    account,
                    admin_qqs: parse_or(&admin_json, serde_json::json!([]))
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect(),
                    options: parse_or(&options_json, serde_json::json!({})),
                    token_secret: token_secret.filter(|name| !name.trim().is_empty()),
                    routes: parse_or(&routes_json, serde_json::json!([])),
                })
            },
        )
        .collect()
}

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

/// OneBot SnowLuma 的工厂。一个连接一个 `LinkSlot`，`launch` 里起守护线程。
#[derive(Debug, Default)]
pub struct OneBotSnowLumaAdapterFactory;

/// 连接 options 的形状声明（JSON Schema 子集，与插件 config_schema 同一渲染器）。
/// keys 必须与 factory/link 里实际读取的一致。static 里不能调 json!（非 const），
/// manifest() 运行时注入。
fn onebot11_options_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "http_listen": {
                "type": "string",
                "description": "HTTP 上报监听地址（如 127.0.0.1:53000）；配置了就必须给这条连接配 token"
            },
            "async_upload_timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "default": 30000,
                "description": "大文件上传走后台任务时的时限（毫秒）"
            }
        }
    })
}

impl AdapterFactory for OneBotSnowLumaAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        let mut manifest = MANIFEST.clone();
        manifest.metadata.options_schema = Some(onebot11_options_schema());
        manifest
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
            // provider 侧连接库（标准 schema，core 定义）：确保存在，运维在此
            // 维护连接行。ensure 要拿库文件路径——注入扫描的是
            // `state/adapters/<目录>/connections.db`，传目录会让 SQLite
            // 打不开目录（存在时）或建出一个同名无后缀文件（不存在时），
            // 两种情况 core 都看不到。
            let connections_db = host
                .state_dir
                .parent()
                .expect("state_dir 有父目录")
                .join("onebot11")
                .join("connections.db");
            ensure_connections_db(&connections_db);
            // 重启回放（B期）：把库里的连接集合推给 core 的运行时注册表。
            // 空库推空集合 = 「本提供者现在没有连接」，与注入基线一致。
            if let Some(sender) = host.connections.as_ref() {
                let connections = read_provider_connections(&connections_db);
                if let Err(error) = sender.send(ProviderConnections {
                    provider: "onebot11".to_owned(),
                    connections,
                    removed: Vec::new(),
                }) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!("adapter.onebot11.connections_forward_failed", error = error)
                    );
                }
            }
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
