//! 独立 Issue/WorkItem 服务：SQLite 持久化、状态机、订阅 outbox 与 token 鉴权。
//!
//! 这个 crate 不依赖 ACB，也不认识 ACB 的会话/房间类型；ACB 只通过 HTTP API
//! 和 process-tools provider 接入。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub mod server;

pub const STATUS_NEW: &str = "new";
pub const STATUS_IN_PROGRESS: &str = "in_progress";
pub const STATUS_PENDING_VERIFICATION: &str = "pending_verification";
pub const STATUS_DONE: &str = "done";
pub const STATUS_CLOSED: &str = "closed";

/// 枚举值的单一出处：`/v1/meta` 和 Web UI 都从这里取，避免前端再抄一份。
pub const ISSUE_STATUSES: [&str; 5] = [
    STATUS_NEW,
    STATUS_IN_PROGRESS,
    STATUS_PENDING_VERIFICATION,
    STATUS_DONE,
    STATUS_CLOSED,
];
pub const ISSUE_PRIORITIES: [&str; 4] = ["P0", "P1", "P2", "P3"];
pub const LINK_KINDS: [&str; 4] = ["relates", "blocks", "duplicates", "parent"];

/// 自带的 Web UI（单文件、无构建步骤）。`server.rs` 在 `GET /` 直接吐它。
pub const UI_HTML: &str = include_str!("ui.html");

pub const SCOPE_READ: &str = "issue:read";
pub const SCOPE_WRITE: &str = "issue:write";
pub const SCOPE_REPORT: &str = "issue:report";
pub const SCOPE_SUBSCRIBE: &str = "issue:subscribe";
pub const SCOPE_VERIFY: &str = "issue:verify";
pub const SCOPE_ADMIN: &str = "issue:admin";

pub const ALL_SCOPES: &[&str] = &[
    SCOPE_READ,
    SCOPE_WRITE,
    SCOPE_REPORT,
    SCOPE_SUBSCRIBE,
    SCOPE_VERIFY,
    SCOPE_ADMIN,
];

pub const DEFAULT_VERIFICATION_HOURS: i64 = 72;
pub const AUTO_CLOSE_AFTER_ESCALATION_HOURS: i64 = 96;
pub const CLAIM_STALE_MS: i64 = 5 * 60 * 1000;

static ID_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum IssueError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("stale_version")]
    StaleVersion,
    #[error("invalid transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },
    #[error("verification required: {0}")]
    VerificationRequired(String),
}

pub type IssueResult<T> = Result<T, IssueError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(default)]
    pub reference: String,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            source_type: "agent".to_owned(),
            reference: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub priority: String,
    /// 工作项类型：bug / task / feature / question。GitHub 的 label 语义太重，
    /// Azure 的 work item type 更接近这里要的「一眼知道是什么」。
    #[serde(default = "default_kind")]
    pub kind: String,
    /// 里程碑（GitHub milestone / Azure iteration 的轻量版）：自由文本，不建表。
    #[serde(default)]
    pub milestone: Option<String>,
    pub tags: Vec<String>,
    pub assignee: Option<String>,
    pub reporter: String,
    pub source: Source,
    pub related: Option<Value>,
    pub resolution: Option<String>,
    pub verification_owner: Option<String>,
    pub verification_due_at: Option<i64>,
    pub verification_escalated_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    pub version: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub work_items: Vec<WorkItem>,
}

