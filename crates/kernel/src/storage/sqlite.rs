//! SQLite 实现：把事情记在一个文件里，重启接着用。
//!
//! 只有一条规矩值得先说：**流水里存原话，不存旁白**。`redelivery` / `note` 是投递时
//! 才有的东西（见 `engine::open_turn`），不落库；重启后它们由投递计数重新算出来。
//!
//! 表结构见 `docs/07-storage.md`。改动表结构时把 `SCHEMA_VERSION` 加一，并且**先写迁移**
//! 再写代码——现在遇到不认识的版本会直接拒绝打开，而不是往老库里写坏数据。

use std::path::Path;
use std::time::Duration;

use acb_proto::{Element, Record, RecordKind, RoomId, Sender, SenderRole};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::{
    ConfigObjectsStore, Cursor, Deliveries, FEEDBACK_CAP, Feedback, FeedbackDraft, FeedbackReply,
    Feedbacks, MIN_KEPT, ModelFallbacks, Note, NoteKey, NoteKind, Notes, Policies, PolicyEffect,
    PrunePlan, RecordDraft, Recorded, SessionMap, SessionMaps, StorageError, TaskRecord,
    TaskStatus, Tasks, TopicStatus, Transcript, Watermarks,
};
use crate::config_objects::{ConfigObject, ConfigObjectKind, ConfigObjects};
use crate::grants::{
    AccountGrant, BotAdminGrant, GrantRule, GrantsConfig, RoomGrant, SystemAdminGrant,
};

/// 这个程序认识的库结构版本。改表就加一。
///
/// 1 → 2：`records` 加 `reply_to_self` 与 `platform_id`（引用回复的判定要用）。
/// 2 → 3：房间从「平台 + 房间号」改成「连接 + 路径」——`platform` 列改叫 `connection`，
///         `room` 列改叫 `room_key`，并留下 `room_json` 存原始路径结构。
/// 3 → 4：device 概念升级为「harness 会话」——水位/投递计数/乱序确认三张表的 `device`
///         列改叫 `session`（ADR-0017）。
/// 4 → 5：加 `session_maps` 表——acb 会话 ↔ harness 内部会话的映射，桥侧控制台落盘
///         （ADR-0018）。
/// 5 → 6：`records` 加 `sender_role` 列——发送者身份（系统管理员/群主/群管理员），
///         适配器判得出来才填，判不出留空。
/// 6 → 7：加 `feedback` 表——agent 的反馈通道（ADR-0021）。新表由建表语句建，不用搬数据。
/// 7 → 8：事件记录加 `event_meta` JSON（tags / payload / summary），老记录回填空值。
/// 8 → 9：`records` 加 `labels` JSON；适配器可以给记录追加通用标签（如 recall）。
/// 9 → 10：`records` 加 `suppressed`；被抑制的记录只进流水，不投递/不唤醒/不计未读，
///         语义落在记录上，之后新加的订阅者也不会看到它。
/// 10 → 11：`feedback` 加 `replies` JSON；维护方回复与通知原会话（T7c）。
/// 11 → 12：加 `tasks` 表——后台任务与出站尝试记录落盘，重启后仍可按 id 查。
/// 12 → 13：加 `policy_*` 四张表——权限规则、房间政策、审批路由、管理员从
///          `config.toml` 搬进库。新表由建表语句建，不用搬数据；首次启动时
///          桥把 TOML 里的 `[grants]` 一次性导入（见 `engine::sync_policy`）。
/// 13 → 14：加 `model_fallbacks` 表——主模型被拒（内容审核 / 限流 / 下线）时
///          按顺序改用的后备模型。纯新增，不用搬数据；空表 = 不重试。
/// 14 → 15：加 `config_objects` 表——连接/会话/人格/记忆/harness 五类业务对象
///          从配置文件搬进库。存的是每一条的 TOML 原文，读出来拼回合并文档再解析。
///          纯新增，不用搬数据；首次启动时桥把文件里那五段一次性导入
///          （见 `config_objects::import_once`）。
pub const SCHEMA_VERSION: i64 = 15;

/// 建表语句。`IF NOT EXISTS`：新库建、老库不动。
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS records (
    connection  TEXT    NOT NULL,   -- 连接名（哪个号在看这个房间）
    room_key    TEXT    NOT NULL,   -- 房间路径，如 group/158296748
    room_json   TEXT,               -- 路径的原始结构（JSON 数组），留着给以后换形状用
    record_id   INTEGER NOT NULL,
    kind        TEXT    NOT NULL,   -- message | event
    event       TEXT,               -- 仅 kind = event
    sender_id   TEXT    NOT NULL,
    sender_name TEXT    NOT NULL,
    sender_role TEXT,               -- 身份：system_admin | owner | admin，判不出留空
    at          INTEGER NOT NULL,   -- Unix 毫秒
    at_self     INTEGER NOT NULL,   -- 0 / 1
    reply_to_self INTEGER NOT NULL DEFAULT 0, -- 0 / 1，引用的是我发的消息
    platform_id TEXT,               -- 平台自己的消息编号，反查引用对象用
    elements    TEXT    NOT NULL,   -- 内容元素的 JSON 数组
    event_meta  TEXT,               -- 仅 kind = event：{tags, payload, summary} JSON
    labels      TEXT,               -- 记录级通用标签 JSON 数组（如 ["recall"]）
    suppressed  INTEGER NOT NULL DEFAULT 0, -- 0 / 1：只进流水，不投递/不唤醒/不计未读
    PRIMARY KEY (connection, room_key, record_id)
);


CREATE TABLE IF NOT EXISTS watermarks (
    session    TEXT    NOT NULL,
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    watermark  INTEGER NOT NULL,
    PRIMARY KEY (session, connection, room_key)
);

-- 乱序确认：水位之上已经被确认、但前面还有缺口的那几条。
CREATE TABLE IF NOT EXISTS acks (
    session    TEXT    NOT NULL,
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    record_id  INTEGER NOT NULL,
    PRIMARY KEY (session, connection, room_key, record_id)
);

CREATE TABLE IF NOT EXISTS deliveries (
    session    TEXT    NOT NULL,
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    record_id  INTEGER NOT NULL,
    times      INTEGER NOT NULL,
    PRIMARY KEY (session, connection, room_key, record_id)
);

CREATE TABLE IF NOT EXISTS notes (
    connection TEXT    NOT NULL,
    room_key   TEXT    NOT NULL,
    kind       TEXT    NOT NULL,   -- term | member | topic
    note_key   TEXT    NOT NULL,
    value      TEXT    NOT NULL,
    label      TEXT,
    status     TEXT,               -- open | closed，仅话题
    corrected  INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (connection, room_key, kind, note_key)
);

CREATE TABLE IF NOT EXISTS session_maps (
    session         TEXT    NOT NULL,   -- acb 会话 id
    harness_session TEXT    NOT NULL,   -- harness 内部会话 id（dsh 的 sessionId）
    meta            TEXT    NOT NULL,   -- 元信息 JSON（标题/模型等，harness 回报什么存什么）
    updated_at      INTEGER NOT NULL,
    PRIMARY KEY (session)
);

-- agent 的反馈（ADR-0021）。指纹相同的合并成一条，次数累加。
CREATE TABLE IF NOT EXISTS feedback (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    fingerprint TEXT    NOT NULL UNIQUE,  -- 去重指纹（类型 + 标题归一化）
    title       TEXT    NOT NULL,         -- 正文第一行，列表里扫这个
    body        TEXT    NOT NULL,         -- 完整正文
    suggestion  TEXT,                     -- 建议怎么改
    kind        TEXT    NOT NULL,         -- bug | gap | friction | idea
    severity    TEXT    NOT NULL,         -- blocker | major | minor
    refs        TEXT    NOT NULL,         -- JSON 数组：水位编号、工具名、错误码
    occurrences INTEGER NOT NULL,         -- agent 自称发生了多少次
    repeats     INTEGER NOT NULL,         -- 被报告了几次
    tool        TEXT,                     -- 出事时在调哪个桥工具
    session     TEXT    NOT NULL,
    harness     TEXT,
    room        TEXT,
    context     TEXT    NOT NULL,         -- 现场 JSON：票、模型、时间
    status      TEXT    NOT NULL,         -- open | acked | resolved | wontfix
    note        TEXT,
    replies     TEXT    NOT NULL DEFAULT '[]',  -- JSON 数组：维护方回复（T7c）
    first_at    INTEGER NOT NULL,
    last_at     INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS feedback_by_status ON feedback (status, last_at DESC);

-- 后台任务与出站尝试（异步执行的地基）。发送类调用执行前先写 running，
-- 成功补 platform_id，超时留 unknown；重启后仍能按 id 查。
CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT    PRIMARY KEY,
    session         TEXT    NOT NULL,
    connection      TEXT    NOT NULL,
    room_key        TEXT    NOT NULL,
    handler         TEXT    NOT NULL,   -- 工具/op 名；尝试记录也是它
    params_summary  TEXT    NOT NULL,   -- 参数摘要，给人看
    wake_on         TEXT    NOT NULL,   -- never | error | always
    status          TEXT    NOT NULL,   -- queued | running | succeeded | failed | unknown
    result          TEXT,               -- JSON
    error           TEXT,
    delivery_error  TEXT,
    event_id        INTEGER,
    event_woke      TEXT    NOT NULL DEFAULT '[]',  -- JSON 数组
    event_error     TEXT,
    platform_id     TEXT,               -- 平台回执编号
    idempotency_key TEXT,               -- 发送类调用的幂等键
    created_at      INTEGER NOT NULL,
    started_at      INTEGER,
    finished_at     INTEGER
);

