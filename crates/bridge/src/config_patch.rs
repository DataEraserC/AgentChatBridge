//! 结构化配置补丁：管理页表单提交的修改，外科手术式写回 TOML（ADR-0009）。
//!
//! 为什么不是「JSON 反序列化再整个重新生成 TOML」：那会把用户手写的注释和排版
//! 全毁掉。用 toml_edit 在原文档上打补丁，只动表单碰过的键——`toml` crate 本身
//! 就是 toml_edit 的封装，解析行为完全一致。
//!
//! 补丁原子性：任何一条 op 失败就整体报错，一个字节都不落盘（落盘前的校验、
//! 备份由 console 的保存流程统一管，这里只负责「改文本」）。

use acb_i18n::{Message, msg};
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value, value};

/// 补丁错误：主文案是 key + 参数，渲染由调用方按请求 locale 完成；
/// `detail` 只放无法 key 化的底层原文（TOML 解析器、provider 校验等）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchError {
    pub message: Message,
    pub detail: Option<String>,
}

impl PatchError {
    pub fn new(message: Message) -> Self {
        Self {
            message,
            detail: None,
        }
    }

    pub fn with_detail(message: Message, detail: impl ToString) -> Self {
        Self {
            message,
            detail: Some(detail.to_string()),
        }
    }

    /// 把内层错误包成「第 N 条补丁（kind）：<内层>」，保留内层 detail。
    fn wrap(self, context: Message) -> Self {
        Self {
            message: msg!(
                "bridge.patch.op_failed",
                context = context,
                error = self.message
            ),
            detail: self.detail,
        }
    }
}

impl From<Message> for PatchError {
    fn from(message: Message) -> Self {
        Self::new(message)
    }
}

/// 把「来源名」写成 TOML 内联表 `{ secret = "<name>" }`。
fn secret_item(name: &str) -> Item {
    let mut table = InlineTable::new();
    table.insert("secret", Value::from(name.to_owned()));
    Item::Value(Value::InlineTable(table))
}

/// 应用一组补丁操作，返回（改完的 TOML 全文，人话改动清单）。
pub fn apply_patch(
    text: &str,
    ops: &[serde_json::Value],
) -> Result<(String, Vec<Message>), PatchError> {
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        PatchError::with_detail(msg!("bridge.patch.existing_config_unparsable"), error)
    })?;
    let mut changes = Vec::new();

    for (index, op) in ops.iter().enumerate() {
        let kind = op
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let where_ = msg!("bridge.patch.op_context", index = index + 1, kind = kind);
        match kind {
            "session" => session_op(&mut doc, op, &mut changes),
            "subscription" => subscription_op(&mut doc, op, &mut changes),
            "persona" => persona_op(&mut doc, op, &mut changes),
            "memory" => memory_op(&mut doc, op, &mut changes),
            "harness" => harness_op(&mut doc, op, &mut changes),
            "connection" => connection_op(&mut doc, op, &mut changes),
            "global" => global_op(&mut doc, op, &mut changes),
            "delete" => delete_op(&mut doc, op, &mut changes),
            other => Err(msg!("bridge.patch.unknown_op", op = other).into()),
        }
        .map_err(|error: PatchError| error.wrap(where_))?;
    }

    Ok((doc.to_string(), changes))
}

// ---------------------------------------------------------------------------
// 各类操作
// ---------------------------------------------------------------------------