/// 区分「字段缺失」与「显式 null」：`Option<Option<T>>` 字段上配它，缺字段仍是
/// `None`（不动），`null` 变成 `Some(None)`（清空）。
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn default_kind() -> String {
    "task".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: String,
    pub issue_id: String,
    pub title: String,
    pub body: String,
    pub priority: String,
    pub tags: Vec<String>,
    pub status: String,
    pub assignee: Option<String>,
    pub source: Source,
    pub related: Option<Value>,
    pub acceptance: String,
    pub result: Option<String>,
    pub verification: Option<Value>,
    pub attempts: i64,
    pub verification_due_at: Option<i64>,
    pub verification_escalated_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueEvent {
    pub id: String,
    pub issue_id: String,
    pub work_item_id: Option<String>,
    pub kind: String,
    pub actor: String,
    pub from_status: Option<String>,
    pub to_status: Option<String>,
    pub summary: String,
    pub created_at: i64,
    pub payload: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub subscriber_id: String,
    pub channel: String,
    pub filter: Value,
    pub active: bool,
    pub created_at: i64,
    pub last_delivered_event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Principal {
    pub id: String,
    pub scopes: Vec<String>,
    pub token_id: String,
}

impl Principal {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes
            .iter()
            .any(|item| item == scope || item == SCOPE_ADMIN)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateIssue {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub milestone: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub reporter: Option<String>,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub related: Option<Value>,
    #[serde(default)]
    pub verification_owner: Option<String>,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateIssue {
    pub expected_version: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// 三态：缺字段 = 不动，`null` = 清空，字符串 = 设值。serde 默认会把 `null` 和
    /// 缺字段都解成 `None`，所以清空必须靠 `deserialize_some` 把 `null` 包成 `Some(None)`。
    #[serde(default, deserialize_with = "deserialize_some")]
    pub milestone: Option<Option<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    pub assignee: Option<Option<String>>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub resolution: Option<String>,
    #[serde(default)]
    pub verification_owner: Option<String>,
    #[serde(default)]
    pub comment: Option<String>,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateWorkItem {
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub acceptance: String,
    #[serde(default)]
    pub related: Option<Value>,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReportResult {
    #[serde(default)]
    pub status: Option<String>,
    pub result: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VerifyWorkItem {
    pub passed: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateSubscription {
    pub subscriber_id: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    pub filter: Value,
}

fn default_channel() -> String {
    "console".to_owned()
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MyWorkQuery {
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub subscriber: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub ack_event_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnreadEvents {
    pub count: usize,
    pub event_ids: Vec<String>,
    pub events: Vec<Value>,
    pub redelivery: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MyWork {
    pub work_items: Vec<WorkItem>,
    pub unread_events: UnreadEvents,
}

// ── 基础工具 ──────────────────────────────────────────────────────────────

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_id(prefix: &str, now: i64) -> String {
    let seq = ID_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{now:x}_{seq:x}_{}", random_hex(6))
}

fn random_hex(len: usize) -> String {
    use std::io::Read;

    // `/dev/urandom` 是无限流，不能用 `fs::read`（会一直读直到 OOM）。
    let mut out = String::with_capacity(len.max(64));
    let mut bytes = [0_u8; 32];
    match std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes)) {
        Ok(()) => {
            for byte in bytes {
                out.push_str(&format!("{byte:02x}"));
            }
        }
        Err(_) => {
            let counter = ID_SEQ.load(Ordering::Relaxed);
            out.push_str(&format!("{:x}{:x}", now_ms(), counter));
        }
    }
    while out.len() < len {
        out.push('0');
    }
    out.truncate(len);
    out
}

pub fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn open_db(path: impl AsRef<Path>) -> IssueResult<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    migrate(&conn)?;
    Ok(conn)
}

pub fn migrate(conn: &Connection) -> IssueResult<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS issues (
            id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            status TEXT NOT NULL,
            priority TEXT NOT NULL,
            assignee TEXT,
            reporter TEXT NOT NULL,
            source_type TEXT NOT NULL,
            source_ref TEXT NOT NULL,
            related TEXT,
            resolution TEXT,
            verification_owner TEXT,
            verification_due_at INTEGER,
            verification_escalated_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            closed_at INTEGER,
            version INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS issue_status_updated ON issues(status, updated_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS issue_assignee_updated ON issues(assignee, updated_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS issue_updated ON issues(updated_at DESC, id DESC);
        CREATE TABLE IF NOT EXISTS issue_tags (
            issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
            tag TEXT NOT NULL,
            PRIMARY KEY(issue_id, tag)
        );
        CREATE INDEX IF NOT EXISTS issue_tags_tag ON issue_tags(tag, issue_id);

        CREATE TABLE IF NOT EXISTS work_items (
            id TEXT PRIMARY KEY,
            issue_id TEXT NOT NULL REFERENCES issues(id),
            title TEXT NOT NULL,
            body TEXT NOT NULL,
            priority TEXT NOT NULL,
            tags TEXT NOT NULL,
            status TEXT NOT NULL,
            assignee TEXT,
            source_type TEXT NOT NULL,
            source_ref TEXT NOT NULL,
            related TEXT,
            acceptance TEXT NOT NULL,
            result TEXT,
            verification TEXT,
            attempts INTEGER NOT NULL,
            verification_due_at INTEGER,
            verification_escalated_at INTEGER,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS work_issue ON work_items(issue_id, updated_at DESC, id DESC);
        CREATE INDEX IF NOT EXISTS work_assignee ON work_items(assignee, status, updated_at DESC, id DESC);

        CREATE TABLE IF NOT EXISTS issue_events (
            id TEXT PRIMARY KEY,
            issue_id TEXT NOT NULL REFERENCES issues(id),
            work_item_id TEXT,
            kind TEXT NOT NULL,
            actor TEXT NOT NULL,
            from_status TEXT,
            to_status TEXT,
            summary TEXT NOT NULL,
            idempotency_key TEXT UNIQUE,
            created_at INTEGER NOT NULL,
            payload TEXT
        );
        CREATE INDEX IF NOT EXISTS event_issue ON issue_events(issue_id, created_at DESC, id DESC);

        CREATE TABLE IF NOT EXISTS event_deliveries (
            event_id TEXT NOT NULL REFERENCES issue_events(id),
            subscriber_id TEXT NOT NULL,
            channel TEXT NOT NULL,
            state TEXT NOT NULL,
            claim_id TEXT,
            attempts INTEGER NOT NULL DEFAULT 0,
            claimed_at INTEGER,
            delivered_at INTEGER,
            ack_at INTEGER,
            PRIMARY KEY(event_id, subscriber_id, channel)
        );
        CREATE INDEX IF NOT EXISTS delivery_pending ON event_deliveries(subscriber_id, channel, state);

        CREATE TABLE IF NOT EXISTS subscriptions (
            id TEXT PRIMARY KEY,
            subscriber_id TEXT NOT NULL,
            channel TEXT NOT NULL,
            filter TEXT NOT NULL,
            active INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            last_delivered_event_id TEXT
        );

        CREATE TABLE IF NOT EXISTS tokens (
            id TEXT PRIMARY KEY,
            principal_id TEXT NOT NULL,
            name TEXT NOT NULL,
            scopes TEXT NOT NULL,
            token_hash TEXT NOT NULL UNIQUE,
            created_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL,
            revoked_at INTEGER,
            last_used_at INTEGER,
            rotated_from TEXT
        );
        "#,
    )?;
    // 2 → kind / milestone：GitHub 的 label + milestone、Azure 的 work item type，
    // 都是「先有字段再谈视图」。列可空/带默认值，老库原地升级，不搬数据。
    ensure_column(conn, "issues", "kind", "TEXT NOT NULL DEFAULT 'task'")?;
    ensure_column(conn, "issues", "milestone", "TEXT")?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS issue_links (
            id TEXT PRIMARY KEY,
            from_issue TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
            to_issue TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
            kind TEXT NOT NULL,
            actor TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            UNIQUE(from_issue, to_issue, kind)
        );
        CREATE INDEX IF NOT EXISTS issue_links_from ON issue_links(from_issue, created_at);
        CREATE INDEX IF NOT EXISTS issue_links_to ON issue_links(to_issue, created_at);
        "#,
    )?;
    Ok(())
}

/// 给老库补列：`PRAGMA table_info` 查得到就跳过。只增不改，和 ACB 的迁移规矩一致。
fn ensure_column(conn: &Connection, table: &str, column: &str, ddl: &str) -> IssueResult<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .collect();
    if names.iter().any(|name| name == column) {
        return Ok(());
    }
    conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {ddl}"),
        [],
    )?;
    Ok(())
}

// ── token ─────────────────────────────────────────────────────────────────

pub struct NewToken {
    pub plaintext: String,
    pub token_id: String,
    pub principal_id: String,
    pub scopes: Vec<String>,
    pub expires_at: i64,
}

pub fn create_token(
    conn: &Connection,
    principal_id: &str,
    name: &str,
    scopes: &[String],
    expires_at: i64,
    now: i64,
    rotated_from: Option<&str>,
) -> IssueResult<NewToken> {
    for scope in scopes {
        if !ALL_SCOPES.contains(&scope.as_str()) {
            return Err(IssueError::BadRequest(format!("unknown scope {scope}")));
        }
    }
    if principal_id == "acb-bridge" && scopes.iter().any(|scope| scope == SCOPE_ADMIN) {
        return Err(IssueError::BadRequest(
            "acb-bridge service token must not include issue:admin".to_owned(),
        ));
    }
    let plaintext = format!("acb_issue_{}", random_hex(48));
    let id = new_id("tk", now);
    conn.execute(
        "INSERT INTO tokens (id, principal_id, name, scopes, token_hash, created_at, expires_at, rotated_from) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            id,
            principal_id,
            name,
            serde_json::to_string(scopes).unwrap_or_else(|_| "[]".to_owned()),
            token_hash(&plaintext),
            now,
            expires_at,
            rotated_from
        ],
    )?;
    Ok(NewToken {
        plaintext,
        token_id: id,
        principal_id: principal_id.to_owned(),
        scopes: scopes.to_vec(),
        expires_at,
    })
}

pub fn rotate_token(conn: &Connection, token_id: &str, now: i64) -> IssueResult<NewToken> {
    let (principal_id, name, scopes, expires_at): (String, String, String, i64) = conn
        .query_row(
            "SELECT principal_id, name, scopes, expires_at FROM tokens WHERE id = ? AND revoked_at IS NULL",
            params![token_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| IssueError::NotFound(format!("token {token_id}")))?;
    let scopes: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
    // 旧 token 保留 7 天重叠期，不立即失效。
    let new_expiry_for_old = (now + 7 * 24 * 3600 * 1000).min(expires_at);
    conn.execute(
        "UPDATE tokens SET expires_at = ? WHERE id = ?",
        params![new_expiry_for_old, token_id],
    )?;
    create_token(
        conn,
        &principal_id,
        &name,
        &scopes,
        now + 90 * 24 * 3600 * 1000,
        now,
        Some(token_id),
    )
}

pub fn revoke_token(conn: &Connection, token_id: &str, now: i64) -> IssueResult<()> {
    let changed = conn.execute(
        "UPDATE tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL",
        params![now, token_id],
    )?;
    if changed == 0 {
        return Err(IssueError::NotFound(format!("token {token_id}")));
    }
    Ok(())
}

pub fn authenticate(conn: &Connection, token: &str, now: i64) -> IssueResult<Principal> {
    if token.trim().is_empty() {
        return Err(IssueError::Unauthorized);
    }
    let hash = token_hash(token.trim());
    let row: Option<(String, String, String, i64, Option<i64>, String)> = conn
        .query_row(
            "SELECT id, principal_id, scopes, expires_at, revoked_at, name FROM tokens WHERE token_hash = ?",
            params![hash],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((id, principal_id, scopes, expires_at, revoked_at, _name)) = row else {
        return Err(IssueError::Unauthorized);
    };
    if revoked_at.is_some() || expires_at <= now {
        return Err(IssueError::Unauthorized);
    }
    conn.execute(
        "UPDATE tokens SET last_used_at = ? WHERE id = ?",
        params![now, id],
    )?;
    let scopes: Vec<String> = serde_json::from_str(&scopes).unwrap_or_default();
    Ok(Principal {
        id: principal_id,
        scopes,
        token_id: id,
    })
}

pub fn require_scope(principal: &Principal, scope: &str) -> IssueResult<()> {
    if principal.has(scope) {
        Ok(())
    } else {
        Err(IssueError::Forbidden(format!("requires scope {scope}")))
    }
}

impl Principal {
    /// ACB service token 可以用 `on_behalf_of`；其它 token 一律忽略该字段。
    pub fn actor_for(&self, on_behalf_of: Option<&str>) -> String {
        if self.id == "acb-bridge" {
            on_behalf_of
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(&self.id)
                .to_owned()
        } else {
            self.id.clone()
        }
    }
}

// ── 行映射 ───────────────────────────────────────────────────────────────

fn row_to_issue(row: &rusqlite::Row<'_>, tags: Vec<String>) -> Issue {
    let related: Option<String> = row.get(9).unwrap_or(None);
    Issue {
        id: row.get(0).unwrap_or_default(),
        title: row.get(1).unwrap_or_default(),
        body: row.get(2).unwrap_or_default(),
        status: row.get(3).unwrap_or_default(),
        priority: row.get(4).unwrap_or_default(),
        assignee: row.get(5).unwrap_or(None),
        reporter: row.get(6).unwrap_or_default(),
        source: Source {
            source_type: row.get(7).unwrap_or_default(),
            reference: row.get(8).unwrap_or_default(),
        },
        related: related.and_then(|text| serde_json::from_str(&text).ok()),
        resolution: row.get(10).unwrap_or(None),
        verification_owner: row.get(11).unwrap_or(None),
        verification_due_at: row.get(12).unwrap_or(None),
        verification_escalated_at: row.get(13).unwrap_or(None),
        created_at: row.get(14).unwrap_or(0),
        updated_at: row.get(15).unwrap_or(0),
        closed_at: row.get(16).unwrap_or(None),
        version: row.get(17).unwrap_or(1),
        kind: row
            .get::<_, Option<String>>(18)
            .unwrap_or(None)
            .unwrap_or_else(default_kind),
        milestone: row.get(19).unwrap_or(None),
        tags,
        work_items: Vec::new(),
    }
}

fn tags_of(conn: &Connection, issue_id: &str) -> IssueResult<Vec<String>> {
    let mut stmt = conn.prepare("SELECT tag FROM issue_tags WHERE issue_id = ? ORDER BY tag")?;
    let rows = stmt.query_map(params![issue_id], |row| row.get(0))?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn load_issue(conn: &Connection, id: &str) -> IssueResult<Issue> {
    let row = conn
        .query_row(
            "SELECT id, title, body, status, priority, assignee, reporter, source_type, source_ref, \
                    related, resolution, verification_owner, verification_due_at, \
                    verification_escalated_at, created_at, updated_at, closed_at, version, \
                    kind, milestone \
             FROM issues WHERE id = ?",
            params![id],
            |row| {
                let tags = Vec::new();
                Ok(row_to_issue(row, tags))
            },
        )
        .optional()?
        .ok_or_else(|| IssueError::NotFound(format!("issue {id}")))?;
    let mut issue = row;
    issue.tags = tags_of(conn, id)?;
    issue.work_items = list_work_items(conn, id)?;
    Ok(issue)
}

fn row_to_work(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkItem> {
    let tags: String = row.get(5).unwrap_or_else(|_| "[]".to_owned());
    let related: Option<String> = row.get(10).unwrap_or(None);
    let verification: Option<String> = row.get(13).unwrap_or(None);
    Ok(WorkItem {
        id: row.get(0).unwrap_or_default(),
        issue_id: row.get(1).unwrap_or_default(),
        title: row.get(2).unwrap_or_default(),
        body: row.get(3).unwrap_or_default(),
        priority: row.get(4).unwrap_or_default(),
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        status: row.get(6).unwrap_or_default(),
        assignee: row.get(7).unwrap_or(None),
        source: Source {
            source_type: row.get(8).unwrap_or_default(),
            reference: row.get(9).unwrap_or_default(),
        },
        related: related.and_then(|text| serde_json::from_str(&text).ok()),
        acceptance: row.get(11).unwrap_or_default(),
        result: row.get(12).unwrap_or(None),
        verification: verification.and_then(|text| serde_json::from_str(&text).ok()),
        attempts: row.get(14).unwrap_or(0),
        verification_due_at: row.get(15).unwrap_or(None),
        verification_escalated_at: row.get(16).unwrap_or(None),
        created_at: row.get(17).unwrap_or(0),
        updated_at: row.get(18).unwrap_or(0),
    })
}

fn list_work_items(conn: &Connection, issue_id: &str) -> IssueResult<Vec<WorkItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, issue_id, title, body, priority, tags, status, assignee, source_type, \
                source_ref, related, acceptance, result, verification, attempts, \
                verification_due_at, verification_escalated_at, created_at, updated_at \
         FROM work_items WHERE issue_id = ? ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![issue_id], row_to_work)?;
    Ok(rows.filter_map(Result::ok).collect())
}

fn load_work_item(conn: &Connection, id: &str) -> IssueResult<WorkItem> {
    conn.query_row(
        "SELECT id, issue_id, title, body, priority, tags, status, assignee, source_type, \
                source_ref, related, acceptance, result, verification, attempts, \
                verification_due_at, verification_escalated_at, created_at, updated_at \
         FROM work_items WHERE id = ?",
        params![id],
        row_to_work,
    )
    .optional()?
    .ok_or_else(|| IssueError::NotFound(format!("work item {id}")))
}

// ── 状态机 ───────────────────────────────────────────────────────────────

pub fn allowed_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        (STATUS_NEW, STATUS_IN_PROGRESS)
            | (STATUS_IN_PROGRESS, STATUS_PENDING_VERIFICATION)
            | (STATUS_PENDING_VERIFICATION, STATUS_DONE)
            | (STATUS_PENDING_VERIFICATION, STATUS_IN_PROGRESS)
            | (
                STATUS_NEW | STATUS_IN_PROGRESS | STATUS_PENDING_VERIFICATION,
                STATUS_CLOSED
            )
            | (STATUS_DONE | STATUS_CLOSED, STATUS_IN_PROGRESS)
    )
}

fn default_verifier(issue: &Issue, executor: Option<&str>, admin: &str) -> String {
    if let Some(owner) = issue.verification_owner.as_deref() {
        return owner.to_owned();
    }
    // 创建者优先；创建者就是执行者时不能自验收，退回指派人，再退回管理员。
    if executor != Some(issue.reporter.as_str()) {
        return issue.reporter.clone();
    }
    match issue.assignee.as_deref() {
        Some(assignee) if Some(assignee) != executor && assignee != issue.reporter => {
            assignee.to_owned()
        }
        _ => admin.to_owned(),
    }
}

fn can_verify(
    issue: &Issue,
    executor: Option<&str>,
    actor: &str,
    is_admin: bool,
    admin: &str,
) -> bool {
    is_admin || actor == default_verifier(issue, executor, admin)
}

// ── 事件与 outbox ─────────────────────────────────────────────────────────

// 事件字段全部来自状态迁移上下文，拆成一个 only-used-once 结构体反而更绕。
#[allow(clippy::too_many_arguments)]
fn emit_event(
    conn: &Connection,
    issue_id: &str,
    work_item_id: Option<&str>,
    kind: &str,
    actor: &str,
    from_status: Option<&str>,
    to_status: Option<&str>,
    summary: &str,
    idempotency_key: Option<&str>,
    payload: Option<&Value>,
    now: i64,
) -> IssueResult<Option<String>> {
    if let Some(key) = idempotency_key {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM issue_events WHERE idempotency_key = ?",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return Ok(Some(existing));
        }
    }
    let id = new_id("ev", now);
    conn.execute(
        "INSERT INTO issue_events (id, issue_id, work_item_id, kind, actor, from_status, to_status, \
            summary, idempotency_key, created_at, payload) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        params![
            id,
            issue_id,
            work_item_id,
            kind,
            actor,
            from_status,
            to_status,
            summary,
            idempotency_key,
            now,
            payload.map(|value| serde_json::to_string(value).unwrap_or_default())
        ],
    )?;
    deliver_event(conn, &id, issue_id, now)?;
    Ok(Some(id))
}

fn deliver_event(conn: &Connection, event_id: &str, issue_id: &str, now: i64) -> IssueResult<()> {
    let issue = load_issue(conn, issue_id)?;
    let mut stmt = conn
        .prepare("SELECT id, subscriber_id, channel, filter FROM subscriptions WHERE active = 1")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows.flatten() {
        let (_, subscriber_id, channel, filter) = row;
        let filter: Value = serde_json::from_str(&filter).unwrap_or_else(|_| json!({}));
        if subscription_matches(&filter, &issue) {
            conn.execute(
                "INSERT OR IGNORE INTO event_deliveries \
                 (event_id, subscriber_id, channel, state, attempts) VALUES (?,?,?, 'pending', 0)",
                params![event_id, subscriber_id, channel],
            )?;
        }
    }
    let _ = now;
    Ok(())
}

fn subscription_matches(filter: &Value, issue: &Issue) -> bool {
    if let Some(issue_id) = filter.get("issue_id").and_then(Value::as_str) {
        if issue_id != issue.id {
            return false;
        }
    }
    if let Some(tags) = filter.get("tags").and_then(Value::as_array) {
        for tag in tags.iter().filter_map(Value::as_str) {
            if !issue.tags.iter().any(|item| item == tag) {
                return false;
            }
        }
    }
    if let Some(query) = filter.get("query").and_then(Value::as_str) {
        let needle = query.to_lowercase();
        if !needle.is_empty()
            && !format!("{}\n{}", issue.title, issue.body)
                .to_lowercase()
                .contains(&needle)
        {
            return false;
        }
    }
    true
}

// ── Issue CRUD ────────────────────────────────────────────────────────────

pub fn create_issue(
    conn: &Connection,
    input: CreateIssue,
    actor: &str,
    now: i64,
) -> IssueResult<Issue> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err(IssueError::BadRequest("title is required".to_owned()));
    }
    let id = new_id("is", now);
    let source = input.source.unwrap_or_default();
    let reporter = input.reporter.unwrap_or_else(|| actor.to_owned());
    let priority = input.priority.unwrap_or_else(|| "P2".to_owned());
    let kind = normalized_kind(input.kind.as_deref());
    validate_kind(&kind)?;
    let milestone = clean_text(input.milestone);
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO issues (id, title, body, status, priority, assignee, reporter, source_type, \
            source_ref, related, verification_owner, created_at, updated_at, version, kind, milestone) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,1,?,?)",
        params![
            id,
            title,
            input.body,
            STATUS_NEW,
            priority,
            input.assignee,
            reporter,
            source.source_type,
            source.reference,
            input
                .related
                .map(|value| serde_json::to_string(&value).unwrap_or_default()),
            input.verification_owner,
            now,
            now,
            kind,
            milestone
        ],
    )?;
    for tag in normalized_tags(&input.tags) {
        tx.execute(
            "INSERT OR IGNORE INTO issue_tags (issue_id, tag) VALUES (?, ?)",
            params![id, tag],
        )?;
    }
    emit_event(
        &tx,
        &id,
        None,
        "issue.created",
        actor,
        None,
        Some(STATUS_NEW),
        title,
        None,
        None,
        now,
    )?;
    tx.commit()?;
    load_issue(conn, &id)
}