CREATE INDEX IF NOT EXISTS tasks_by_session ON tasks (session, created_at DESC);
CREATE INDEX IF NOT EXISTS tasks_open ON tasks (status, created_at);

-- 策略（schema 13）。以前 `[grants]` 只能手改 TOML：改错要重启，事后补配置
-- 还救不回已经建出去的票据（2026-11-09：审批路由漏配一个群，票一路 pending
-- 到过期）。从此桥按库判权限、控制台按库改权限。
--
-- 四类对象分表是因为键不同：规则靠 id（审计与判定都要追溯来源），房间靠
-- `qq:group/123`，账号靠 `qq:10001`，两个管理员是单例。payload 存整份 JSON，
-- 形状就是 config.toml 里那一份——读写都走同一个 serde 定义，不会两套字段。
CREATE TABLE IF NOT EXISTS policy_rules (
    id         TEXT PRIMARY KEY,     -- GrantRule.id
    effect     TEXT NOT NULL,        -- allow | deny
    payload    TEXT NOT NULL,        -- GrantRule 的 JSON
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_rooms (
    room_key   TEXT PRIMARY KEY,     -- qq:group/769843907
    payload    TEXT NOT NULL,        -- RoomGrant：trust / members / approval_route
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_accounts (
    account_key TEXT PRIMARY KEY,    -- qq:10001（连接:账号）
    payload     TEXT NOT NULL,       -- AccountGrant：approval_route
    updated_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS policy_admins (
    role       TEXT PRIMARY KEY,     -- bot_admin | system_admin
    payload    TEXT NOT NULL,        -- BotAdminGrant / SystemAdminGrant
    updated_at INTEGER NOT NULL
);

-- 后备模型链：主模型被拒时按 position 从小到大换。顺序本身就是语义，
-- 所以不用自增主键排序（删一条再插会打乱顺序）。
CREATE TABLE IF NOT EXISTS model_fallbacks (
    model_ref  TEXT PRIMARY KEY,     -- provider/model
    position   INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- 业务对象（schema 15）。连接/会话/人格/记忆/harness 从配置文件搬进库。
--
-- 一张表装五类，是因为它们的行为完全一样：按 id 唯一、整类替换、读出来要能
-- 拼回 TOML。分五张表会得到五份一模一样的代码，还要五处一起改。
-- payload 是**这一条的 TOML 原文**（`[[sessions]]` 那一段），不是结构体 JSON：
-- 解析、默认值、`token = { secret = ... }` 的引用解析都走原来那条路。
CREATE TABLE IF NOT EXISTS config_objects (
    kind       TEXT NOT NULL,        -- connection | session | persona | memory | harness
    id         TEXT NOT NULL,        -- 对象自己的 id
    payload    TEXT NOT NULL,        -- 含 [[kind]] 表头的 TOML 原文
    position   INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (kind, id)
);
"#;

/// 库里的家底，启动时打给人看。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub records: u64,
    pub rooms: u64,
    pub notes: u64,
}

/// SQLite 存储。
///
/// 一条连接、单进程使用。桥的所有访问都在那把互斥锁后面，所以不需要连接池；
/// 多进程同时开同一个文件不在支持范围内（WAL 也只保证读不被写挡住）。
pub struct SqliteStore {
    conn: Connection,
}

impl std::fmt::Debug for SqliteStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SqliteStore")
    }
}

impl SqliteStore {
    /// 打开（不存在就新建）。父目录也会顺手建出来。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    backend(acb_i18n::t!(
                        "kernel.storage.create_dir_failed",
                        dir = parent.display(),
                        error = error
                    ))
                })?;
            }
        }
        let conn = Connection::open(path).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.open_failed",
                path = path.display(),
                error = error
            ))
        })?;
        Self::prepare(conn, true)
    }

    /// 全在内存里的 SQLite。测试用：跑的是真的 SQL，但不落盘。
    pub fn open_memory() -> Result<Self, StorageError> {
        let conn = Connection::open_in_memory().map_err(|error| backend(error.to_string()))?;
        Self::prepare(conn, false)
    }

    fn prepare(conn: Connection, on_disk: bool) -> Result<Self, StorageError> {
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|error| backend(error.to_string()))?;
        // WAL 让「读」不被「写」挡住；内存库没有意义，跳过。
        if on_disk {
            conn.execute_batch("PRAGMA journal_mode = WAL;")
                .map_err(|error| {
                    backend(acb_i18n::t!(
                        "kernel.storage.enable_wal_failed",
                        error = error
                    ))
                })?;
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL;")
            .map_err(|error| backend(error.to_string()))?;
        conn.execute_batch(SCHEMA).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.create_tables_failed",
                error = error
            ))
        })?;

        let store = Self { conn };
        store.check_version()?;
        // 索引必须等结构补齐之后再建：第 1 版的库里还没有 `connection` / `platform_id`
        // 这两列，先建索引会直接报「没有这一列」。
        store.ensure_indexes()?;
        Ok(store)
    }

    /// 补索引。`IF NOT EXISTS` 所以每次打开都跑一遍也没成本；老库打开时自己就有。
    fn ensure_indexes(&self) -> Result<(), StorageError> {
        self.conn
            .execute_batch(
                // 引用对齐要按平台编号反查（ADR-0024）。没这个索引就得扫完整个房间。
                "CREATE INDEX IF NOT EXISTS records_platform
                     ON records (connection, room_key, platform_id);",
            )
            .map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.create_indexes_failed",
                    error = error
                ))
            })
    }

    /// 把库升到 [`SCHEMA_VERSION`]。
    ///
    /// 规矩：**先补列再改版本号**，中途失败下次还能接着补。比当前版本新的库直接拒绝——
    /// 老程序往新库里写，就是在拿不认识的结构瞎猜。
    fn check_version(&self) -> Result<(), StorageError> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| backend(error.to_string()))?;

        // 新库（或被清过版本号的库）：按 0 处理，后面照常补列，不会漏。
        let version = match found {
            None => 0,
            Some(raw) => raw.trim().parse::<i64>().map_err(|_| {
                backend(acb_i18n::t!(
                    "kernel.storage.schema_version_unreadable",
                    raw = raw
                ))
            })?,
        };

        if version > SCHEMA_VERSION {
            return Err(backend(acb_i18n::t!(
                "kernel.storage.schema_version_too_new",
                version = version,
                current = SCHEMA_VERSION
            )));
        }

        // 1 → 2：引用回复要判「被引用的是谁发的」，得留下平台消息编号。
        if version < 2 {
            self.add_column_if_missing("records", "reply_to_self", "INTEGER NOT NULL DEFAULT 0")?;
            self.add_column_if_missing("records", "platform_id", "TEXT")?;
        }

        // 2 → 3：房间从「平台 + 房间号」改成「连接 + 路径」。
        //
        // 只改名、不搬数据：老库里 `platform` 那一列本来就是连接名（以前叫平台名），
        // `room` 那一列本来就是路径（以前叫房间号），语义没变，只是名字对上了。
        // `room_json` 是新留的后路——以后房间要换成更自由的形状，原始结构还在这儿。
        if version < 3 {
            for table in ["records", "watermarks", "acks", "deliveries", "notes"] {
                self.rename_column_if_needed(table, "platform", "connection")?;
                self.rename_column_if_needed(table, "room", "room_key")?;
            }
            self.add_column_if_missing("records", "room_json", "TEXT")?;
            // 第 1 版留下了 `records_platform_id`，改名之后它跟下面要建的索引完全重复。
            // 留着只是白占写入开销，删掉。
            self.conn
                .execute_batch("DROP INDEX IF EXISTS records_platform_id;")
                .map_err(|error| {
                    backend(acb_i18n::t!(
                        "kernel.storage.drop_old_indexes_failed",
                        error = error
                    ))
                })?;
        }

        // 3 → 4：device 概念升级为「harness 会话」。只改名不搬数据——`device` 那一列
        // 本来就是「谁的水位/谁的重投计数」，语义没变，只是名字从设备换成会话。
        if version < 4 {
            for table in ["watermarks", "acks", "deliveries"] {
                self.rename_column_if_needed(table, "device", "session")?;
            }
        }

        // 5 → 6：发送者身份。可空，老记录判不出就留空，不必回填。
        if version < 6 {
            self.add_column_if_missing("records", "sender_role", "TEXT")?;
        }

        // 7 → 8：事件语义标签 / 原始载荷 / 人话摘要。老记录没有就留空。
        if version < 8 {
            self.add_column_if_missing("records", "event_meta", "TEXT")?;
        }

        // 8 → 9：记录级通用标签。老记录没有就留空。
        if version < 9 {
            self.add_column_if_missing("records", "labels", "TEXT")?;
        }
        // 兼容这一版早期试验库：结构版本已是 9，但只有 recalled 没有 labels。
        self.add_column_if_missing("records", "labels", "TEXT")?;

        // 9 → 10：被抑制的记录。
        if version < 10 {
            self.add_column_if_missing("records", "suppressed", "INTEGER NOT NULL DEFAULT 0")?;
        }

        // 10 → 11：反馈回复。老反馈没有回复，默认空数组。
        if version < 11 {
            self.add_column_if_missing("feedback", "replies", "TEXT NOT NULL DEFAULT '[]'")?;
        }

        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![SCHEMA_VERSION.to_string()],
            )
            .map_err(|error| backend(error.to_string()))?;

        self.conn
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS records_by_platform_id
                     ON records (connection, room_key, platform_id);",
            )
            .map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.create_indexes_failed",
                    error = error
                ))
            })?;
        Ok(())
    }

    /// 老列在才改名。新库建表时用的已经是新名字，这里就该什么都不做。
    fn rename_column_if_needed(
        &self,
        table: &str,
        old: &str,
        new: &str,
    ) -> Result<(), StorageError> {
        let names = self.columns_of(table)?;
        if !names.iter().any(|name| name == old) {
            return Ok(());
        }
        self.conn
            .execute_batch(&format!(
                "ALTER TABLE {table} RENAME COLUMN {old} TO {new};"
            ))
            .map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.rename_failed",
                    table = table,
                    old = old,
                    new = new,
                    error = error
                ))
            })
    }

    /// 表里有哪些列。
    fn columns_of(&self, table: &str) -> Result<Vec<String>, StorageError> {
        let mut statement = self
            .conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| backend(error.to_string()))?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| backend(error.to_string()))?;
        let mut found = Vec::new();
        for name in names {
            found.push(name.map_err(|error| backend(error.to_string()))?);
        }
        Ok(found)
    }

    /// 列不存在才加。`ALTER TABLE ... ADD COLUMN` 没有 `IF NOT EXISTS`，只能先问一嘴。
    fn add_column_if_missing(
        &self,
        table: &str,
        column: &str,
        definition: &str,
    ) -> Result<(), StorageError> {
        let names = self.columns_of(table)?;
        if names.iter().any(|name| name == column) {
            return Ok(());
        }
        self.conn
            .execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition};"
            ))
            .map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.add_column_failed",
                    table = table,
                    column = column,
                    error = error
                ))
            })
    }

    /// 库里有多少东西。只给启动摘要用，不算进 [`Store`] 约束。
    pub fn counts(&self) -> Result<Counts, StorageError> {
        let records = self.count("SELECT COUNT(*) FROM records")?;
        let rooms =
            self.count("SELECT COUNT(*) FROM (SELECT DISTINCT connection, room_key FROM records)")?;
        let notes = self.count("SELECT COUNT(*) FROM notes")?;
        Ok(Counts {
            records,
            rooms,
            notes,
        })
    }

    fn count(&self, sql: &str) -> Result<u64, StorageError> {
        let value: i64 = self
            .conn
            .query_row(sql, [], |row| row.get(0))
            .map_err(|error| backend(error.to_string()))?;
        Ok(value.max(0) as u64)
    }
}