/// 会话修改或**新建**：
/// - 修改：`{"op":"session","id":"main","patch":{"persona":"p2","model":null}}`。
///   只改出现的键；`model` 给 null 表示清掉（= 由 harness 侧决定）。
/// - 新建：`{"op":"session","id":"new1","create":{"harness":"dsh","secret":"dsh-dev-token",...},"patch":{...}}`。
///   `create` 必须带 harness 和 secret（密钥来源名，harness 连桥全靠它）；
///   persona / name / model / memories 可选。patch 随后照常应用。
fn session_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    let id = string_field(op, "id")?;
    if session_table_mut(doc, &id).is_none() {
        let create = op
            .get("create")
            .and_then(serde_json::Value::as_object)
            .ok_or(msg!("bridge.patch.session_missing_create", id = &id))?;
        let harness = create
            .get("harness")
            .and_then(serde_json::Value::as_str)
            .ok_or(msg!("bridge.patch.session_harness_required"))?;
        if harness_table(doc, harness).is_none() {
            return Err(msg!("bridge.patch.session_unknown_harness", harness = harness).into());
        }
        let secret = create
            .get("secret")
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.is_empty())
            .ok_or(msg!("bridge.patch.session_token_required"))?;
        let persona = create
            .get("persona")
            .and_then(serde_json::Value::as_str)
            .filter(|p| !p.is_empty())
            .ok_or(msg!("bridge.patch.session_persona_required"))?;
        let persona_id = persona.to_owned();

        let sessions = doc
            .get_mut("sessions")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or(msg!("bridge.patch.session_section_missing"))?;
        let mut table = Table::new();
        table.insert("id", value(id.clone()));
        table.insert("harness", value(harness.to_owned()));
        table.insert("token", secret_item(secret));
        table.insert("persona", value(persona_id));
        for key in ["name", "model"] {
            if let Some(text) = create.get(key).and_then(serde_json::Value::as_str) {
                if !text.is_empty() {
                    table.insert(key, value(text.to_owned()));
                }
            }
        }
        if let Some(list) = create.get("memories") {
            let memories =
                str_list(list).map_err(|_| msg!("bridge.patch.memories_must_be_list"))?;
            if !memories.is_empty() {
                table.insert("memories", str_array(&memories));
            }
        }
        sessions.push(table);
        changes.push(msg!(
            "bridge.patch.session_created",
            id = &id,
            harness = harness
        ));
    }

    let patch = match op.get("patch").map(serde_json::Value::as_object) {
        Some(Some(patch)) => patch,
        // create-only 的 op（只建不改）没有 patch，直接完事。
        Some(None) => return Err(msg!("bridge.patch.patch_must_be_object").into()),
        None => return Ok(()),
    };
    let table =
        session_table_mut(doc, &id).ok_or(msg!("bridge.patch.session_not_found", id = &id))?;

    for (key, val) in patch {
        match key.as_str() {
            "name" | "persona" => {
                let text = val
                    .as_str()
                    .ok_or(msg!("bridge.patch.field_must_be_string", key = key))?;
                table.insert(key.as_str(), value(text.to_owned()));
                changes.push(msg!(
                    "bridge.patch.session_field_set",
                    id = &id,
                    key = key,
                    value = text
                ));
            }
            "model" => match val {
                serde_json::Value::Null => {
                    table.remove("model");
                    changes.push(msg!("bridge.patch.session_model_cleared", id = &id));
                }
                serde_json::Value::String(text) => {
                    table.insert("model", value(text.clone()));
                    changes.push(msg!(
                        "bridge.patch.session_model_set",
                        id = &id,
                        value = text
                    ));
                }
                _ => return Err(msg!("bridge.patch.model_wrong_type").into()),
            },
            "memories" => {
                let list = str_list(val).map_err(|_| msg!("bridge.patch.memories_must_be_list"))?;
                table.insert("memories", str_array(&list));
                changes.push(msg!(
                    "bridge.patch.session_memories_set",
                    id = &id,
                    list = list.join(", ")
                ));
            }
            other => {
                return Err(msg!("bridge.patch.session_unknown_field", field = other).into());
            }
        }
    }
    Ok(())
}

/// 订阅修改。两种形态：
/// - 删除：`{"op":"subscription","session":"main","room":"qq:group/123","remove":true}`
/// - 增改：`{"op":"subscription","session":"main","connection":"qq","room":"group/456","wake":{...}}`
///   已存在就只动 wake 里出现的键（null = 删键回到默认），不存在就整条新建。
///
/// `room` 写 `qq:group/123`（带连接前缀）或 `group/123`（配 connection 字段）都行。
fn subscription_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    let session_id = string_field(op, "session")?;
    let session = session_table_mut(doc, &session_id)
        .ok_or(msg!("bridge.patch.session_not_found", id = &session_id))?;

    if op
        .get("remove")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        let room = string_field(op, "room")?;
        let (connection, path) = split_room(&room, op)?;
        let subs = session
            .get_mut("subscriptions")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or(msg!(
                "bridge.patch.session_no_subscriptions",
                session = &session_id
            ))?;
        let index = subs
            .iter()
            .position(|t| room_matches(t, &connection, &path))
            .ok_or(msg!(
                "bridge.patch.subscription_not_found",
                session = &session_id,
                connection = &connection,
                path = &path
            ))?;
        subs.remove(index);
        changes.push(msg!(
            "bridge.patch.subscription_removed",
            session = &session_id,
            connection = &connection,
            path = &path
        ));
        return Ok(());
    }

    let (connection, path) = split_room(&string_field(op, "room")?, op)?;
    let subs = session
        .get_mut("subscriptions")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or(msg!(
            "bridge.patch.subscription_section_missing",
            session = &session_id
        ))?;

    if let Some(existing) = subs
        .iter_mut()
        .find(|t| room_matches(t, &connection, &path))
    {
        if let Some(wake) = op.get("wake").and_then(serde_json::Value::as_object) {
            set_wake(
                existing,
                wake,
                &msg!(
                    "bridge.patch.subscription_label",
                    session = &session_id,
                    connection = &connection,
                    path = &path
                ),
                changes,
            )?;
        } else {
            changes.push(msg!(
                "bridge.patch.subscription_unchanged",
                session = &session_id,
                connection = &connection,
                path = &path
            ));
        }
        return Ok(());
    }

    // 新建一条订阅。
    let mut sub = Table::new();
    sub.insert("connection", value(connection.clone()));
    sub.insert("room", value(path.clone()));
    if let Some(wake) = op.get("wake").and_then(serde_json::Value::as_object) {
        let mut wake_table = Table::new();
        fill_wake(&mut wake_table, wake)?;
        sub.insert("wake", Item::Table(wake_table));
    }
    subs.push(sub);
    changes.push(msg!(
        "bridge.patch.subscription_added",
        session = &session_id,
        connection = &connection,
        path = &path
    ));
    Ok(())
}