fn normalized_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect()
}

/// 工作项类型：封闭枚举。写错就报错，不静默落成 task——否则筛选项永远对不上。
const ISSUE_KINDS: [&str; 5] = ["bug", "task", "feature", "question", "chore"];

fn normalized_kind(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => value.to_lowercase(),
        None => default_kind(),
    }
}

fn validate_kind(kind: &str) -> IssueResult<()> {
    if ISSUE_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(bad_request(format!(
            "kind must be one of {} (got {kind:?})",
            ISSUE_KINDS.join(", ")
        )))
    }
}

/// 里程碑是自由文本：首尾空白去掉，空串等于清空。
fn clean_text(value: Option<String>) -> Option<String> {
    value
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

pub fn update_issue(
    conn: &Connection,
    id: &str,
    input: UpdateIssue,
    actor: &str,
    is_admin: bool,
    admin: &str,
    now: i64,
) -> IssueResult<Issue> {
    let current = load_issue(conn, id)?;
    let tx = conn.unchecked_transaction()?;
    let mut issue = current.clone();
    let mut meta_changes: Vec<String> = Vec::new();
    if let Some(title) = input.title {
        if title.trim().is_empty() {
            return Err(IssueError::BadRequest("title is required".to_owned()));
        }
        issue.title = title;
    }
    if let Some(body) = input.body {
        issue.body = body;
    }
    if let Some(priority) = input.priority {
        if priority != issue.priority {
            meta_changes.push(format!("priority {}", priority));
        }
        issue.priority = priority;
    }
    if let Some(kind) = input.kind {
        let kind = normalized_kind(Some(&kind));
        validate_kind(&kind)?;
        if kind != issue.kind {
            meta_changes.push(format!("kind {kind}"));
        }
        issue.kind = kind;
    }
    if let Some(milestone) = input.milestone {
        let milestone = clean_text(milestone);
        if milestone != issue.milestone {
            meta_changes.push(match &milestone {
                Some(value) => format!("milestone {value}"),
                None => "milestone cleared".to_owned(),
            });
        }
        issue.milestone = milestone;
    }
    if let Some(assignee) = input.assignee {
        issue.assignee = assignee;
    }
    if let Some(owner) = input.verification_owner {
        issue.verification_owner = Some(owner);
    }
    if let Some(tags) = input.tags {
        tx.execute("DELETE FROM issue_tags WHERE issue_id = ?", params![id])?;
        for tag in normalized_tags(&tags) {
            tx.execute(
                "INSERT OR IGNORE INTO issue_tags (issue_id, tag) VALUES (?, ?)",
                params![id, tag],
            )?;
        }
        issue.tags = normalized_tags(&tags);
    }
    if let Some(status) = input.status.as_deref() {
        if status != issue.status {
            if !allowed_transition(&issue.status, status) {
                return Err(IssueError::InvalidTransition {
                    from: issue.status.clone(),
                    to: status.to_owned(),
                });
            }
            match status {
                STATUS_PENDING_VERIFICATION => {
                    let has_result = issue.work_items.iter().any(|item| {
                        item.result.is_some() || item.status == STATUS_PENDING_VERIFICATION
                    });
                    if !has_result {
                        return Err(IssueError::BadRequest(
                            "in_progress -> pending_verification requires a reported work item"
                                .to_owned(),
                        ));
                    }
                    issue.verification_due_at =
                        Some(now + DEFAULT_VERIFICATION_HOURS * 3600 * 1000);
                }
                STATUS_DONE => {
                    let executor = issue.assignee.as_deref();
                    if !can_verify(&issue, executor, actor, is_admin, admin) {
                        return Err(forbidden(actor));
                    }
                    let passed = issue.work_items.iter().any(|item| {
                        item.verification
                            .as_ref()
                            .and_then(|value| value.get("state"))
                            .and_then(Value::as_str)
                            == Some("passed")
                    });
                    if !passed && !is_admin {
                        return Err(verification_required(id));
                    }
                    if !passed && input.resolution.as_deref().unwrap_or("").trim().is_empty() {
                        return Err(verification_required(id));
                    }
                    issue.closed_at = Some(now);
                }
                STATUS_CLOSED => {
                    if input.resolution.as_deref().unwrap_or("").trim().is_empty()
                        && issue.resolution.is_none()
                    {
                        return Err(bad_request("closed requires resolution".to_owned()));
                    }
                    issue.closed_at = Some(now);
                }
                STATUS_IN_PROGRESS => {
                    issue.closed_at = None;
                    issue.verification_due_at = None;
                    issue.verification_escalated_at = None;
                }
                _ => {}
            }
            emit_event(
                &tx,
                id,
                None,
                "issue.status_changed",
                actor,
                Some(&issue.status),
                Some(status),
                input.comment.as_deref().unwrap_or(status),
                None,
                None,
                now,
            )?;
            issue.status = status.to_owned();
        }
    }
    if let Some(resolution) = input.resolution {
        issue.resolution = Some(resolution);
    }
    if let Some(comment) = input.comment {
        emit_event(
            &tx,
            id,
            None,
            "issue.comment_added",
            actor,
            None,
            None,
            &comment,
            None,
            None,
            now,
        )?;
    }
    if !meta_changes.is_empty() {
        emit_event(
            &tx,
            id,
            None,
            "issue.updated",
            actor,
            None,
            None,
            &meta_changes.join("; "),
            None,
            None,
            now,
        )?;
    }
    issue.updated_at = now;
    let changed = tx.execute(
        "UPDATE issues SET title=?, body=?, status=?, priority=?, kind=?, milestone=?, assignee=?, resolution=?, \
            verification_owner=?, verification_due_at=?, verification_escalated_at=?, \
            closed_at=?, updated_at=?, version=version+1 WHERE id=? AND version=?",
        params![
            issue.title,
            issue.body,
            issue.status,
            issue.priority,
            issue.kind,
            issue.milestone,
            issue.assignee,
            issue.resolution,
            issue.verification_owner,
            issue.verification_due_at,
            issue.verification_escalated_at,
            issue.closed_at,
            now,
            id,
            input.expected_version
        ],
    )?;
    if changed == 0 {
        return Err(IssueError::StaleVersion);
    }
    tx.commit()?;
    load_issue(conn, id)
}

// 状态机分支里用的小别名，避免在错误信息里重复长格式。
fn forbidden(actor: &str) -> IssueError {
    IssueError::Forbidden(format!("{actor} is not the verifier"))
}
fn verification_required(id: &str) -> IssueError {
    IssueError::VerificationRequired(format!("issue {id} needs a passed work item"))
}
fn bad_request(message: String) -> IssueError {
    IssueError::BadRequest(message)
}

pub fn get_issue(conn: &Connection, id: &str) -> IssueResult<Issue> {
    load_issue(conn, id)
}

pub fn get_work_item(conn: &Connection, id: &str) -> IssueResult<WorkItem> {
    load_work_item(conn, id)
}

pub fn list_work_items_public(conn: &Connection, issue_id: &str) -> IssueResult<Vec<WorkItem>> {
    load_issue(conn, issue_id)?;
    list_work_items(conn, issue_id)
}

pub fn create_work_item(
    conn: &Connection,
    issue_id: &str,
    input: CreateWorkItem,
    actor: &str,
    now: i64,
) -> IssueResult<WorkItem> {
    let issue = load_issue(conn, issue_id)?;
    let title = input.title.trim();
    if title.is_empty() {
        return Err(IssueError::BadRequest("title is required".to_owned()));
    }
    let id = new_id("wi", now);
    let source = Source::default();
    let body = input.body.unwrap_or_else(|| issue.body.clone());
    let priority = input.priority.unwrap_or_else(|| issue.priority.clone());
    let tags = if input.tags.is_empty() {
        issue.tags.clone()
    } else {
        normalized_tags(&input.tags)
    };
    conn.execute(
        "INSERT INTO work_items (id, issue_id, title, body, priority, tags, status, assignee, \
            source_type, source_ref, related, acceptance, attempts, created_at, updated_at) \
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,0,?,?)",
        params![
            id,
            issue_id,
            title,
            body,
            priority,
            serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_owned()),
            STATUS_NEW,
            input.assignee,
            source.source_type,
            source.reference,
            input
                .related
                .map(|value| serde_json::to_string(&value).unwrap_or_default()),
            input.acceptance,
            now,
            now
        ],
    )?;
    emit_event(
        conn,
        issue_id,
        Some(&id),
        "workitem.created",
        actor,
        None,
        None,
        title,
        None,
        None,
        now,
    )?;
    load_work_item(conn, &id)
}