fn backend(detail: impl Into<String>) -> StorageError {
    StorageError::Backend(detail.into())
}

fn io_error(error: rusqlite::Error) -> StorageError {
    backend(error.to_string())
}

impl Transcript for SqliteStore {
    fn append(&mut self, draft: RecordDraft) -> Result<Record, StorageError> {
        let (kind, event, event_meta) = match &draft.kind {
            RecordKind::Message => ("message", None, None),
            RecordKind::Event {
                event,
                tags,
                payload,
                summary,
                body,
            } => (
                "event",
                Some(event.clone()),
                Some(
                    serde_json::to_string(&StoredEventMeta {
                        tags: tags.clone(),
                        payload: payload.clone(),
                        summary: summary.clone(),
                        body: *body,
                    })
                    .map_err(|error| {
                        backend(acb_i18n::t!(
                            "kernel.storage.store_event_metadata_failed",
                            error = error
                        ))
                    })?,
                ),
            ),
        };
        let elements = serde_json::to_string(&draft.elements).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_elements_failed",
                error = error
            ))
        })?;

        // 路径的原始结构也存一份：以后房间要换成更自由的形状，原始数据还在，
        // 重新算键是一次脚本的事，不用重导。
        let room_json = serde_json::to_string(&draft.room.path).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_room_path_failed",
                error = error
            ))
        })?;

        let transaction = self.conn.transaction().map_err(io_error)?;
        // 编号是「房间内」的：同一个房间接着往上数，不同连接同号互不相干。
        let next: i64 = transaction
            .query_row(
                "SELECT COALESCE(MAX(record_id), 0) + 1 FROM records WHERE connection = ?1 AND room_key = ?2",
                params![draft.room.connection, draft.room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;

        transaction
            .execute(
                "INSERT INTO records
                 (connection, room_key, room_json, record_id, kind, event, sender_id, sender_name,
                  sender_role, at, at_self, reply_to_self, platform_id, elements, event_meta, labels,
                  suppressed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, NULL, ?16)",
                params![
                    draft.room.connection,
                    draft.room.key(),
                    room_json,
                    next,
                    kind,
                    event,
                    draft.sender.id,
                    draft.sender.name,
                    draft.sender.role.map(|role| role_str(&role)),
                    draft.at,
                    if draft.at_self { 1 } else { 0 },
                    if draft.reply_to_self { 1 } else { 0 },
                    draft.platform_id,
                    elements,
                    event_meta,
                    if draft.suppressed { 1 } else { 0 },
                ],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;

        Ok(Record {
            id: next.max(0) as u64,
            room: draft.room,
            kind: draft.kind,
            sender: draft.sender,
            at: draft.at,
            elements: draft.elements,
            at_self: draft.at_self,
            reply_to_self: draft.reply_to_self,
            platform_id: draft.platform_id,
            quote_ref: None,
            labels: Vec::new(),
            suppressed: draft.suppressed,
            // 流水里只有原话；重投的旁白由投递时补（见 engine::open_turn）。
            redelivery: None,
            note: None,
        })
    }

    fn since(&self, room: &RoomId, after: u64, limit: usize) -> Result<Vec<Record>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT record_id, kind, event, sender_id, sender_name, sender_role, at, at_self,
                        reply_to_self, platform_id, elements, event_meta, labels, suppressed
                 FROM records
                 WHERE connection = ?1 AND room_key = ?2 AND record_id > ?3
                 ORDER BY record_id
                 LIMIT ?4",
            )
            .map_err(io_error)?;

        let rows = statement
            .query_map(
                params![room.connection, room.key(), after as i64, limit as i64],
                |row| {
                    Ok(RawRow {
                        id: row.get::<_, i64>(0)?.max(0) as u64,
                        kind: row.get(1)?,
                        event: row.get(2)?,
                        sender_id: row.get(3)?,
                        sender_name: row.get(4)?,
                        sender_role: row.get(5)?,
                        at: row.get(6)?,
                        at_self: row.get::<_, i64>(7)? != 0,
                        reply_to_self: row.get::<_, i64>(8)? != 0,
                        platform_id: row.get(9)?,
                        elements: row.get(10)?,
                        event_meta: row.get(11)?,
                        labels: row.get(12)?,
                        suppressed: row.get::<_, i64>(13)? != 0,
                    })
                },
            )
            .map_err(io_error)?;

        let mut records = Vec::new();
        for row in rows {
            let raw = row.map_err(io_error)?;
            records.push(raw.into_record(room)?);
        }
        Ok(records)
    }

    fn latest(&self, room: &RoomId) -> Result<Option<u64>, StorageError> {
        let max: Option<i64> = self
            .conn
            .query_row(
                "SELECT MAX(record_id) FROM records WHERE connection = ?1 AND room_key = ?2",
                params![room.connection, room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(max.map(|value| value.max(0) as u64))
    }

    fn rooms(&self) -> Result<Vec<RoomId>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT DISTINCT connection, room_key FROM records ORDER BY connection, room_key",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(io_error)?;

        let mut rooms = Vec::new();
        for row in rows {
            let (connection, room_key) = row.map_err(io_error)?;
            let room = RoomId::new(&connection, &room_key).map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.unrecognized_room",
                    connection = connection,
                    room_key = room_key,
                    error = error
                ))
            })?;
            rooms.push(room);
        }
        Ok(rooms)
    }

    fn count(&self, room: &RoomId) -> Result<u64, StorageError> {
        let value: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM records WHERE connection = ?1 AND room_key = ?2",
                params![room.connection, room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(value.max(0) as u64)
    }

    fn located(&self, room: &RoomId, platform_id: &str) -> Result<Option<Record>, StorageError> {
        if platform_id.is_empty() {
            return Ok(None);
        }
        let row = self
            .conn
            .query_row(
                // 强制走 platform 索引：不指定的话 SQLite 会选主键索引（省掉排序），
                // 于是引用一条很老的消息时要一条条往回扫。这里相等条件命中的通常只有
                // 一行，多一次微小的排序换「不扫全房间」是划算的。
                "SELECT record_id, kind, event, sender_id, sender_name, sender_role, at, at_self,
                        reply_to_self, platform_id, elements, event_meta, labels, suppressed
                 FROM records INDEXED BY records_platform
                 WHERE connection = ?1 AND room_key = ?2 AND platform_id = ?3
                 ORDER BY record_id DESC
                 LIMIT 1",
                params![room.connection, room.key(), platform_id],
                |row| {
                    Ok(RawRow {
                        id: row.get::<_, i64>(0)?.max(0) as u64,
                        kind: row.get(1)?,
                        event: row.get(2)?,
                        sender_id: row.get(3)?,
                        sender_name: row.get(4)?,
                        sender_role: row.get(5)?,
                        at: row.get(6)?,
                        at_self: row.get::<_, i64>(7)? != 0,
                        reply_to_self: row.get::<_, i64>(8)? != 0,
                        platform_id: row.get(9)?,
                        elements: row.get(10)?,
                        event_meta: row.get(11)?,
                        labels: row.get(12)?,
                        suppressed: row.get::<_, i64>(13)? != 0,
                    })
                },
            )
            .optional()
            .map_err(io_error)?;

        match row {
            Some(raw) => Ok(Some(raw.into_record(room)?)),
            None => Ok(None),
        }
    }

    fn exists_elsewhere(&self, room: &RoomId, platform_id: &str) -> Result<bool, StorageError> {
        if platform_id.is_empty() {
            return Ok(false);
        }
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM records
                 WHERE connection = ?1 AND platform_id = ?2 AND room_key <> ?3
                 LIMIT 1",
                params![room.connection, platform_id, room.key()],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        Ok(found.is_some())
    }

    fn add_labels(
        &mut self,
        room: &RoomId,
        platform_id: &str,
        labels: &[String],
    ) -> Result<bool, StorageError> {
        if platform_id.is_empty() || labels.is_empty() {
            return Ok(false);
        }
        let row: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT record_id, labels FROM records
                 WHERE connection = ?1 AND room_key = ?2 AND platform_id = ?3
                 ORDER BY record_id DESC
                 LIMIT 1",
                params![room.connection, room.key(), platform_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(io_error)?;
        let Some((record_id, raw)) = row else {
            return Ok(false);
        };
        let mut current: Vec<String> = raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.read_labels_failed",
                    error = error
                ))
            })?
            .unwrap_or_default();
        let before = current.len();
        for label in labels {
            if !current.contains(label) {
                current.push(label.clone());
            }
        }
        if current.len() == before {
            return Ok(false);
        }
        let text = serde_json::to_string(&current).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_labels_failed",
                error = error
            ))
        })?;
        self.conn
            .execute(
                "UPDATE records SET labels = ?4
                 WHERE connection = ?1 AND room_key = ?2 AND record_id = ?3",
                params![room.connection, room.key(), record_id, text],
            )
            .map_err(io_error)?;
        Ok(true)
    }

    fn prune(&mut self, room: &RoomId, plan: &PrunePlan) -> Result<u64, StorageError> {
        let keep = plan.keep_newest.max(MIN_KEPT) as i64;
        let transaction = self.conn.transaction().map_err(io_error)?;

        // 三个条件同时成立才删：早于时间线、低于编号安全线、排不进最新的 keep 条。
        // `LIMIT -1` 在 SQLite 里表示「不设上限」，所以 keep_newest 传 0 时会退化成一条都不删，
        // 但上面的 `max(MIN_KEPT)` 已经把它抬到 1 了。
        let deleted = transaction
            .execute(
                "DELETE FROM records
                 WHERE connection = ?1 AND room_key = ?2
                   AND record_id < ?3
                   AND (?4 IS NULL OR at < ?4)
                   AND record_id NOT IN (
                       SELECT record_id FROM records
                       WHERE connection = ?1 AND room_key = ?2
                       ORDER BY record_id DESC
                       LIMIT ?5
                   )",
                params![
                    room.connection,
                    room.key(),
                    plan.below as i64,
                    plan.older_than,
                    keep
                ],
            )
            .map_err(io_error)?;

        if deleted > 0 {
            // 指向已删记录的账要一起清：投递计数没意义了；乱序确认也不可能再推进水位
            // （它前面那条已经不在了，前沿永远等不到）。
            for table in ["deliveries", "acks"] {
                transaction
                    .execute(
                        &format!(
                            "DELETE FROM {table}
                             WHERE connection = ?1 AND room_key = ?2
                               AND record_id NOT IN (
                                   SELECT record_id FROM records
                                   WHERE connection = ?1 AND room_key = ?2
                               )"
                        ),
                        params![room.connection, room.key()],
                    )
                    .map_err(io_error)?;
            }
        }

        transaction.commit().map_err(io_error)?;
        Ok(deleted as u64)
    }
}