/// 人格卡新增或更新：`{"op":"persona","id":"p","name":"..","text":".."}`。
fn persona_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    upsert_named_table(doc, "personas", op, &["name", "text"], changes)
}

/// 记忆仓库声明新增或更新：`{"op":"memory","id":"m","name":"..","kind":"git","location":".."}`。
fn memory_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    upsert_named_table(doc, "memories", op, &["name", "kind", "location"], changes)
}

/// harness 声明新增或更新：`{"op":"harness","id":"dsh2","name":"另一个 DSH"}`。
fn harness_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    upsert_named_table(doc, "harnesses", op, &["name"], changes)
}

/// 按 id 找 `[[harnesses]]` 里的那张表（新建会话时要校验引用存在）。
fn harness_table<'a>(doc: &'a mut DocumentMut, id: &str) -> Option<&'a Table> {
    doc.get("harnesses")
        .and_then(Item::as_array_of_tables)?
        .iter()
        .find(|t| t.get("id").and_then(Item::as_str) == Some(id))
}

/// 连接修改或**新建**（改这些**要重启桥**——守护线程攥着旧连接信息）：
/// - 修改：`{"op":"connection","id":"qq","patch":{"url":"..","secret":"..","account":".."}}`
/// - 新建：`{"op":"connection","id":"qq2","create":{"platform":"qq","kind":"onebot11",
///   "url":"ws://..","account":"10002","secret":"qq-token"}}`
fn connection_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    let id = string_field(op, "id")?;
    let exists = doc
        .get("connections")
        .and_then(Item::as_array_of_tables)
        .map(|arr| {
            arr.iter()
                .any(|t| t.get("id").and_then(Item::as_str) == Some(id.as_str()))
        })
        .unwrap_or(false);

    if !exists {
        let create = op
            .get("create")
            .and_then(serde_json::Value::as_object)
            .ok_or(msg!("bridge.patch.connection_missing_create", id = &id))?;
        let platform = create
            .get("platform")
            .and_then(serde_json::Value::as_str)
            .ok_or(msg!("bridge.patch.connection_platform_required"))?;
        let kind = create
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or(msg!("bridge.patch.connection_kind_required"))?;
        let url = create
            .get("url")
            .and_then(serde_json::Value::as_str)
            .filter(|u| !u.is_empty())
            .ok_or(msg!("bridge.patch.connection_url_required"))?;
        let account = create
            .get("account")
            .and_then(serde_json::Value::as_str)
            .ok_or(msg!("bridge.patch.connection_account_required"))?;

        let connections = doc
            .get_mut("connections")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or(msg!("bridge.patch.connection_section_missing"))?;
        let mut table = Table::new();
        table.insert("id", value(id.clone()));
        table.insert("platform", value(platform.to_owned()));
        table.insert("kind", value(kind.to_owned()));
        table.insert("url", value(url.to_owned()));
        table.insert("account", value(account.to_owned()));
        if let Some(secret) = create.get("secret").and_then(serde_json::Value::as_str) {
            if !secret.is_empty() {
                table.insert("token", secret_item(secret));
            }
        }
        connections.push(table);
        changes.push(msg!("bridge.patch.connection_created", id = &id));
    }

    let patch = match op.get("patch") {
        Some(patch) => patch
            .as_object()
            .ok_or(msg!("bridge.patch.patch_must_be_object"))?,
        // create-only 的 op（只建不改）没有 patch，直接完事。
        None => return Ok(()),
    };
    let table = doc
        .get_mut("connections")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|arr| {
            arr.iter_mut()
                .find(|t| t.get("id").and_then(Item::as_str) == Some(id.as_str()))
        })
        .ok_or(msg!("bridge.patch.connection_not_found", id = &id))?;

    for (key, val) in patch {
        match key.as_str() {
            "url" | "account" => {
                let text = val
                    .as_str()
                    .ok_or(msg!("bridge.patch.field_must_be_string", key = key))?;
                table.insert(key.as_str(), value(text.to_owned()));
                changes.push(msg!(
                    "bridge.patch.connection_field_updated",
                    id = &id,
                    key = key
                ));
            }
            "secret" => match val {
                serde_json::Value::Null => {
                    table.remove("token");
                    changes.push(msg!("bridge.patch.connection_token_cleared", id = &id));
                }
                serde_json::Value::String(name) => {
                    table.insert("token", secret_item(name));
                    changes.push(msg!("bridge.patch.connection_token_updated", id = &id));
                }
                _ => return Err(msg!("bridge.patch.token_wrong_type").into()),
            },
            other => {
                return Err(msg!("bridge.patch.connection_unknown_field", field = other).into());
            }
        }
    }
    Ok(())
}