pub fn report_result(
    conn: &Connection,
    work_item_id: &str,
    input: ReportResult,
    actor: &str,
    now: i64,
) -> IssueResult<Value> {
    let item = load_work_item(conn, work_item_id)?;
    let tx = conn.unchecked_transaction()?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT id FROM issue_events WHERE idempotency_key = ?",
            params![input.idempotency_key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(event_id) = existing {
        return Ok(json!({"ok": true, "duplicate": true, "event_id": event_id}));
    }
    let status = input
        .status
        .as_deref()
        .unwrap_or(STATUS_PENDING_VERIFICATION);
    if status != STATUS_PENDING_VERIFICATION && status != STATUS_IN_PROGRESS {
        return Err(bad_request(format!(
            "report status must be pending_verification or in_progress; got {status}"
        )));
    }
    let due = if status == STATUS_PENDING_VERIFICATION {
        Some(now + DEFAULT_VERIFICATION_HOURS * 3600 * 1000)
    } else {
        None
    };
    tx.execute(
        "UPDATE work_items SET status=?, result=?, verification_due_at=?, updated_at=? WHERE id=?",
        params![status, input.result, due, now, work_item_id],
    )?;
    let event_id = emit_event(
        &tx,
        &item.issue_id,
        Some(work_item_id),
        "workitem.result_reported",
        actor,
        Some(&item.status),
        Some(status),
        &input.result,
        Some(&input.idempotency_key),
        None,
        now,
    )?;
    tx.commit()?;
    Ok(json!({"ok": true, "event_id": event_id}))
}

pub fn verify_work_item(
    conn: &Connection,
    work_item_id: &str,
    input: VerifyWorkItem,
    actor: &str,
    is_admin: bool,
    admin: &str,
    now: i64,
) -> IssueResult<WorkItem> {
    let item = load_work_item(conn, work_item_id)?;
    let issue = load_issue(conn, &item.issue_id)?;
    let executor = item.assignee.as_deref().or(issue.assignee.as_deref());
    if !can_verify(&issue, executor, actor, is_admin, admin) {
        return Err(IssueError::Forbidden(format!(
            "{actor} is not the verifier for issue {}",
            issue.id
        )));
    }
    let next = if input.passed {
        STATUS_DONE
    } else {
        STATUS_IN_PROGRESS
    };
    if item.status != STATUS_PENDING_VERIFICATION {
        return Err(bad_request(format!(
            "work item {} is {}, not pending_verification",
            item.id, item.status
        )));
    }
    let verification = json!({
        "state": if input.passed { "passed" } else { "failed" },
        "by": actor,
        "at": now,
        "note": input.note,
    });
    let attempts = if input.passed {
        item.attempts
    } else {
        item.attempts + 1
    };
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE work_items SET status=?, verification=?, attempts=?, updated_at=? WHERE id=?",
        params![
            next,
            serde_json::to_string(&verification).unwrap_or_default(),
            attempts,
            now,
            work_item_id
        ],
    )?;
    emit_event(
        &tx,
        &item.issue_id,
        Some(work_item_id),
        "workitem.verification_changed",
        actor,
        Some(&item.status),
        Some(next),
        if input.passed {
            "verification passed"
        } else {
            "verification failed"
        },
        None,
        Some(&verification),
        now,
    )?;
    tx.commit()?;
    load_work_item(conn, work_item_id)
}