/// 身份 → 库里的字符串。跟 [`SenderRole`] 的 serde 表示保持一致。
fn role_str(role: &SenderRole) -> &'static str {
    match role {
        SenderRole::SystemAdmin => "system_admin",
        SenderRole::Owner => "owner",
        SenderRole::Admin => "admin",
    }
}

/// 库里的字符串 → 身份。不认识的就当没判出来（宁可少标，别瞎标）。
fn role_from_str(text: &str) -> Option<SenderRole> {
    match text {
        "system_admin" => Some(SenderRole::SystemAdmin),
        "owner" => Some(SenderRole::Owner),
        "admin" => Some(SenderRole::Admin),
        _ => None,
    }
}

/// 事件记录附加的语义信息（`event_meta` 列）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEventMeta {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    /// 旧记录没有这个字段，按「有正文」读；新写入的拍一拍会明确写 `false`。
    #[serde(default = "body_default")]
    body: bool,
}

fn body_default() -> bool {
    true
}

impl Default for StoredEventMeta {
    fn default() -> Self {
        Self {
            tags: Vec::new(),
            payload: None,
            summary: None,
            body: true,
        }
    }
}

/// 从库里读出来的一行，还没变回 [`Record`]。
struct RawRow {
    id: u64,
    kind: String,
    event: Option<String>,
    sender_id: String,
    sender_name: String,
    sender_role: Option<String>,
    at: i64,
    at_self: bool,
    reply_to_self: bool,
    platform_id: Option<String>,
    elements: String,
    event_meta: Option<String>,
    labels: Option<String>,
    suppressed: bool,
}

impl RawRow {
    fn into_record(self, room: &RoomId) -> Result<Record, StorageError> {
        let kind = match self.kind.as_str() {
            "message" => RecordKind::Message,
            "event" => {
                let meta: StoredEventMeta = self
                    .event_meta
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()
                    .map_err(|error| {
                        backend(acb_i18n::t!(
                            "kernel.storage.read_event_metadata_failed",
                            error = error
                        ))
                    })?
                    .unwrap_or_default();
                RecordKind::Event {
                    event: self.event.unwrap_or_default(),
                    tags: meta.tags,
                    payload: meta.payload,
                    summary: meta.summary,
                    body: meta.body,
                }
            }
            other => {
                return Err(backend(acb_i18n::t!(
                    "kernel.storage.unrecognized_record_kind",
                    kind = other
                )));
            }
        };
        let elements: Vec<Element> = serde_json::from_str(&self.elements).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.read_elements_failed",
                error = error
            ))
        })?;
        let labels: Vec<String> = self
            .labels
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.read_labels_failed",
                    error = error
                ))
            })?
            .unwrap_or_default();

        Ok(Record {
            id: self.id,
            room: room.clone(),
            kind,
            sender: Sender {
                id: self.sender_id,
                name: self.sender_name,
                role: self.sender_role.as_deref().and_then(role_from_str),
            },
            at: self.at,
            elements,
            at_self: self.at_self,
            reply_to_self: self.reply_to_self,
            platform_id: self.platform_id,
            quote_ref: None,
            labels,
            suppressed: self.suppressed,
            redelivery: None,
            note: None,
        })
    }
}