/// 全局标量：`{"op":"global","patch":{"delivery_limit":50,"max_records_per_room":1000,
/// "max_age_days":30,"default_on_turn_error":["log"]}}`。null = 删键回默认。
fn global_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    let patch = op
        .get("patch")
        .and_then(serde_json::Value::as_object)
        .ok_or(msg!("bridge.patch.patch_required"))?;

    for (key, val) in patch {
        match key.as_str() {
            "delivery_limit" => {
                set_or_remove_int(
                    doc,
                    "delivery_limit",
                    val,
                    &msg!("bridge.patch.label_delivery_limit"),
                    changes,
                )?;
            }
            "turn_timeout_secs" => {
                set_or_remove_int(
                    doc,
                    "turn_timeout_secs",
                    val,
                    &msg!("bridge.patch.label_turn_timeout"),
                    changes,
                )?;
            }
            "max_records_per_room" | "max_age_days" => {
                if doc.get("retention").is_none() {
                    doc.insert("retention", Item::Table(Table::new()));
                }
                let table = doc
                    .get_mut("retention")
                    .and_then(Item::as_table_mut)
                    .ok_or(msg!("bridge.patch.retention_not_table"))?;
                match val {
                    serde_json::Value::Null => {
                        table.remove(key.as_str());
                        changes.push(msg!("bridge.patch.key_removed_default", key = key));
                    }
                    serde_json::Value::Number(number) => {
                        let n = number
                            .as_u64()
                            .ok_or(msg!("bridge.patch.key_non_negative_int", key = key))?;
                        table.insert(key.as_str(), value(n as i64));
                        changes.push(msg!("bridge.patch.key_set", key = key, value = n));
                    }
                    _ => {
                        return Err(msg!("bridge.patch.key_number_or_null", key = key).into());
                    }
                }
            }
            "default_on_turn_error" => {
                let list = match val {
                    serde_json::Value::Null => Vec::new(),
                    other => str_list(other)
                        .map_err(|_| msg!("bridge.patch.turn_error_list_required"))?,
                };
                doc.insert("default_on_turn_error", str_array(&list));
                changes.push(msg!("bridge.patch.turn_error_set", list = list.join(", ")));
            }
            other => {
                return Err(msg!("bridge.patch.global_unknown_field", field = other).into());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 通用小件
// ---------------------------------------------------------------------------

/// 按会话 id 找 `[[sessions]]` 里的那张表。
fn session_table_mut<'a>(doc: &'a mut DocumentMut, id: &str) -> Option<&'a mut Table> {
    doc.get_mut("sessions")
        .and_then(Item::as_array_of_tables_mut)?
        .iter_mut()
        .find(|t| t.get("id").and_then(Item::as_str) == Some(id))
}

/// `room` 可以写 `qq:group/123`（从页面显示串直接来）或裸路径 `group/123`
/// （那就要另带 connection 字段）。返回（连接名, 路径）。
fn split_room(room: &str, op: &serde_json::Value) -> Result<(String, String), PatchError> {
    if let Some((connection, path)) = room.split_once(':') {
        return Ok((connection.to_owned(), path.to_owned()));
    }
    let connection = string_field(op, "connection")
        .map_err(|_| msg!("bridge.patch.room_bare_path", room = room))?;
    Ok((connection, room.to_owned()))
}

/// 裸表是不是指向（connection, path）这条订阅。
fn room_matches(table: &Table, connection: &str, path: &str) -> bool {
    table.get("connection").and_then(Item::as_str) == Some(connection)
        && table.get("room").and_then(Item::as_str) == Some(path)
}

/// 在已存在的订阅表上改 wake：只动出现的键，null = 删键回默认。
fn set_wake(
    sub: &mut Table,
    wake: &serde_json::Map<String, serde_json::Value>,
    label: &Message,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    if sub.get_mut("wake").and_then(Item::as_table_mut).is_none() {
        sub.insert("wake", Item::Table(Table::new()));
    }
    let wake_table = sub
        .get_mut("wake")
        .and_then(Item::as_table_mut)
        .ok_or(msg!("bridge.patch.wake_not_table"))?;
    fill_wake(wake_table, wake)?;
    changes.push(msg!("bridge.patch.wake_updated", label = label));
    Ok(())
}

/// 往 wake 表里填字段（新建和修改共用）。
fn fill_wake(
    wake_table: &mut Table,
    wake: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), PatchError> {
    for (key, val) in wake {
        match key.as_str() {
            "provider" => match val {
                serde_json::Value::Null => {
                    wake_table.remove("provider");
                }
                serde_json::Value::String(text) => {
                    let text = text.trim();
                    acb_kernel::ProviderId::parse(text).map_err(|error| {
                        PatchError::with_detail(
                            msg!("bridge.patch.wake_provider_invalid", provider = text),
                            error,
                        )
                    })?;
                    wake_table.insert("provider", value(text));
                }
                _ => return Err(msg!("bridge.patch.wake_provider_type").into()),
            },
            "at_mention" | "reply" | "name_mention" | "poke" => {
                let flag = val
                    .as_bool()
                    .ok_or(msg!("bridge.patch.wake_flag_type", key = key))?;
                wake_table.insert(key.as_str(), value(flag));
            }
            "accumulate" => match val {
                serde_json::Value::Null => {
                    wake_table.remove("accumulate");
                }
                serde_json::Value::Number(number) => {
                    let n = number
                        .as_u64()
                        .ok_or(msg!("bridge.patch.wake_accumulate_type"))?;
                    wake_table.insert("accumulate", value(n as i64));
                }
                _ => return Err(msg!("bridge.patch.wake_accumulate_nullable").into()),
            },
            "names" | "events" | "event_tags" | "ignore_event_tags" | "keywords"
            | "ignore_speakers" => match val {
                serde_json::Value::Null => {
                    wake_table.remove(key.as_str());
                }
                other => {
                    let list = str_list(other)
                        .map_err(|_| msg!("bridge.patch.wake_list_type", key = key))?;
                    wake_table.insert(key.as_str(), str_array(&list));
                }
            },
            other => {
                return Err(msg!("bridge.patch.wake_unknown_field", field = other).into());
            }
        }
    }
    Ok(())
}

/// 人格/记忆这类「按 id 找数组条目，找不到就新建」的通用逻辑。
fn upsert_named_table(
    doc: &mut DocumentMut,
    array_key: &str,
    op: &serde_json::Value,
    fields: &[&str],
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    let id = string_field(op, "id")?;
    let array = doc
        .get_mut(array_key)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or(msg!("bridge.patch.section_missing", section = array_key))?;

    let existing = array
        .iter_mut()
        .find(|t| t.get("id").and_then(Item::as_str) == Some(id.as_str()));
    let label = format!("{array_key}/{id}");
    if let Some(table) = existing {
        for field in fields {
            if let Some(text) = op.get(*field).and_then(serde_json::Value::as_str) {
                table.insert(field, value(text.to_owned()));
                changes.push(msg!(
                    "bridge.patch.entry_field_updated",
                    label = &label,
                    field = *field
                ));
            }
        }
        return Ok(());
    }

    let mut table = Table::new();
    table.insert("id", value(id.clone()));
    for field in fields {
        let text = op
            .get(*field)
            .and_then(serde_json::Value::as_str)
            .ok_or(msg!(
                "bridge.patch.new_entry_missing_field",
                label = &label,
                field = *field
            ))?;
        table.insert(field, value(text.to_owned()));
    }
    array.push(table);
    changes.push(msg!("bridge.patch.entry_created", label = &label));
    Ok(())
}

/// 删除一个具名条目：`{"op":"delete","section":"session|persona|memory|harness|connection","id":"..."}`。
///
/// 删完留下的悬空引用（比如删了人格还有会话指着它）不在这里查——落盘前
/// console 会用完整配置校验兜住，报错和手改 TOML 写错是一样的体验。
fn delete_op(
    doc: &mut DocumentMut,
    op: &serde_json::Value,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    let id = string_field(op, "id")?;
    let section = string_field(op, "section")?;
    let array_key = match section.as_str() {
        "connection" => "connections",
        "persona" => "personas",
        "memory" => "memories",
        "harness" => "harnesses",
        "session" => "sessions",
        other => {
            return Err(msg!("bridge.patch.unknown_section", section = other).into());
        }
    };
    let array = doc
        .get_mut(array_key)
        .and_then(Item::as_array_of_tables_mut)
        .ok_or(msg!(
            "bridge.patch.section_missing_cannot_delete",
            section = array_key
        ))?;
    let position = array
        .iter()
        .position(|t| t.get("id").and_then(Item::as_str) == Some(id.as_str()))
        .ok_or_else(|| {
            msg!(
                "bridge.patch.entry_not_found",
                section = array_key,
                id = &id
            )
        })?;
    array.remove(position);
    changes.push(msg!(
        "bridge.patch.entry_deleted",
        label = format!("{array_key}/{id}")
    ));
    Ok(())
}

fn string_field(op: &serde_json::Value, key: &str) -> Result<String, PatchError> {
    op.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or(msg!("bridge.patch.missing_string_field", key = key).into())
}

fn str_list(val: &serde_json::Value) -> Result<Vec<String>, ()> {
    val.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or(())
}

fn str_array(values: &[String]) -> Item {
    let mut array = toml_edit::Array::new();
    for item in values {
        array.push(item.clone());
    }
    value(array)
}

/// 顶层整数键：数字 → 设值；null → 删键。
fn set_or_remove_int(
    doc: &mut DocumentMut,
    key: &str,
    val: &serde_json::Value,
    label: &Message,
    changes: &mut Vec<Message>,
) -> Result<(), PatchError> {
    match val {
        serde_json::Value::Null => {
            doc.remove(key);
            changes.push(msg!("bridge.patch.label_removed_default", label = label));
            Ok(())
        }
        serde_json::Value::Number(number) => {
            let n = number
                .as_u64()
                .ok_or(msg!("bridge.patch.label_non_negative_int", label = label))?;
            doc.insert(key, value(n as i64));
            changes.push(msg!("bridge.patch.label_set", label = label, value = n));
            Ok(())
        }
        _ => Err(msg!("bridge.patch.label_number_or_null", label = label).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::apply_patch;

    fn changes_en(changes: &[acb_i18n::Message]) -> Vec<String> {
        changes.iter().map(|change| change.render("en")).collect()
    }

    fn error_en(error: &super::PatchError) -> String {
        error.message.render("en")
    }

    fn base() -> String {
        r#"
# 用户手写的注释：机器人主会话
[[personas]]
id = "p"
name = "人格"
text = "你是桥。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "bot"
token = { secret = "conn-secret" }

[[sessions]]
id = "main"
harness = "dsh"
persona = "p"
token = { secret = "t-main" }

[[sessions.subscriptions]]
connection = "qq"
room = "group/123"

[sessions.subscriptions.wake]
at_mention = true
keywords = ["喂"]
"#
        .to_owned()
    }

    fn parse_ops(value: serde_json::Value) -> Vec<serde_json::Value> {
        value.as_array().expect("测试里都是数组").to_vec()
    }

    #[test]
    fn delete_removes_entry_and_keeps_the_rest() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "delete", "section": "persona", "id": "p"},
            {"op": "delete", "section": "session", "id": "main"},
            {"op": "delete", "section": "connection", "id": "qq"}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("补丁能打");
        assert_eq!(changes.len(), 3);
        assert!(!new_text.contains("[[personas]]"));
        assert!(!new_text.contains("[[sessions]]"));
        assert!(!new_text.contains("[[connections]]"));
        // 没删的还在。注释挂在被删表的 decor 上会跟着走（toml_edit 语义），
        // 注释保留由 wake_patch 测试单独覆盖。
        assert!(new_text.contains("[[harnesses]]"));
        // 删成不存在的 id 要报错，而不是静默成功。
        let ops = parse_ops(serde_json::json!([
            {"op": "delete", "section": "persona", "id": "nope"}
        ]));
        assert!(super::apply_patch(&text, &ops).is_err());
    }

    #[test]
    fn wake_patch_keeps_comments_and_only_touched_keys() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "subscription", "session": "main", "room": "qq:group/123",
             "wake": {"keywords": ["喂", "在吗"], "accumulate": 10}}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("补丁能打");
        // 手写注释原样保留。
        assert!(new_text.contains("# 用户手写的注释：机器人主会话"));
        // 没碰过的键原样保留。
        assert!(new_text.contains("at_mention = true"));
        // 改过的键生效，且整份还能被 Config::parse 接受。
        assert!(new_text.contains("accumulate = 10"));
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("补丁后配置合法");
        let sub = &config.session("main").expect("会话在").subscriptions[0];
        assert_eq!(sub.wake.keywords, vec!["喂".to_owned(), "在吗".to_owned()]);
        assert_eq!(sub.wake.accumulate, Some(10));
        assert!(!changes.is_empty());
    }

    #[test]
    fn add_and_remove_subscription() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "subscription", "session": "main", "connection": "qq", "room": "group/456"}
        ]));
        let (new_text, _) = super::apply_patch(&text, &ops).expect("能加");
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.session("main").expect("在").subscriptions.len(), 2);

        let ops = parse_ops(serde_json::json!([
            {"op": "subscription", "session": "main", "room": "qq:group/123", "remove": true}
        ]));
        let (new_text, changes) = super::apply_patch(&new_text, &ops).expect("能删");
        assert!(changes_en(&changes)[0].contains("removed"));
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        let subs = &config.session("main").expect("在").subscriptions;
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].room.to_string(), "qq:group/456");
    }

    #[test]
    fn session_model_null_clears_key() {
        let text = base().replace("persona = \"p\"", "persona = \"p\"\nmodel = \"gpt-x\"");
        let ops = parse_ops(serde_json::json!([
            {"op": "session", "id": "main", "patch": {"model": null, "name": "主力"}}
        ]));
        let (new_text, _) = super::apply_patch(&text, &ops).expect("能打");
        assert!(!new_text.contains("gpt-x"), "model 键被删掉");
        assert!(new_text.contains("name = \"主力\""));
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.session("main").expect("在").model, None);
    }

    #[test]
    fn persona_upsert_creates_when_missing() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "persona", "id": "p2", "name": "二号", "text": "你是二号桥。"}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("能加");
        assert!(changes_en(&changes)[0].contains("created"));
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.personas.len(), 2);
        assert_eq!(config.personas[1].name, "二号");
    }

    #[test]
    fn global_scalars_and_errors() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "global", "patch": {"delivery_limit": 50, "max_age_days": 7, "turn_timeout_secs": 0}}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("能打");
        assert!(new_text.contains("delivery_limit = 50"));
        assert!(new_text.contains("max_age_days = 7"));
        assert!(new_text.contains("turn_timeout_secs = 0"));
        assert_eq!(changes.len(), 3);
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.delivery_limit, 50);
        assert_eq!(config.retention.max_age_days, 7);
        assert_eq!(config.turn_timeout_secs, 0);

        // 不认识的 op 整体报错。
        let ops = parse_ops(serde_json::json!([
            {"op": "global", "patch": {"delivery_limit": 1}},
            {"op": "nonsense"}
        ]));
        let error = super::apply_patch(&text, &ops).expect_err("该报错");
        assert!(
            error_en(&error).contains("op 2"),
            "错误要指明第几条：{}",
            error_en(&error)
        );

        // 不存在的会话报错。
        let ops = parse_ops(serde_json::json!([
            {"op": "session", "id": "ghost", "patch": {"name": "x"}}
        ]));
        let error = super::apply_patch(&text, &ops).expect_err("该报错");
        assert!(error_en(&error).contains("ghost"));
    }

    #[test]
    fn connection_patch_updates_url_and_token() {
        let text = base();
        let ops = parse_ops(serde_json::json!([
            {"op": "connection", "id": "qq", "patch": {"url": "ws://127.0.0.1:3002", "secret": null}}
        ]));
        let (new_text, changes) = super::apply_patch(&text, &ops).expect("能打");
        assert!(new_text.contains("ws://127.0.0.1:3002"));
        assert!(!new_text.contains("conn-secret"), "token 键被删");
        assert!(
            changes_en(&changes)
                .iter()
                .all(|c| c.contains("restart required"))
        );
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.connections[0].token, None);
    }

    #[test]
    fn session_create_requires_harness_and_token() {
        let text = base();
        // 缺 create → 还是报「没有会话」。
        let error = super::apply_patch(
            &text,
            &parse_ops(
                serde_json::json!([{"op": "session", "id": "new1", "patch": {"name": "x"}}]),
            ),
        )
        .expect_err("该报错");
        let rendered = error_en(&error);
        assert!(
            rendered.contains("new1") && rendered.contains("create"),
            "{rendered}"
        );
        // create 里 harness 不存在 → 报错指名。
        let error = super::apply_patch(
            &text,
            &parse_ops(serde_json::json!([{"op": "session", "id": "new1",
                "create": {"harness": "ghost", "secret": "t"}, "patch": {}}])),
        )
        .expect_err("该报错");
        let rendered = error_en(&error);
        assert!(rendered.contains("ghost"), "{rendered}");
        // 正常新建：id/harness/secret/persona 齐全，patch 随后应用。
        let (new_text, changes) = super::apply_patch(
            &text,
            &parse_ops(serde_json::json!([
                {"op": "session", "id": "new1",
                 "create": {"harness": "dsh", "secret": "t-new", "persona": "p", "name": "二号"},
                 "patch": {"model": "some-model"}}
            ])),
        )
        .expect("能建");
        assert!(changes_en(&changes)[0].contains("created"));
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        let session = config.session("new1").expect("新会话在");
        assert_eq!(session.harness, "dsh");
        assert_eq!(session.persona, "p");
        assert_eq!(session.model.as_deref(), Some("some-model"));
        assert_eq!(session.token, "t-new");
        assert!(new_text.contains("# 用户手写的注释"), "注释不丢");
    }

    #[test]
    fn connection_create_and_harness_upsert() {
        let text = base();
        let (new_text, changes) = super::apply_patch(
            &text,
            &parse_ops(serde_json::json!([
                {"op": "connection", "id": "qq2",
                 "create": {"platform": "qq", "kind": "onebot11",
                            "url": "ws://127.0.0.1:3003", "account": "10002", "secret": "t2"}},
                {"op": "harness", "id": "dsh2", "name": "二号 DSH"}
            ])),
        )
        .expect("能建");
        let rendered = changes_en(&changes);
        assert!(rendered[0].contains("created") && rendered[1].contains("created"));
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.connections.len(), 2);
        assert_eq!(config.connections[1].account, "10002");
        assert_eq!(config.connections[1].token.as_deref(), Some("t2"));
        assert_eq!(config.harnesses.len(), 2);
        assert_eq!(config.harnesses[1].name, "二号 DSH");
        // 已存在的 harness 再 upsert 一次只改 name。
        let (new_text, _) = super::apply_patch(
            &new_text,
            &parse_ops(serde_json::json!([{"op": "harness", "id": "dsh2", "name": "改名"}])),
        )
        .expect("能改");
        let config = acb_kernel::Config::parse_fixture(&new_text).expect("合法");
        assert_eq!(config.harnesses[1].name, "改名");
        assert_eq!(config.harnesses.len(), 2, "不重复建");
    }

    #[test]
    fn create_then_delete_keeps_other_sessions_with_subscription_tables() {
        let doc = "[[harnesses]]\nid = \"dsh\"\nkind = \"dsh\"\n\n[[sessions]]\nid = \"a\"\nharness = \"dsh\"\npersona = \"p\"\ntoken = { secret = \"a-token\" }\n\n[sessions.subscriptions]\nroom = \"group/1\"\n\n[[sessions]]\nid = \"b\"\nharness = \"dsh\"\npersona = \"p\"\ntoken = { secret = \"b-token\" }\n";
        let create = serde_json::json!([{"op":"session","id":"c","create":{"harness":"dsh","secret":"c-token","persona":"p"}}]);
        let (patched, _) = apply_patch(doc, create.as_array().expect("array")).expect("create");
        println!("--- after create ---\n{patched}");
        let session_ids = |text: &str| -> Vec<String> {
            let value: toml::Value = text.parse().expect("parse");
            value
                .get("sessions")
                .and_then(toml::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            item.get("id")
                                .and_then(toml::Value::as_str)
                                .map(str::to_owned)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        assert_eq!(
            session_ids(&patched),
            vec!["a", "b", "c"],
            "create 后会话不该乱"
        );
        let delete = serde_json::json!([{"op":"delete","section":"session","id":"c"}]);
        let (final_text, _) =
            apply_patch(&patched, delete.as_array().expect("array")).expect("delete");
        println!("--- after delete ---\n{final_text}");
        assert_eq!(
            session_ids(&final_text),
            vec!["a", "b"],
            "delete 后不该丢掉别的会话"
        );
    }
}