// ── 搜索 ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SearchQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub milestone: Option<String>,
    #[serde(default)]
    pub reporter: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchPage {
    pub issues: Vec<Issue>,
    pub next_cursor: Option<String>,
}

pub fn search_issues(conn: &Connection, query: &SearchQuery) -> IssueResult<SearchPage> {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let mut sql = String::from(
        "SELECT id, title, body, status, priority, assignee, reporter, source_type, source_ref, \
                related, resolution, verification_owner, verification_due_at, \
                verification_escalated_at, created_at, updated_at, closed_at, version, kind, milestone \
         FROM issues WHERE 1=1",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(status) = query.status.as_deref() {
        sql.push_str(" AND status = ?");
        args.push(Box::new(status.to_owned()));
    }
    if let Some(kind) = query
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        sql.push_str(" AND kind = ?");
        args.push(Box::new(kind.to_lowercase()));
    }
    if let Some(priority) = query
        .priority
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        sql.push_str(" AND priority = ?");
        args.push(Box::new(priority.to_uppercase()));
    }
    if let Some(milestone) = query
        .milestone
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if milestone == "none" {
            sql.push_str(" AND milestone IS NULL");
        } else {
            sql.push_str(" AND milestone = ?");
            args.push(Box::new(milestone.to_owned()));
        }
    }
    if let Some(reporter) = query
        .reporter
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        sql.push_str(" AND reporter = ?");
        args.push(Box::new(reporter.to_owned()));
    }
    if let Some(assignee) = query.assignee.as_deref() {
        if assignee == "unassigned" {
            sql.push_str(" AND assignee IS NULL");
        } else {
            sql.push_str(" AND assignee = ?");
            args.push(Box::new(assignee.to_owned()));
        }
    }
    if let Some(since) = query.since {
        sql.push_str(" AND updated_at >= ?");
        args.push(Box::new(since));
    }
    if let Some(until) = query.until {
        sql.push_str(" AND updated_at <= ?");
        args.push(Box::new(until));
    }
    if let Some(q) = query.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        sql.push_str(" AND lower(title || char(10) || body) LIKE ?");
        args.push(Box::new(format!("%{}%", q.to_lowercase())));
    }
    for tag in normalized_tags(&query.tags) {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM issue_tags t WHERE t.issue_id = issues.id AND t.tag = ?)",
        );
        args.push(Box::new(tag));
    }
    if let Some(cursor) = query.cursor.as_deref() {
        if let Some((updated, id)) = cursor.split_once(':') {
            if let Ok(updated) = updated.parse::<i64>() {
                sql.push_str(" AND (updated_at < ? OR (updated_at = ? AND id < ?))");
                args.push(Box::new(updated));
                args.push(Box::new(updated));
                args.push(Box::new(id.to_owned()));
            }
        }
    }
    sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
    args.push(Box::new((limit + 1) as i64));

    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|arg| arg.as_ref()).collect();
    let mut issues = Vec::new();
    let rows = stmt.query_map(params.as_slice(), |row| Ok(row_to_issue(row, Vec::new())))?;
    for row in rows {
        let mut issue = row?;
        issue.tags = tags_of(conn, &issue.id)?;
        issues.push(issue);
    }
    let next_cursor = if issues.len() > limit {
        issues.pop();
        issues
            .last()
            .map(|issue| format!("{}:{}", issue.updated_at, issue.id))
    } else {
        None
    };
    Ok(SearchPage {
        issues,
        next_cursor,
    })
}