impl Watermarks for SqliteStore {
    fn watermark(&self, cursor: &Cursor) -> Result<u64, StorageError> {
        let value: Option<i64> = self
            .conn
            .query_row(
                "SELECT watermark FROM watermarks
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3",
                params![cursor.session, cursor.room.connection, cursor.room.key()],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        Ok(value.unwrap_or(0).max(0) as u64)
    }

    fn ack(&mut self, cursor: &Cursor, ids: &[u64]) -> Result<u64, StorageError> {
        let transaction = self.conn.transaction().map_err(io_error)?;
        let current: Option<i64> = transaction
            .query_row(
                "SELECT watermark FROM watermarks
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3",
                params![cursor.session, cursor.room.connection, cursor.room.key()],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        let current = current.unwrap_or(0).max(0) as u64;

        let latest: Option<i64> = transaction
            .query_row(
                "SELECT MAX(record_id) FROM records WHERE connection = ?1 AND room_key = ?2",
                params![cursor.room.connection, cursor.room.key()],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        let latest = latest.unwrap_or(0).max(0) as u64;

        // 先全验一遍再写：拿错编号就整批拒绝，别把一半写进去、留个半截状态。
        for &id in ids {
            if id > latest {
                return Err(StorageError::UnknownRecord {
                    room: cursor.room.to_string(),
                    id,
                });
            }
        }

        for &id in ids {
            if id > current {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO acks (session, connection, room_key, record_id)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![
                            cursor.session,
                            cursor.room.connection,
                            cursor.room.key(),
                            id as i64
                        ],
                    )
                    .map_err(io_error)?;
            }
        }

        // 水位只推连续前沿：中间缺一条就停在缺口前。被抑制的记录从不投递，
        // 没人会确认它，所以它不构成缺口——水位直接越过去。
        let mut frontier = current;
        loop {
            let next = frontier + 1;
            let present: Option<i64> = transaction
                .query_row(
                    "SELECT record_id FROM acks
                     WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                    params![
                        cursor.session,
                        cursor.room.connection,
                        cursor.room.key(),
                        next as i64
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(io_error)?;
            if present.is_some() {
                transaction
                    .execute(
                        "DELETE FROM acks
                         WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                        params![
                            cursor.session,
                            cursor.room.connection,
                            cursor.room.key(),
                            next as i64
                        ],
                    )
                    .map_err(io_error)?;
                frontier = next;
                continue;
            }
            let suppressed: Option<i64> = transaction
                .query_row(
                    "SELECT suppressed FROM records
                     WHERE connection = ?1 AND room_key = ?2 AND record_id = ?3",
                    params![cursor.room.connection, cursor.room.key(), next as i64],
                    |row| row.get(0),
                )
                .optional()
                .map_err(io_error)?;
            if suppressed == Some(1) {
                frontier = next;
                continue;
            }
            break;
        }

        transaction
            .execute(
                "INSERT INTO watermarks (session, connection, room_key, watermark)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session, connection, room_key) DO UPDATE SET watermark = excluded.watermark",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    frontier as i64
                ],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;

        Ok(frontier)
    }

    fn acked(&self, cursor: &Cursor) -> Result<Vec<u64>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT record_id FROM acks
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3
                 ORDER BY record_id",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(
                params![cursor.session, cursor.room.connection, cursor.room.key()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(io_error)?;

        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(io_error)?.max(0) as u64);
        }
        Ok(ids)
    }
}

impl Notes for SqliteStore {
    fn note(&self, key: &NoteKey) -> Result<Option<Note>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT value, label, status, corrected, updated_at FROM notes
                 WHERE connection = ?1 AND room_key = ?2 AND kind = ?3 AND note_key = ?4",
                params![
                    key.room.connection,
                    key.room.key(),
                    key.kind.as_str(),
                    key.key
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(io_error)?;

        let Some((value, label, status, corrected, updated_at)) = row else {
            return Ok(None);
        };
        Ok(Some(Note {
            key: key.clone(),
            value,
            label,
            status: status.as_deref().and_then(parse_status),
            corrected,
            updated_at,
        }))
    }

    fn upsert_note(&mut self, note: Note) -> Result<(), StorageError> {
        self.conn
            .execute(
                "INSERT INTO notes
                 (connection, room_key, kind, note_key, value, label, status, corrected, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (connection, room_key, kind, note_key) DO UPDATE SET
                     value = excluded.value,
                     label = excluded.label,
                     status = excluded.status,
                     corrected = excluded.corrected,
                     updated_at = excluded.updated_at",
                params![
                    note.key.room.connection,
                    note.key.room.key(),
                    note.key.kind.as_str(),
                    note.key.key,
                    note.value,
                    note.label,
                    note.status.map(status_str),
                    if note.corrected { 1 } else { 0 },
                    note.updated_at,
                ],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn delete_note(&mut self, key: &NoteKey) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM notes WHERE connection = ?1 AND room_key = ?2 AND kind = ?3 AND note_key = ?4",
                params![
                    key.room.connection,
                    key.room.key(),
                    key.kind.as_str(),
                    key.key
                ],
            )
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn notes_in(&self, room: &RoomId) -> Result<Vec<Note>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT kind, note_key, value, label, status, corrected, updated_at
                 FROM notes WHERE connection = ?1 AND room_key = ?2
                 ORDER BY kind, note_key",
            )
            .map_err(io_error)?;

        let rows = statement
            .query_map(params![room.connection, room.key()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)? != 0,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(io_error)?;

        let mut notes = Vec::new();
        for row in rows {
            let (kind, key, value, label, status, corrected, updated_at) = row.map_err(io_error)?;
            let Some(kind) = NoteKind::parse(&kind) else {
                return Err(backend(acb_i18n::t!(
                    "kernel.storage.unrecognized_note_kind",
                    kind = kind
                )));
            };
            notes.push(Note {
                key: NoteKey::new(room.clone(), kind, key),
                value,
                label,
                status: status.as_deref().and_then(parse_status),
                corrected,
                updated_at,
            });
        }
        Ok(notes)
    }
}

impl Deliveries for SqliteStore {
    fn count_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<u32, StorageError> {
        self.conn
            .execute(
                "INSERT INTO deliveries (session, connection, room_key, record_id, times)
                 VALUES (?1, ?2, ?3, ?4, 1)
                 ON CONFLICT (session, connection, room_key, record_id)
                 DO UPDATE SET times = times + 1",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    id as i64
                ],
            )
            .map_err(io_error)?;

        let times: i64 = self
            .conn
            .query_row(
                "SELECT times FROM deliveries
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    id as i64
                ],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        Ok(times.max(1) as u32)
    }

    fn forget_delivery(&mut self, cursor: &Cursor, id: u64) -> Result<(), StorageError> {
        self.conn
            .execute(
                "DELETE FROM deliveries
                 WHERE session = ?1 AND connection = ?2 AND room_key = ?3 AND record_id = ?4",
                params![
                    cursor.session,
                    cursor.room.connection,
                    cursor.room.key(),
                    id as i64
                ],
            )
            .map_err(io_error)?;
        Ok(())
    }
}

impl SessionMaps for SqliteStore {
    fn session_map(&self, session: &str) -> Result<Option<SessionMap>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT harness_session, meta, updated_at FROM session_maps WHERE session = ?1",
                params![session],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(io_error)?;

        let Some((harness_session, meta, updated_at)) = row else {
            return Ok(None);
        };
        let meta: serde_json::Value = serde_json::from_str(&meta).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.parse_session_map_meta_failed",
                error = error
            ))
        })?;
        Ok(Some(SessionMap {
            session: session.to_owned(),
            harness_session,
            meta,
            updated_at,
        }))
    }

    fn upsert_session_map(&mut self, map: SessionMap) -> Result<(), StorageError> {
        let meta = serde_json::to_string(&map.meta).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_session_map_meta_failed",
                error = error
            ))
        })?;
        self.conn
            .execute(
                "INSERT INTO session_maps (session, harness_session, meta, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (session) DO UPDATE SET
                     harness_session = excluded.harness_session,
                     meta = excluded.meta,
                     updated_at = excluded.updated_at",
                params![map.session, map.harness_session, meta, map.updated_at],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn delete_session_map(&mut self, session: &str) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM session_maps WHERE session = ?1",
                params![session],
            )
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn session_maps(&self) -> Result<Vec<SessionMap>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT session, harness_session, meta, updated_at FROM session_maps
                 ORDER BY session",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(io_error)?;

        let mut maps = Vec::new();
        for row in rows {
            let (session, harness_session, meta, updated_at) = row.map_err(io_error)?;
            let meta: serde_json::Value = serde_json::from_str(&meta).map_err(|error| {
                backend(acb_i18n::t!(
                    "kernel.storage.parse_session_map_meta_failed",
                    error = error
                ))
            })?;
            maps.push(SessionMap {
                session,
                harness_session,
                meta,
                updated_at,
            });
        }
        Ok(maps)
    }
}