// ── 订阅 ─────────────────────────────────────────────────────────────────

pub fn create_subscription(
    conn: &Connection,
    input: CreateSubscription,
    now: i64,
) -> IssueResult<Subscription> {
    if input.subscriber_id.trim().is_empty() {
        return Err(bad_request("subscriber_id is required".to_owned()));
    }
    if input.channel.trim().is_empty() {
        return Err(bad_request("channel is required".to_owned()));
    }
    let id = new_id("sub", now);
    conn.execute(
        "INSERT INTO subscriptions (id, subscriber_id, channel, filter, active, created_at) \
         VALUES (?,?,?,?,1,?)",
        params![
            id,
            input.subscriber_id,
            input.channel,
            serde_json::to_string(&input.filter).unwrap_or_else(|_| "{}".to_owned()),
            now
        ],
    )?;
    Ok(Subscription {
        id,
        subscriber_id: input.subscriber_id,
        channel: input.channel,
        filter: input.filter,
        active: true,
        created_at: now,
        last_delivered_event_id: None,
    })
}

pub fn list_subscriptions(
    conn: &Connection,
    subscriber: Option<&str>,
) -> IssueResult<Vec<Subscription>> {
    let (sql, args): (&str, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(subscriber) = subscriber {
        (
            "SELECT id, subscriber_id, channel, filter, active, created_at, last_delivered_event_id \
             FROM subscriptions WHERE subscriber_id = ? ORDER BY created_at DESC",
            vec![Box::new(subscriber.to_owned())],
        )
    } else {
        (
            "SELECT id, subscriber_id, channel, filter, active, created_at, last_delivered_event_id \
             FROM subscriptions ORDER BY created_at DESC",
            Vec::new(),
        )
    };
    let mut stmt = conn.prepare(sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|arg| arg.as_ref()).collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        let filter: String = row.get(3)?;
        Ok(Subscription {
            id: row.get(0)?,
            subscriber_id: row.get(1)?,
            channel: row.get(2)?,
            filter: serde_json::from_str(&filter).unwrap_or_else(|_| json!({})),
            active: row.get::<_, i64>(4)? != 0,
            created_at: row.get(5)?,
            last_delivered_event_id: row.get(6)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn deactivate_subscription(conn: &Connection, id: &str, now: i64) -> IssueResult<()> {
    let changed = conn.execute(
        "UPDATE subscriptions SET active = 0 WHERE id = ? AND active = 1",
        params![id],
    )?;
    if changed == 0 {
        return Err(IssueError::NotFound(format!("subscription {id}")));
    }
    let _ = now;
    Ok(())
}

// ── outbox / my work ─────────────────────────────────────────────────────

pub fn ack_events(
    conn: &Connection,
    subscriber: &str,
    channel: &str,
    event_ids: &[String],
    now: i64,
) -> IssueResult<Value> {
    let mut acked = 0usize;
    let mut already = 0usize;
    let tx = conn.unchecked_transaction()?;
    for event_id in event_ids {
        let changed = tx.execute(
            "UPDATE event_deliveries SET state='delivered', delivered_at=?, ack_at=? \
             WHERE event_id=? AND subscriber_id=? AND channel=? AND state IN ('pending','claimed')",
            params![now, now, event_id, subscriber, channel],
        )?;
        if changed > 0 {
            acked += 1;
        } else {
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM event_deliveries WHERE event_id=? AND subscriber_id=? AND channel=?",
                    params![event_id, subscriber, channel],
                    |row| row.get(0),
                )
                .optional()?;
            if exists.is_some() {
                already += 1;
            }
        }
    }
    tx.commit()?;
    Ok(json!({"ok": true, "acked": acked, "already": already}))
}

fn claim_deliveries(
    conn: &Connection,
    subscriber: &str,
    channel: &str,
    now: i64,
    limit: usize,
) -> IssueResult<UnreadEvents> {
    let tx = conn.unchecked_transaction()?;
    // 5 分钟未确认的 claim 回到 pending，允许重投。
    tx.execute(
        "UPDATE event_deliveries SET state='pending', claim_id=NULL, claimed_at=NULL \
         WHERE state='claimed' AND claimed_at < ?",
        params![now - CLAIM_STALE_MS],
    )?;
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM event_deliveries WHERE subscriber_id=? AND channel=? AND state IN ('pending','claimed')",
        params![subscriber, channel],
        |row| row.get(0),
    )?;
    let mut stmt = tx.prepare(
        "SELECT d.event_id, e.issue_id, e.kind, e.summary, e.created_at, d.state \
         FROM event_deliveries d JOIN issue_events e ON e.id = d.event_id \
         WHERE d.subscriber_id=? AND d.channel=? AND d.state IN ('pending','claimed') \
         ORDER BY e.created_at, e.id LIMIT ?",
    )?;
    let rows = stmt.query_map(params![subscriber, channel, limit as i64], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    let mut events = Vec::new();
    let mut event_ids = Vec::new();
    for row in rows.flatten() {
        let (event_id, issue_id, kind, summary, created_at, state) = row;
        if state == "pending" {
            let claim = new_id("claim", now);
            let changed = tx.execute(
                "UPDATE event_deliveries SET state='claimed', claim_id=?, claimed_at=?, \
                    attempts=attempts+1 WHERE event_id=? AND subscriber_id=? AND channel=? AND state='pending'",
                params![claim, now, event_id, subscriber, channel],
            )?;
            if changed == 0 {
                continue;
            }
        }
        let title: String = tx
            .query_row(
                "SELECT title FROM issues WHERE id = ?",
                params![issue_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_default();
        event_ids.push(event_id.clone());
        events.push(json!({
            "event_id": event_id,
            "issue_id": issue_id,
            "title": title,
            "kind": kind,
            "summary": summary,
            "created_at": created_at,
        }));
    }
    drop(stmt);
    tx.commit()?;
    Ok(UnreadEvents {
        count: count as usize,
        event_ids,
        events,
        redelivery: count as usize > 0,
    })
}

pub fn my_work(
    conn: &Connection,
    principal: &str,
    query: &MyWorkQuery,
    now: i64,
) -> IssueResult<MyWork> {
    if !query.ack_event_ids.is_empty() {
        let subscriber = query.subscriber.as_deref().unwrap_or(principal);
        let channel = query.channel.as_deref().unwrap_or("console");
        ack_events(conn, subscriber, channel, &query.ack_event_ids, now)?;
    }
    let assignee = query.assignee.as_deref().unwrap_or(principal);
    let mut sql = String::from(
        "SELECT id, issue_id, title, body, priority, tags, status, assignee, source_type, \
                source_ref, related, acceptance, result, verification, attempts, \
                verification_due_at, verification_escalated_at, created_at, updated_at \
         FROM work_items WHERE assignee = ?",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(assignee.to_owned())];
    if let Some(status) = query.status.as_deref() {
        sql.push_str(" AND status = ?");
        args.push(Box::new(status.to_owned()));
    } else {
        sql.push_str(" AND status NOT IN ('done','closed')");
    }
    sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT 50");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = args.iter().map(|arg| arg.as_ref()).collect();
    let rows = stmt.query_map(params.as_slice(), row_to_work)?;
    let work_items: Vec<WorkItem> = rows.filter_map(Result::ok).collect();
    let subscriber = query
        .subscriber
        .as_deref()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("principal:{principal}"));
    let channel = query.channel.as_deref().unwrap_or("console");
    let unread = claim_deliveries(conn, &subscriber, channel, now, 10)?;
    Ok(MyWork {
        work_items,
        unread_events: unread,
    })
}

// ── 维护 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Default)]
pub struct MaintenanceSummary {
    pub requeued: usize,
    pub escalated: usize,
    pub auto_closed: usize,
}

pub fn run_due(conn: &Connection, now: i64, admin: &str) -> IssueResult<MaintenanceSummary> {
    let mut summary = MaintenanceSummary::default();
    let requeued = conn.execute(
        "UPDATE event_deliveries SET state='pending', claim_id=NULL, claimed_at=NULL \
         WHERE state='claimed' AND claimed_at < ?",
        params![now - CLAIM_STALE_MS],
    )?;
    summary.requeued = requeued;

    let escalated: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, title FROM issues WHERE status='pending_verification' \
             AND verification_due_at IS NOT NULL AND verification_due_at <= ? \
             AND verification_escalated_at IS NULL",
        )?;
        let rows = stmt.query_map(params![now], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.filter_map(Result::ok).collect()
    };
    for (id, title) in escalated {
        conn.execute(
            "UPDATE issues SET verification_escalated_at=?, updated_at=?, version=version+1 WHERE id=?",
            params![now, now, id],
        )?;
        emit_event(
            conn,
            &id,
            None,
            "issue.verification_escalated",
            "system",
            Some(STATUS_PENDING_VERIFICATION),
            Some(STATUS_PENDING_VERIFICATION),
            &format!("verification timed out, escalated to {admin}: {title}"),
            None,
            None,
            now,
        )?;
        summary.escalated += 1;
    }

    let auto_close_at = now - AUTO_CLOSE_AFTER_ESCALATION_HOURS * 3600 * 1000;
    let closable: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, title FROM issues WHERE status='pending_verification' \
             AND verification_escalated_at IS NOT NULL AND verification_escalated_at <= ?",
        )?;
        let rows = stmt.query_map(params![auto_close_at], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.filter_map(Result::ok).collect()
    };
    for (id, title) in closable {
        conn.execute(
            "UPDATE issues SET status='closed', resolution='verification_timeout', closed_at=?, \
                updated_at=?, version=version+1 WHERE id=? AND status='pending_verification'",
            params![now, now, id],
        )?;
        emit_event(
            conn,
            &id,
            None,
            "issue.status_changed",
            "system",
            Some(STATUS_PENDING_VERIFICATION),
            Some(STATUS_CLOSED),
            &format!("auto-closed after verification timeout: {title}"),
            None,
            None,
            now,
        )?;
        summary.auto_closed += 1;
    }

    // WorkItem 的验收超时同样处理：升级后自动 closed，避免执行切片永久卡住。
    let work_escalated: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, issue_id, title FROM work_items WHERE status='pending_verification' \
             AND verification_due_at IS NOT NULL AND verification_due_at <= ? \
             AND verification_escalated_at IS NULL",
        )?;
        let rows = stmt.query_map(params![now], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.filter_map(Result::ok).collect()
    };
    for (id, issue_id, title) in work_escalated {
        conn.execute(
            "UPDATE work_items SET verification_escalated_at=?, updated_at=? WHERE id=?",
            params![now, now, id],
        )?;
        emit_event(
            conn,
            &issue_id,
            Some(&id),
            "workitem.verification_escalated",
            "system",
            Some(STATUS_PENDING_VERIFICATION),
            Some(STATUS_PENDING_VERIFICATION),
            &format!("work item verification timed out, escalated to {admin}: {title}"),
            None,
            None,
            now,
        )?;
        summary.escalated += 1;
    }
    let work_closable: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, issue_id, title FROM work_items WHERE status='pending_verification' \
             AND verification_escalated_at IS NOT NULL AND verification_escalated_at <= ?",
        )?;
        let rows = stmt.query_map(params![auto_close_at], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.filter_map(Result::ok).collect()
    };
    for (id, issue_id, title) in work_closable {
        conn.execute(
            "UPDATE work_items SET status='closed', updated_at=? WHERE id=? AND status='pending_verification'",
            params![now, id],
        )?;
        emit_event(
            conn,
            &issue_id,
            Some(&id),
            "workitem.verification_timeout",
            "system",
            Some(STATUS_PENDING_VERIFICATION),
            Some(STATUS_CLOSED),
            &format!("work item auto-closed after verification timeout: {title}"),
            None,
            None,
            now,
        )?;
        summary.auto_closed += 1;
    }
    Ok(summary)
}