impl Feedbacks for SqliteStore {
    fn record_feedback(&mut self, draft: FeedbackDraft) -> Result<Recorded, StorageError> {
        let refs = serde_json::to_string(&draft.refs).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_feedback_origin_failed",
                error = error
            ))
        })?;
        let context = serde_json::to_string(&draft.context).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_feedback_context_failed",
                error = error
            ))
        })?;

        // 先看指纹在不在。在就合并，不在就新建——比 upsert 多一次查询，但两种情况的
        // 返回不一样（新建要给新 id，合并要给累计次数），分开写更好读。
        let existing: Option<(i64, i64)> = self
            .conn
            .query_row(
                "SELECT id, occurrences FROM feedback WHERE fingerprint = ?1",
                params![draft.fingerprint],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(io_error)?;

        if let Some((id, occurrences)) = existing {
            self.conn
                .execute(
                    "UPDATE feedback
                        SET repeats = repeats + 1,
                            occurrences = ?2,
                            last_at = ?3,
                            -- 类型/严重度按 ADR-0021 保留第一次的（人已经看过那一行）。
                            -- 但「空着的字段」不是分类，是缺信息：第一次没填、
                            -- 或者那次参数被解析器吞了，后来的报告带全了就该补上。
                            refs = CASE WHEN refs IN ('[]', '') THEN ?4 ELSE refs END,
                            suggestion = COALESCE(NULLIF(suggestion, ''), ?5),
                            tool = COALESCE(NULLIF(tool, ''), ?6),
                            -- 又出现了：原来标了已修/不修说明没修好，重新打开。
                            status = CASE WHEN status IN ('resolved','wontfix') THEN 'open' ELSE status END
                      WHERE id = ?1",
                    params![id, occurrences + draft.occurrences as i64, draft.at, refs, draft.suggestion, draft.tool],
                )
                .map_err(io_error)?;
            let (repeats, occurrences): (i64, i64) = self
                .conn
                .query_row(
                    "SELECT repeats, occurrences FROM feedback WHERE id = ?1",
                    params![id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(io_error)?;
            return Ok(Recorded {
                id,
                repeats: repeats as u32,
                occurrences: occurrences as u32,
                duplicate: true,
            });
        }

        self.conn
            .execute(
                "INSERT INTO feedback
                    (fingerprint, title, body, suggestion, kind, severity, refs,
                     occurrences, repeats, tool, session, harness, room, context,
                     status, note, first_at, last_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11, ?12, ?13,
                         'open', NULL, ?14, ?14)",
                params![
                    draft.fingerprint,
                    draft.title,
                    draft.body,
                    draft.suggestion,
                    draft.kind,
                    draft.severity,
                    refs,
                    draft.occurrences as i64,
                    draft.tool,
                    draft.session,
                    draft.harness,
                    draft.room,
                    context,
                    draft.at,
                ],
            )
            .map_err(io_error)?;

        let id = self.conn.last_insert_rowid();
        self.cap_feedback()?;
        Ok(Recorded {
            id,
            repeats: 1,
            occurrences: draft.occurrences,
            duplicate: false,
        })
    }

    fn feedback(&self, status: Option<&str>, limit: usize) -> Result<Vec<Feedback>, StorageError> {
        // 不筛状态时把 `?1` 绑成空串，条件恒真——比按条件拼两条 SQL 更容易看出差别在哪。
        let filter = status.unwrap_or("");
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, title, body, suggestion, kind, severity, refs, occurrences, repeats,
                        tool, session, harness, room, context, status, note, replies,
                        first_at, last_at
                   FROM feedback
                  WHERE (?1 = '' OR status = ?1)
                  ORDER BY last_at DESC
                  LIMIT ?2",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(params![filter, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<String>>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, String>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, Option<String>>(15)?,
                    row.get::<_, String>(16)?,
                    row.get::<_, i64>(17)?,
                    row.get::<_, i64>(18)?,
                ))
            })
            .map_err(io_error)?;

        let mut items = Vec::new();
        for row in rows {
            let row = row.map_err(io_error)?;
            items.push(Feedback {
                id: row.0,
                title: row.1,
                body: row.2,
                suggestion: row.3,
                kind: row.4,
                severity: row.5,
                refs: serde_json::from_str(&row.6).unwrap_or_default(),
                occurrences: row.7.max(0) as u32,
                repeats: row.8.max(0) as u32,
                tool: row.9,
                session: row.10,
                harness: row.11,
                room: row.12,
                context: serde_json::from_str(&row.13).unwrap_or(serde_json::Value::Null),
                status: row.14,
                note: row.15,
                replies: serde_json::from_str(&row.16).unwrap_or_default(),
                first_at: row.17,
                last_at: row.18,
            });
        }
        Ok(items)
    }

    fn set_feedback_status(
        &mut self,
        id: i64,
        status: &str,
        note: Option<&str>,
    ) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "UPDATE feedback SET status = ?2, note = COALESCE(?3, note) WHERE id = ?1",
                params![id, status, note],
            )
            .map_err(io_error)?;
        Ok(changed > 0)
    }

    fn feedback_by_id(&self, id: i64) -> Result<Option<Feedback>, StorageError> {
        let mut items = self.feedback(None, usize::MAX)?;
        Ok(items.drain(..).find(|item| item.id == id))
    }

    fn reply_feedback(&mut self, id: i64, reply: FeedbackReply) -> Result<bool, StorageError> {
        let replies: Option<String> = self
            .conn
            .query_row(
                "SELECT replies FROM feedback WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        let Some(raw) = replies else {
            return Ok(false);
        };
        let mut parsed: Vec<FeedbackReply> = serde_json::from_str(&raw).unwrap_or_default();
        parsed.push(reply);
        let encoded = serde_json::to_string(&parsed).map_err(|error| {
            backend(acb_i18n::t!(
                "kernel.storage.store_feedback_replies_failed",
                error = error
            ))
        })?;
        self.conn
            .execute(
                "UPDATE feedback SET replies = ?2 WHERE id = ?1",
                params![id, encoded],
            )
            .map_err(io_error)?;
        Ok(true)
    }
}

impl SqliteStore {
    /// 反馈条数别无限长：超了先扔最久没被提过的。
    fn cap_feedback(&self) -> Result<(), StorageError> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM feedback", [], |row| row.get(0))
            .map_err(io_error)?;
        if total <= FEEDBACK_CAP as i64 {
            return Ok(());
        }
        self.conn
            .execute(
                "DELETE FROM feedback WHERE id IN (
                     SELECT id FROM feedback ORDER BY last_at ASC LIMIT ?1
                 )",
                params![total - FEEDBACK_CAP as i64],
            )
            .map_err(io_error)?;
        Ok(())
    }
}

fn status_str(status: TopicStatus) -> &'static str {
    match status {
        TopicStatus::Open => "open",
        TopicStatus::Closed => "closed",
    }
}

fn parse_status(text: &str) -> Option<TopicStatus> {
    match text {
        "open" => Some(TopicStatus::Open),
        "closed" => Some(TopicStatus::Closed),
        _ => None,
    }
}

/// 一行 `tasks` 记录的原始列。先取出来，再统一把 JSON / 房间路径转成强类型，
/// 免得在 `query_map` 的闭包里处理错误类型。
struct TaskRow {
    id: String,
    session: String,
    connection: String,
    room_key: String,
    handler: String,
    params_summary: String,
    wake_on: String,
    status: String,
    result: Option<String>,
    error: Option<String>,
    delivery_error: Option<String>,
    event_id: Option<i64>,
    event_woke: String,
    event_error: Option<String>,
    platform_id: Option<String>,
    idempotency_key: Option<String>,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
}

impl TaskRow {
    fn into_record(self) -> Result<TaskRecord, StorageError> {
        let Some(status) = TaskStatus::parse(&self.status) else {
            return Err(backend(format!(
                "任务记录 {} 的状态无法识别：{}",
                self.id, self.status
            )));
        };
        let room = RoomId::new(&self.connection, &self.room_key)
            .map_err(|error| backend(format!("任务记录 {} 的房间无法还原：{error}", self.id)))?;
        let result = match self.result {
            Some(text) => Some(serde_json::from_str(&text).map_err(|error| {
                backend(format!("任务记录 {} 的结果 JSON 坏了：{error}", self.id))
            })?),
            None => None,
        };
        let event_woke: Vec<String> = serde_json::from_str(&self.event_woke).unwrap_or_default();
        Ok(TaskRecord {
            id: self.id,
            session: self.session,
            room,
            handler: self.handler,
            params_summary: self.params_summary,
            wake_on: self.wake_on,
            status,
            created_at: self.created_at,
            started_at: self.started_at,
            finished_at: self.finished_at,
            result,
            error: self.error,
            delivery_error: self.delivery_error,
            event_id: self.event_id.map(|value| value.max(0) as u64),
            event_woke,
            event_error: self.event_error,
            platform_id: self.platform_id,
            idempotency_key: self.idempotency_key,
        })
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: row.get(0)?,
        session: row.get(1)?,
        connection: row.get(2)?,
        room_key: row.get(3)?,
        handler: row.get(4)?,
        params_summary: row.get(5)?,
        wake_on: row.get(6)?,
        status: row.get(7)?,
        result: row.get(8)?,
        error: row.get(9)?,
        delivery_error: row.get(10)?,
        event_id: row.get(11)?,
        event_woke: row.get(12)?,
        event_error: row.get(13)?,
        platform_id: row.get(14)?,
        idempotency_key: row.get(15)?,
        created_at: row.get(16)?,
        started_at: row.get(17)?,
        finished_at: row.get(18)?,
    })
}

impl Tasks for SqliteStore {
    fn task_create(&mut self, record: &TaskRecord) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO tasks (
                    id, session, connection, room_key, handler, params_summary, wake_on, status,
                    result, error, delivery_error, event_id, event_woke, event_error,
                    platform_id, idempotency_key, created_at, started_at, finished_at
                 ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
                 )",
                params![
                    record.id,
                    record.session,
                    record.room.connection,
                    record.room.key(),
                    record.handler,
                    record.params_summary,
                    record.wake_on,
                    record.status.as_str(),
                    record.result.as_ref().map(|value| value.to_string()),
                    record.error,
                    record.delivery_error,
                    record.event_id.map(|value| value as i64),
                    serde_json::to_string(&record.event_woke).unwrap_or_else(|_| "[]".to_owned()),
                    record.event_error,
                    record.platform_id,
                    record.idempotency_key,
                    record.created_at,
                    record.started_at,
                    record.finished_at,
                ],
            )
            .map_err(io_error)?;
        Ok(changed > 0)
    }

    fn task_update(&mut self, record: &TaskRecord) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "UPDATE tasks SET
                    session = ?2, connection = ?3, room_key = ?4, handler = ?5,
                    params_summary = ?6, wake_on = ?7, status = ?8, result = ?9, error = ?10,
                    delivery_error = ?11, event_id = ?12, event_woke = ?13, event_error = ?14,
                    platform_id = ?15, idempotency_key = ?16, created_at = ?17,
                    started_at = ?18, finished_at = ?19
                 WHERE id = ?1",
                params![
                    record.id,
                    record.session,
                    record.room.connection,
                    record.room.key(),
                    record.handler,
                    record.params_summary,
                    record.wake_on,
                    record.status.as_str(),
                    record.result.as_ref().map(|value| value.to_string()),
                    record.error,
                    record.delivery_error,
                    record.event_id.map(|value| value as i64),
                    serde_json::to_string(&record.event_woke).unwrap_or_else(|_| "[]".to_owned()),
                    record.event_error,
                    record.platform_id,
                    record.idempotency_key,
                    record.created_at,
                    record.started_at,
                    record.finished_at,
                ],
            )
            .map_err(io_error)?;
        Ok(changed > 0)
    }

    fn task_get(&self, session: &str, id: &str) -> Result<Option<TaskRecord>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, session, connection, room_key, handler, params_summary, wake_on, status,
                        result, error, delivery_error, event_id, event_woke, event_error,
                        platform_id, idempotency_key, created_at, started_at, finished_at
                   FROM tasks WHERE session = ?1 AND id = ?2",
                params![session, id],
                row_to_task,
            )
            .optional()
            .map_err(io_error)?;
        row.map(TaskRow::into_record).transpose()
    }

    fn task_find(&self, id: &str) -> Result<Option<TaskRecord>, StorageError> {
        let row = self
            .conn
            .query_row(
                "SELECT id, session, connection, room_key, handler, params_summary, wake_on, status,
                        result, error, delivery_error, event_id, event_woke, event_error,
                        platform_id, idempotency_key, created_at, started_at, finished_at
                   FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(io_error)?;
        row.map(TaskRow::into_record).transpose()
    }

    fn task_list(
        &self,
        session: &str,
        status: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<TaskRecord>, usize), StorageError> {
        let filter = status.unwrap_or("");
        let total: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE session = ?1 AND (?2 = '' OR status = ?2)",
                params![session, filter],
                |row| row.get(0),
            )
            .map_err(io_error)?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, session, connection, room_key, handler, params_summary, wake_on, status,
                        result, error, delivery_error, event_id, event_woke, event_error,
                        platform_id, idempotency_key, created_at, started_at, finished_at
                   FROM tasks WHERE session = ?1 AND (?2 = '' OR status = ?2)
                  ORDER BY created_at DESC, id DESC
                  LIMIT ?3 OFFSET ?4",
            )
            .map_err(io_error)?;
        let rows = statement
            .query_map(
                params![session, filter, limit as i64, offset as i64],
                row_to_task,
            )
            .map_err(io_error)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(io_error)?.into_record()?);
        }
        Ok((items, total.max(0) as usize))
    }

    fn task_open(&self) -> Result<Vec<TaskRecord>, StorageError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT id, session, connection, room_key, handler, params_summary, wake_on, status,
                        result, error, delivery_error, event_id, event_woke, event_error,
                        platform_id, idempotency_key, created_at, started_at, finished_at
                   FROM tasks WHERE status IN ('pending', 'queued', 'running')
                  ORDER BY created_at ASC, id ASC",
            )
            .map_err(io_error)?;
        let rows = statement.query_map([], row_to_task).map_err(io_error)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(io_error)?.into_record()?);
        }
        Ok(items)
    }

    fn task_prune(&mut self, keep: usize) -> Result<u64, StorageError> {
        let total: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))
            .map_err(io_error)?;
        if total <= keep as i64 {
            return Ok(0);
        }
        let removed = total - keep as i64;
        self.conn
            .execute(
                "DELETE FROM tasks WHERE id IN (
                    SELECT id FROM tasks ORDER BY created_at ASC, id ASC LIMIT ?1
                 )",
                params![removed],
            )
            .map_err(io_error)?;
        Ok(removed as u64)
    }
}

// ---------------------------------------------------------------------------
// 策略（schema 13）
// ---------------------------------------------------------------------------