/// 备份：SQLite 的 `VACUUM INTO` 生成一致性快照，不直接复制 WAL。
pub fn backup_database(conn: &Connection, path: &str) -> IssueResult<()> {
    if std::path::Path::new(path).exists() {
        return Err(bad_request(format!("backup target already exists: {path}")));
    }
    conn.execute("VACUUM INTO ?", params![path])?;
    Ok(())
}

pub fn events_for_issue(conn: &Connection, issue_id: &str) -> IssueResult<Vec<IssueEvent>> {
    let mut stmt = conn.prepare(
        "SELECT id, issue_id, work_item_id, kind, actor, from_status, to_status, summary, created_at, payload \
         FROM issue_events WHERE issue_id=? ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![issue_id], |row| {
        let payload: Option<String> = row.get(9).unwrap_or(None);
        Ok(IssueEvent {
            id: row.get(0)?,
            issue_id: row.get(1)?,
            work_item_id: row.get(2)?,
            kind: row.get(3)?,
            actor: row.get(4)?,
            from_status: row.get(5)?,
            to_status: row.get(6)?,
            summary: row.get(7)?,
            created_at: row.get(8)?,
            payload: payload.and_then(|text| serde_json::from_str(&text).ok()),
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

// ── 评论与关联 ────────────────────────────────────────────────────────────

/// 评论就是一条 `issue.comment_added` 事件：不另建表，时间线与通知、ack 账本
/// 天然复用同一份数据；将来要编辑/删除评论，再给事件加 `edited_at` 之类的字段。
#[derive(Debug, Clone, Deserialize)]
pub struct CommentInput {
    pub body: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

const MAX_COMMENT_BYTES: usize = 64 * 1024;

pub fn add_comment(
    conn: &Connection,
    issue_id: &str,
    input: CommentInput,
    actor: &str,
    now: i64,
) -> IssueResult<IssueEvent> {
    let body = input.body.trim();
    if body.is_empty() {
        return Err(bad_request("comment body is required".to_owned()));
    }
    if body.len() > MAX_COMMENT_BYTES {
        return Err(bad_request(format!(
            "comment is too long ({} bytes, max {MAX_COMMENT_BYTES})",
            body.len()
        )));
    }
    let issue = load_issue(conn, issue_id)?;
    let tx = conn.unchecked_transaction()?;
    let event_id = emit_event(
        &tx,
        issue_id,
        None,
        "issue.comment_added",
        actor,
        None,
        None,
        body,
        input.idempotency_key.as_deref(),
        None,
        now,
    )?;
    // 评论刷新 issue 的 updated_at/version：列表排序、stale_version 都要看得到。
    tx.execute(
        "UPDATE issues SET updated_at=?, version=version+1 WHERE id=?",
        params![now, issue.id],
    )?;
    tx.commit()?;
    let id = event_id.ok_or_else(|| IssueError::NotFound(format!("comment {issue_id}")))?;
    load_event(conn, &id)
}

fn load_event(conn: &Connection, id: &str) -> IssueResult<IssueEvent> {
    let event = conn
        .query_row(
            "SELECT id, issue_id, work_item_id, kind, actor, from_status, to_status, summary, \
                    created_at, payload FROM issue_events WHERE id = ?",
            params![id],
            |row| {
                let payload: Option<String> = row.get(9).unwrap_or(None);
                Ok(IssueEvent {
                    id: row.get(0)?,
                    issue_id: row.get(1)?,
                    work_item_id: row.get(2)?,
                    kind: row.get(3)?,
                    actor: row.get(4)?,
                    from_status: row.get(5)?,
                    to_status: row.get(6)?,
                    summary: row.get(7)?,
                    created_at: row.get(8)?,
                    payload: payload.and_then(|text| serde_json::from_str(&text).ok()),
                })
            },
        )
        .optional()?
        .ok_or_else(|| IssueError::NotFound(format!("event {id}")))?;
    Ok(event)
}

/// 关联类型：GitHub 的 relates/blocks/duplicates，Azure 的 parent/child。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Relates,
    Blocks,
    Duplicates,
    Parent,
}

impl LinkKind {
    fn parse(value: &str) -> IssueResult<Self> {
        match value.trim().to_lowercase().as_str() {
            "relates" | "related" => Ok(Self::Relates),
            "blocks" | "blocked_by" => Ok(Self::Blocks),
            "duplicates" | "duplicated_by" => Ok(Self::Duplicates),
            "parent" | "child" => Ok(Self::Parent),
            other => Err(bad_request(format!(
                "link kind must be relates/blocks/duplicates/parent (got {other:?})"
            ))),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Relates => "relates",
            Self::Blocks => "blocks",
            Self::Duplicates => "duplicates",
            Self::Parent => "parent",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IssueLink {
    pub id: String,
    pub from_issue: String,
    pub to_issue: String,
    pub kind: String,
    pub actor: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateLink {
    #[serde(default)]
    pub to_issue: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub on_behalf_of: Option<String>,
}

pub fn create_link(
    conn: &Connection,
    issue_id: &str,
    input: CreateLink,
    actor: &str,
    now: i64,
) -> IssueResult<IssueLink> {
    let to_issue = input
        .to_issue
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| bad_request("to_issue is required".to_owned()))?;
    if to_issue == issue_id {
        return Err(bad_request("an issue cannot link to itself".to_owned()));
    }
    let kind = LinkKind::parse(input.kind.as_deref().unwrap_or("relates"))?;
    load_issue(conn, issue_id)?;
    load_issue(conn, to_issue)?;
    let id = new_id("ln", now);
    conn.execute(
        "INSERT OR IGNORE INTO issue_links (id, from_issue, to_issue, kind, actor, created_at) \
         VALUES (?,?,?,?,?,?)",
        params![id, issue_id, to_issue, kind.as_str(), actor, now],
    )?;
    // 幂等：同一对 (from,to,kind) 已存在时返回原记录。
    conn.query_row(
        "SELECT id, from_issue, to_issue, kind, actor, created_at FROM issue_links \
         WHERE from_issue=? AND to_issue=? AND kind=?",
        params![issue_id, to_issue, kind.as_str()],
        |row| {
            Ok(IssueLink {
                id: row.get(0)?,
                from_issue: row.get(1)?,
                to_issue: row.get(2)?,
                kind: row.get(3)?,
                actor: row.get(4)?,
                created_at: row.get(5)?,
            })
        },
    )
    .map_err(IssueError::from)
}

pub fn list_links(conn: &Connection, issue_id: &str) -> IssueResult<Vec<IssueLink>> {
    load_issue(conn, issue_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, from_issue, to_issue, kind, actor, created_at FROM issue_links \
         WHERE from_issue=? OR to_issue=? ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![issue_id, issue_id], |row| {
        Ok(IssueLink {
            id: row.get(0)?,
            from_issue: row.get(1)?,
            to_issue: row.get(2)?,
            kind: row.get(3)?,
            actor: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}

pub fn delete_link(conn: &Connection, id: &str) -> IssueResult<bool> {
    let removed = conn.execute("DELETE FROM issue_links WHERE id=?", params![id])?;
    Ok(removed > 0)
}

/// 评论列表：就是事件链里 `issue.comment_added` 那一段。
/// 单独开一个入口是为了让 UI 能「先画评论、再画事件」，而不必自己过滤。
pub fn comments_for_issue(conn: &Connection, issue_id: &str) -> IssueResult<Vec<IssueEvent>> {
    load_issue(conn, issue_id)?;
    let mut stmt = conn.prepare(
        "SELECT id, issue_id, work_item_id, kind, actor, from_status, to_status, summary, created_at, payload \
         FROM issue_events WHERE issue_id=? AND kind='issue.comment_added' ORDER BY created_at, id",
    )?;
    let rows = stmt.query_map(params![issue_id], |row| {
        let payload: Option<String> = row.get(9).unwrap_or(None);
        Ok(IssueEvent {
            id: row.get(0)?,
            issue_id: row.get(1)?,
            work_item_id: row.get(2)?,
            kind: row.get(3)?,
            actor: row.get(4)?,
            from_status: row.get(5)?,
            to_status: row.get(6)?,
            summary: row.get(7)?,
            created_at: row.get(8)?,
            payload: payload.and_then(|text| serde_json::from_str(&text).ok()),
        })
    })?;
    Ok(rows.filter_map(Result::ok).collect())
}