/// 一条策略对象翻译成 JSON 时的失败。坏数据必须是显式错误，不能悄悄丢一条
/// 规则——丢的那条可能正是唯一的 allow，也可能是唯一的 deny。
fn policy_json(error: serde_json::Error, what: &str) -> StorageError {
    backend(acb_i18n::t!(
        "kernel.storage.policy_payload_invalid",
        what = what,
        error = error
    ))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl Policies for SqliteStore {
    fn policy_load(&self) -> Result<Option<GrantsConfig>, StorageError> {
        let mut grants = GrantsConfig::default();
        let mut any = false;

        let mut stmt = self
            .conn
            .prepare("SELECT id, effect, payload FROM policy_rules ORDER BY id")
            .map_err(io_error)?;
        let mut rows = stmt.query([]).map_err(io_error)?;
        while let Some(row) = rows.next().map_err(io_error)? {
            let id: String = row.get(0).map_err(io_error)?;
            let effect: String = row.get(1).map_err(io_error)?;
            let payload: String = row.get(2).map_err(io_error)?;
            let rule: GrantRule = serde_json::from_str(&payload)
                .map_err(|error| policy_json(error, &format!("policy_rules/{id}")))?;
            match PolicyEffect::parse(&effect) {
                Some(PolicyEffect::Allow) => grants.rules.push(rule),
                Some(PolicyEffect::Deny) => grants.deny.push(rule),
                None => {
                    return Err(backend(format!(
                        "policy_rules/{id} 的 effect 不认识：{effect:?}（只认 allow / deny）"
                    )));
                }
            }
            any = true;
        }
        drop(rows);
        drop(stmt);

        for (sql, label) in [
            (
                "SELECT room_key, payload FROM policy_rooms ORDER BY room_key",
                "policy_rooms",
            ),
            (
                "SELECT account_key, payload FROM policy_accounts ORDER BY account_key",
                "policy_accounts",
            ),
        ] {
            let mut stmt = self.conn.prepare(sql).map_err(io_error)?;
            let mut rows = stmt.query([]).map_err(io_error)?;
            while let Some(row) = rows.next().map_err(io_error)? {
                let key: String = row.get(0).map_err(io_error)?;
                let payload: String = row.get(1).map_err(io_error)?;
                if label == "policy_rooms" {
                    let grant: RoomGrant = serde_json::from_str(&payload)
                        .map_err(|error| policy_json(error, &format!("{label}/{key}")))?;
                    grants.rooms.insert(key, grant);
                } else {
                    let grant: AccountGrant = serde_json::from_str(&payload)
                        .map_err(|error| policy_json(error, &format!("{label}/{key}")))?;
                    grants.accounts.insert(key, grant);
                }
                any = true;
            }
        }

        let mut stmt = self
            .conn
            .prepare("SELECT role, payload FROM policy_admins ORDER BY role")
            .map_err(io_error)?;
        let mut rows = stmt.query([]).map_err(io_error)?;
        while let Some(row) = rows.next().map_err(io_error)? {
            let role: String = row.get(0).map_err(io_error)?;
            let payload: String = row.get(1).map_err(io_error)?;
            let what = format!("policy_admins/{role}");
            match role.as_str() {
                "bot_admin" => {
                    grants.bot_admin = Some(
                        serde_json::from_str(&payload)
                            .map_err(|error| policy_json(error, &what))?,
                    );
                }
                "system_admin" => {
                    grants.system_admin = Some(
                        serde_json::from_str(&payload)
                            .map_err(|error| policy_json(error, &what))?,
                    );
                }
                other => {
                    return Err(backend(format!(
                        "policy_admins 里有不认识的 role：{other:?}"
                    )));
                }
            }
            any = true;
        }

        Ok((any || self.policy_initialized()?).then_some(grants))
    }

    fn policy_replace(&mut self, grants: &GrantsConfig) -> Result<(), StorageError> {
        let at = now_ms();
        let transaction = self.conn.transaction().map_err(io_error)?;
        for table in [
            "policy_rules",
            "policy_rooms",
            "policy_accounts",
            "policy_admins",
        ] {
            transaction
                .execute(&format!("DELETE FROM {table}"), [])
                .map_err(io_error)?;
        }
        for (effect, rules) in [
            (PolicyEffect::Allow, &grants.rules),
            (PolicyEffect::Deny, &grants.deny),
        ] {
            for rule in rules {
                let payload = serde_json::to_string(rule)
                    .map_err(|error| policy_json(error, &format!("rule/{}", rule.id)))?;
                transaction
                    .execute(
                        "INSERT INTO policy_rules (id, effect, payload, updated_at)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![rule.id, effect.as_str(), payload, at],
                    )
                    .map_err(io_error)?;
            }
        }
        for (room, grant) in &grants.rooms {
            let payload = serde_json::to_string(grant)
                .map_err(|error| policy_json(error, &format!("room/{room}")))?;
            transaction
                .execute(
                    "INSERT INTO policy_rooms (room_key, payload, updated_at) VALUES (?1, ?2, ?3)",
                    params![room, payload, at],
                )
                .map_err(io_error)?;
        }
        for (account, grant) in &grants.accounts {
            let payload = serde_json::to_string(grant)
                .map_err(|error| policy_json(error, &format!("account/{account}")))?;
            transaction
                .execute(
                    "INSERT INTO policy_accounts (account_key, payload, updated_at)
                     VALUES (?1, ?2, ?3)",
                    params![account, payload, at],
                )
                .map_err(io_error)?;
        }
        if let Some(bot) = &grants.bot_admin {
            let payload =
                serde_json::to_string(bot).map_err(|error| policy_json(error, "bot_admin"))?;
            transaction
                .execute(
                    "INSERT INTO policy_admins (role, payload, updated_at) VALUES ('bot_admin', ?1, ?2)",
                    params![payload, at],
                )
                .map_err(io_error)?;
        }
        if let Some(system) = &grants.system_admin {
            let payload = serde_json::to_string(system)
                .map_err(|error| policy_json(error, "system_admin"))?;
            transaction
                .execute(
                    "INSERT INTO policy_admins (role, payload, updated_at) VALUES ('system_admin', ?1, ?2)",
                    params![payload, at],
                )
                .map_err(io_error)?;
        }
        // 留一个「已经初始化过」的标记：否则「管理员把策略清空了」和「还没导入过」
        // 在库里长得一模一样，重启时会把清空当成首次导入，用旧的 TOML 配置复活。
        transaction
            .execute(
                "INSERT INTO meta (key, value) VALUES ('policy_initialized', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![at.to_string()],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;
        Ok(())
    }

    fn policy_rule_put(
        &mut self,
        effect: PolicyEffect,
        rule: &GrantRule,
        at: i64,
    ) -> Result<(), StorageError> {
        let payload = serde_json::to_string(rule)
            .map_err(|error| policy_json(error, &format!("rule/{}", rule.id)))?;
        self.conn
            .execute(
                "INSERT INTO policy_rules (id, effect, payload, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE SET
                     effect = excluded.effect,
                     payload = excluded.payload,
                     updated_at = excluded.updated_at",
                params![rule.id, effect.as_str(), payload, at],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn policy_rule_delete(&mut self, effect: PolicyEffect, id: &str) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM policy_rules WHERE id = ?1 AND effect = ?2",
                params![id, effect.as_str()],
            )
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn policy_room_put(
        &mut self,
        room: &str,
        grant: &RoomGrant,
        at: i64,
    ) -> Result<(), StorageError> {
        let payload = serde_json::to_string(grant)
            .map_err(|error| policy_json(error, &format!("room/{room}")))?;
        self.conn
            .execute(
                "INSERT INTO policy_rooms (room_key, payload, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (room_key) DO UPDATE SET
                     payload = excluded.payload,
                     updated_at = excluded.updated_at",
                params![room, payload, at],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn policy_room_delete(&mut self, room: &str) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM policy_rooms WHERE room_key = ?1",
                params![room],
            )
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn policy_account_put(
        &mut self,
        account: &str,
        grant: &AccountGrant,
        at: i64,
    ) -> Result<(), StorageError> {
        let payload = serde_json::to_string(grant)
            .map_err(|error| policy_json(error, &format!("account/{account}")))?;
        self.conn
            .execute(
                "INSERT INTO policy_accounts (account_key, payload, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (account_key) DO UPDATE SET
                     payload = excluded.payload,
                     updated_at = excluded.updated_at",
                params![account, payload, at],
            )
            .map_err(io_error)?;
        Ok(())
    }

    fn policy_account_delete(&mut self, account: &str) -> Result<bool, StorageError> {
        let removed = self
            .conn
            .execute(
                "DELETE FROM policy_accounts WHERE account_key = ?1",
                params![account],
            )
            .map_err(io_error)?;
        Ok(removed > 0)
    }

    fn policy_bot_admin_put(
        &mut self,
        value: Option<&BotAdminGrant>,
        at: i64,
    ) -> Result<(), StorageError> {
        self.put_admin_singleton(
            "bot_admin",
            value.map(serde_json::to_string).transpose(),
            at,
        )
    }

    fn policy_system_admin_put(
        &mut self,
        value: Option<&SystemAdminGrant>,
        at: i64,
    ) -> Result<(), StorageError> {
        self.put_admin_singleton(
            "system_admin",
            value.map(serde_json::to_string).transpose(),
            at,
        )
    }
}

impl SqliteStore {
    /// 策略是否已经初始化过（首次导入完成）。
    fn policy_initialized(&self) -> Result<bool, StorageError> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'policy_initialized'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        Ok(found.is_some())
    }

    /// 单例管理员：给了就覆盖，`None` 就删掉那一行。
    fn put_admin_singleton(
        &mut self,
        role: &str,
        payload: Result<Option<String>, serde_json::Error>,
        at: i64,
    ) -> Result<(), StorageError> {
        let Some(payload) = payload.map_err(|error| policy_json(error, role))? else {
            self.conn
                .execute("DELETE FROM policy_admins WHERE role = ?1", params![role])
                .map_err(io_error)?;
            return Ok(());
        };
        self.conn
            .execute(
                "INSERT INTO policy_admins (role, payload, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (role) DO UPDATE SET
                     payload = excluded.payload,
                     updated_at = excluded.updated_at",
                params![role, payload, at],
            )
            .map_err(io_error)?;
        Ok(())
    }
}

impl ModelFallbacks for SqliteStore {
    fn model_fallbacks(&self) -> Result<Vec<String>, StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT model_ref FROM model_fallbacks ORDER BY position, model_ref")
            .map_err(io_error)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(io_error)?;
        let mut out = Vec::new();
        for item in rows {
            out.push(item.map_err(io_error)?);
        }
        Ok(out)
    }

    fn replace_model_fallbacks(&mut self, list: &[String]) -> Result<(), StorageError> {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let transaction = self.conn.transaction().map_err(io_error)?;
        transaction
            .execute("DELETE FROM model_fallbacks", [])
            .map_err(io_error)?;
        for (index, model_ref) in list.iter().enumerate() {
            transaction
                .execute(
                    "INSERT INTO model_fallbacks (model_ref, position, updated_at)
                     VALUES (?1, ?2, ?3)",
                    params![model_ref, index as i64, at],
                )
                .map_err(io_error)?;
        }
        transaction.commit().map_err(io_error)?;
        Ok(())
    }
}

impl ConfigObjectsStore for SqliteStore {
    fn config_objects_load(&self) -> Result<Option<ConfigObjects>, StorageError> {
        let found: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'config_objects_initialized'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(io_error)?;
        if found.is_none() {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare(
                "SELECT kind, id, payload FROM config_objects
                 ORDER BY kind, position, id",
            )
            .map_err(io_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(io_error)?;
        let mut out = ConfigObjects::default();
        for row in rows {
            let (kind, id, payload) = row.map_err(io_error)?;
            let Some(kind) = ConfigObjectKind::parse(&kind) else {
                // 不认识的 kind 说明有人手改过库。不能跳过——那等于悄悄少一个会话。
                return Err(backend(acb_i18n::t!(
                    "kernel.storage.config_object_kind_unknown",
                    kind = kind
                )));
            };
            out.get_mut(kind).push(ConfigObject { id, toml: payload });
        }
        Ok(Some(out))
    }

    fn config_objects_replace(&mut self, set: &ConfigObjects, at: i64) -> Result<(), StorageError> {
        let transaction = self.conn.transaction().map_err(io_error)?;
        transaction
            .execute("DELETE FROM config_objects", [])
            .map_err(io_error)?;
        for kind in ConfigObjectKind::ALL {
            for (index, object) in set.get(kind).iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO config_objects (kind, id, payload, position, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![kind.as_str(), object.id, object.toml, index as i64, at],
                    )
                    .map_err(io_error)?;
            }
        }
        transaction
            .execute(
                "INSERT INTO meta (key, value) VALUES ('config_objects_initialized', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![at.to_string()],
            )
            .map_err(io_error)?;
        transaction.commit().map_err(io_error)?;
        Ok(())
    }

    fn config_object_put(
        &mut self,
        kind: ConfigObjectKind,
        id: &str,
        toml: &str,
        position: i64,
        at: i64,
    ) -> Result<(), StorageError> {
        self.conn
            .execute(
                "INSERT INTO config_objects (kind, id, payload, position, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (kind, id) DO UPDATE SET
                     payload = excluded.payload,
                     position = excluded.position,
                     updated_at = excluded.updated_at",
                params![kind.as_str(), id, toml, position, at],
            )
            .map_err(io_error)?;
        self.mark_config_objects_initialized(at)
    }

    fn config_object_delete(
        &mut self,
        kind: ConfigObjectKind,
        id: &str,
    ) -> Result<bool, StorageError> {
        let changed = self
            .conn
            .execute(
                "DELETE FROM config_objects WHERE kind = ?1 AND id = ?2",
                params![kind.as_str(), id],
            )
            .map_err(io_error)?;
        let at = now_ms();
        self.mark_config_objects_initialized(at)?;
        Ok(changed > 0)
    }
}

impl SqliteStore {
    fn mark_config_objects_initialized(&mut self, at: i64) -> Result<(), StorageError> {
        self.conn
            .execute(
                "INSERT INTO meta (key, value) VALUES ('config_objects_initialized', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                params![at.to_string()],
            )
            .map_err(io_error)?;
        Ok(())
    }
}
