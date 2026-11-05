//! 出站：把桥要发的内容变成 OneBot 动作，等平台回 `message_id`。
//!
//! 发动作是「塞进队列 → 等 echo 回包」，真正的写 socket 由 [`crate::link::pump`] 那个线程做。
//! 这样桥那边调 `send` 的时候不需要碰 socket，也就不可能和读线程抢着写。

use std::sync::mpsc;
use std::time::Duration;

use acb_kernel::{
    CallError, CallOutcome, Element, MethodSpec, Outbound, OutboundError, OutgoingKind,
    OutgoingRecord, RoomId, Sent,
};
use serde_json::{Value, json};

use crate::catalog::{ROUTES, Route, Target, catalog};
use crate::forward;
use crate::link::{Action, LinkError, LinkSlot};
use crate::segment::{from_onebot, to_onebot};

/// 普通消息等平台回话的上限。超了就报错，不无限挂着。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// 带媒体/文件的消息和平台调用等回话的上限。
///
/// 50 MB 的视频走 NapCat 上传，10 秒根本不够；以前超时被当成「发送失败」，
/// agent 会重发，群里就出现两条。45 秒留在 harness 的 60 秒工具超时以内。
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(45);

/// 平台侧 `download_file` 默认超时是 60 秒（`SNOWLUMA_DOWNLOAD_FILE_TIMEOUT_MS`）。
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// 批量踢人的一次上限。SnowLuma 没有文档化上限；core 不替模型无限制搬运名单。
const MAX_BATCH_KICK: usize = 20;

/// 自定义表情 id 的长度上限：32 位 MD5 加上完整 id 的前缀余量。
const MAX_EMOJI_ID_LEN: usize = 256;

/// OCR 的图片地址/缓存 id 上限：只收标识，不替模型搬运图片内容。
const MAX_OCR_SOURCE_LEN: usize = 2048;

/// 下载类动作的 ACB 侧默认/最大大小。平台上限是 1 GiB；默认仍保守，调用方可显式抬高。
const DEFAULT_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_DOWNLOAD_URL_LEN: usize = 4096;
const MAX_DOWNLOAD_NAME_LEN: usize = 255;

/// 流式上传：单块 1 MiB、总大小 256 MiB（平台单块上限 1 MiB）。
const MAX_STREAM_CHUNK_BYTES: u64 = 1024 * 1024;
const MAX_STREAM_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_STREAM_ID_LEN: usize = 64;
const MAX_STREAM_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

/// 合并转发载荷：不让 core 变成任意大小转发载荷的搬运通道。
const MAX_FORWARD_BYTES: usize = 256 * 1024;
const MAX_FORWARD_NODES: usize = 100;

/// 群相册长文本/文件标识上限。
const MAX_ALBUM_TEXT_CHARS: usize = 4096;
const MAX_ALBUM_FILE_LEN: usize = 4096;

/// SSO 包体（hex 字符串）上限。
const MAX_PACKET_HEX_CHARS: usize = 64 * 1024;

// 平台方法表在 `catalog.rs`：那里的每一行都是「人也做得到的事」。

/// OneBot 出站通道。一条连接一个。
pub struct OneBotSnowLumaOutbound {
    slot: LinkSlot,
    /// 这条连接的连接名，别的连接发过来直接拒。
    connection: String,
    timeout: Duration,
    upload_timeout: Duration,
    download_timeout: Duration,
    seq: u64,
}

impl OneBotSnowLumaOutbound {
    pub fn new(slot: LinkSlot, connection: impl Into<String>) -> Self {
        Self {
            slot,
            connection: connection.into(),
            timeout: SEND_TIMEOUT,
            upload_timeout: UPLOAD_TIMEOUT,
            download_timeout: DOWNLOAD_TIMEOUT,
            seq: 0,
        }
    }

    /// 换一个等回话的时限（测试用）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.upload_timeout = timeout;
        self.download_timeout = timeout;
        self
    }

    /// 这条出站要等多久：带媒体/文件/平台卡片的消息要上传，给更长的窗口。
    fn send_timeout(&self, elements: &[Element]) -> Duration {
        let may_upload = elements.iter().any(|element| {
            matches!(
                element,
                Element::Image { .. }
                    | Element::Audio { .. }
                    | Element::Video { .. }
                    | Element::File { .. }
                    | Element::Opaque { .. }
            )
        });
        if may_upload {
            self.upload_timeout
        } else {
            self.timeout
        }
    }

    /// 超时不是明确失败：请求可能还在上传/处理，也可能已经执行。
    fn timeout_detail(timeout: Duration) -> String {
        acb_i18n::t!(
            "adapter.onebot11.timeout_detail",
            seconds = timeout.as_secs()
        )
    }
}

impl Outbound for OneBotSnowLumaOutbound {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unavailable(acb_i18n::t!(
                "adapter.onebot11.connection_mismatch",
                connection = self.connection,
                room = room.connection
            )));
        }

        let (action, params) = action_of(room, elements)?;
        let Some(sender) = self.slot.current() else {
            return Err(OutboundError::Unavailable(acb_i18n::t!(
                "adapter.onebot11.not_connected",
                connection = self.connection
            )));
        };

        self.seq += 1;
        let echo = format!("acb-{}", self.seq);
        let (reply_tx, reply_rx) = mpsc::channel();
        sender
            .send(Action {
                body: json!({"action": action, "params": params, "echo": echo}),
                reply: reply_tx,
            })
            .map_err(|_| {
                OutboundError::Unavailable(acb_i18n::t!("adapter.onebot11.connection_lost"))
            })?;

        let timeout = self.send_timeout(elements);
        let response = reply_rx
            .recv_timeout(timeout)
            .map_err(|_| OutboundError::Timeout(Self::timeout_detail(timeout)))?
            .map_err(|error: LinkError| OutboundError::Unavailable(error.to_string()))?;

        parse_response(response)
    }

    fn catalog(&self, _connection: &str) -> std::sync::Arc<[MethodSpec]> {
        catalog()
    }

    fn call(
        &mut self,
        room: &RoomId,
        method: &str,
        params: &Value,
    ) -> Result<CallOutcome, CallError> {
        if room.connection != self.connection {
            return Err(CallError::Forbidden(acb_i18n::t!(
                "adapter.onebot11.call_connection_mismatch",
                connection = self.connection,
                room = room.connection
            )));
        }

        // 合并转发要一层层取，模型不该为这件事点三次（ADR-0024）。
        if method == "msg.forward" {
            return self.forward_tree(params).map(CallOutcome::value);
        }

        let route = ROUTES
            .iter()
            .find(|route| route.method == method)
            .ok_or_else(|| CallError::UnknownMethod {
                method: method.to_owned(),
                suggestion: None,
            })?;

        let action = action_of_route(route, room)?;
        let mut params = params.clone();
        normalize_call_params(route.method, &mut params)?;
        validate_call_params(route.method, &params)?;
        let payload = call_payload(route, room, &params)?;
        let timeout = if matches!(
            method,
            "file.download"
                | "stream.download_image"
                | "stream.download_record"
                | "stream.download_file"
                | "stream.test_download"
        ) {
            self.download_timeout
        } else {
            self.upload_timeout
        };
        let response = self.dispatch(action, payload, timeout)?;
        let value = parse_call_response(method, response)?;
        let record = outgoing_record(method, &params, &value);
        Ok(CallOutcome { value, record })
    }
}

impl OneBotSnowLumaOutbound {
    /// 展开一条合并转发：按 `depth` / `max_nodes` 递归取，摊平成带路径的节点表。
    ///
    /// 递归放在适配器里，是因为「一次只给一层」是平台的规矩，不该让模型替我们跑循环。
    fn forward_tree(&mut self, params: &Value) -> Result<Value, CallError> {
        let id = params
            .get("id")
            .and_then(|value| match value {
                Value::String(text) => Some(text.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                CallError::BadParams(acb_i18n::t!("adapter.onebot11.forward_id_required"))
            })?;

        let depth = params
            .get("depth")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(forward::DEFAULT_DEPTH);
        let max_nodes = params
            .get("max_nodes")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(forward::DEFAULT_MAX_NODES);

        let mut fetch = |id: &str| {
            // 展开是一层层读，单层用普通时限：别让一次深转发的总耗时撞上 harness 超时。
            let raw = self.dispatch("get_forward_msg", json!({ "id": id }), self.timeout)?;
            parse_call_response("msg.forward", raw)
        };
        forward::expand(&id, depth, max_nodes, &mut fetch)
    }

    /// 塞一个动作进队列、等平台按 echo 回话。`call` 和转发展开共用这一条通道。
    fn dispatch(
        &mut self,
        action: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let Some(sender) = self.slot.current() else {
            return Err(CallError::Unavailable(acb_i18n::t!(
                "adapter.onebot11.not_connected",
                connection = self.connection
            )));
        };

        self.seq += 1;
        let echo = format!("acb-call-{}", self.seq);
        let (reply_tx, reply_rx) = mpsc::channel();
        sender
            .send(Action {
                body: json!({"action": action, "params": params, "echo": echo}),
                reply: reply_tx,
            })
            .map_err(|_| {
                CallError::Unavailable(acb_i18n::t!("adapter.onebot11.connection_lost"))
            })?;

        reply_rx
            .recv_timeout(timeout)
            .map_err(|_| CallError::Timeout(Self::timeout_detail(timeout)))?
            .map_err(|error: LinkError| CallError::Unavailable(error.to_string()))
    }
}

/// 这条路由在**这个房间**里该调哪个动作。
///
/// 只有少数方法按场所分叉（拍一拍在 QQ 里就是两个接口：`group_poke` / `friend_poke`）。
/// 分不出来就明确报错，别猜一个发出去。
fn action_of_route(route: &Route, room: &RoomId) -> Result<&'static str, CallError> {
    match route.target {
        Target::Fixed(action) => Ok(action),
        Target::ByRoom { group, private } => match room.path.as_slice() {
            [kind, _] if kind == "group" => Ok(group),
            [kind, _] if kind == "private" => Ok(private),
            _ => Err(CallError::BadParams(acb_i18n::t!(
                "adapter.onebot11.room_needs_group_or_private",
                method = route.method,
                room = room.key()
            ))),
        },
    }
}

/// 组装真正要发给平台的参数。
///
/// 能从房间推出来的参数（群里推 `group_id`、私聊推 `user_id`）就自动补上，
/// 模型不用重复填；推不出来又没给，明确报参数不对，别让平台那边回一个看不懂的错。
fn call_payload(route: &Route, room: &RoomId, params: &Value) -> Result<Value, CallError> {
    let mut payload = match params {
        Value::Object(map) => map.clone(),
        Value::Null => serde_json::Map::new(),
        _ => {
            return Err(CallError::BadParams(acb_i18n::t!(
                "adapter.onebot11.params_must_be_object"
            )));
        }
    };

    // 房间目标参数（group_id / user_id）只能从当前房间推导，**不允许在 params 里换房间**。
    // 要操作同一会话的另一个房间，用 platform_call 的 room 参数，而不是偷换 group_id。
    match (route.from_group, route.from_private) {
        (Some(group_key), Some(private_key)) => {
            match room.path.as_slice() {
                [kind, id] if kind == "group" => {
                    fill_room_target(&mut payload, route.method, group_key, room, id)?;
                    // user_id 在这类方法里可能有两种身份：
                    // - msg.poke 在群里：user_id 是“拍谁”，不是房间目标，必须保留；
                    // - 其它（如 msg.send_forward）：user_id 指向另一个私聊房间，拒绝。
                    if route.method != "msg.poke" && payload.contains_key(private_key) {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.private_target_other_room",
                            method = route.method,
                            key = private_key,
                            room = room
                        )));
                    }
                    if route.method == "msg.poke" && !payload.contains_key(private_key) {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.poke_needs_user_id"
                        )));
                    }
                }
                [kind, id] if kind == "private" => {
                    fill_room_target(&mut payload, route.method, private_key, room, id)?;
                    if payload.contains_key(group_key) {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.group_target_other_room",
                            method = route.method,
                            key = group_key,
                            room = room
                        )));
                    }
                }
                _ => {
                    return Err(CallError::BadParams(acb_i18n::t!(
                        "adapter.onebot11.room_needs_group_or_private",
                        method = route.method,
                        room = room.key()
                    )));
                }
            }
        }
        (Some(key), None) => fill_matching_group_target(&mut payload, route.method, key, room)?,
        (None, Some(key)) => fill_matching_private_target(&mut payload, route.method, key, room)?,
        (None, None) => {}
    }

    Ok(Value::Object(payload))
}

/// 把适配器暴露的友好参数翻译成平台参数，并在本地拒绝明显歧义的写法。
///
/// core 只校验必填/类型/枚举；这一层负责 `55 → 5`、`open/close → 0/1`、
/// 文件夹改名别名优先级这类平台细节。
fn normalize_call_params(method: &str, params: &mut Value) -> Result<(), CallError> {
    let Some(map) = params.as_object_mut() else {
        return Ok(());
    };
    match method {
        "group.add_option" => {
            let add_type = map
                .get("add_type")
                .and_then(Value::as_u64)
                .filter(|value| *value == 55 || *value <= 5)
                .ok_or_else(|| {
                    CallError::BadParams(acb_i18n::t!("adapter.onebot11.add_type_invalid"))
                })?;
            let normalized = if add_type == 55 { 5 } else { add_type };
            map.insert("add_type".to_owned(), json!(normalized));
            match normalized {
                4 => {
                    if optional_text(map, "question").is_none()
                        || optional_text(map, "answer").is_none()
                    {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.add_option_question_required"
                        )));
                    }
                }
                5 => {
                    if optional_text(map, "question").is_none() {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.add_option_question_required"
                        )));
                    }
                    map.remove("answer");
                }
                _ => {
                    // 0-3 不写 question/answer；平台只认 4/5 的这两个字段。
                    map.remove("question");
                    map.remove("answer");
                }
            }
        }
        "group.search" => {
            let name = switch_value(map, "name_search")?;
            let code = switch_value(map, "code_search")?;
            if name.is_none() && code.is_none() {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.search_switch_required"
                )));
            }
            if let Some(value) = name {
                map.insert("no_finger_open".to_owned(), json!(value));
            }
            if let Some(value) = code {
                map.insert("no_code_finger_open".to_owned(), json!(value));
            }
            map.remove("name_search");
            map.remove("code_search");
        }
        "group.file.folder_rename" => {
            let _ = required_text(map, "folder_id")?;
            let name = optional_text(map, "new_folder_name")
                .or_else(|| optional_text(map, "name"))
                .map(str::to_owned)
                .ok_or_else(|| {
                    CallError::BadParams(acb_i18n::t!("adapter.onebot11.folder_name_required"))
                })?;
            map.insert("new_folder_name".to_owned(), json!(name));
            // 平台把 name 当别名；归一化成唯一的 new_folder_name，避免两个值打架。
            map.remove("name");
        }
        "group.tag_list" | "group.tag_add" | "group.tag_delete" => {
            let valid = map
                .get("group_class")
                .and_then(Value::as_u64)
                .is_some_and(|value| value <= u32::MAX as u64);
            if !valid {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.group_class_invalid"
                )));
            }
            if method != "group.tag_list" {
                let _ = required_text(map, "name")?;
            }
        }
        "face.custom_add" => {
            let file = required_text(map, "file")?;
            if file.chars().count() > MAX_DOWNLOAD_URL_LEN {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "file",
                    max = MAX_DOWNLOAD_URL_LEN
                )));
            }
            let lower = file.to_ascii_lowercase();
            let known = lower.starts_with("file://")
                || lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("base64://")
                || (lower.starts_with("data:") && lower.contains(";base64,"));
            // 裸路径是合法输入；只有带 scheme 但不在白名单里的才本地拒绝。
            if !known && lower.contains("://") {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.custom_face_source_invalid"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

/// `open`/`close` → 0/1；空值/缺省返回 None。
fn switch_value(map: &serde_json::Map<String, Value>, key: &str) -> Result<Option<u8>, CallError> {
    let Some(value) = optional_text(map, key) else {
        return Ok(None);
    };
    match value {
        "open" => Ok(Some(0)),
        "close" => Ok(Some(1)),
        other => Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.search_switch_invalid",
            field = key,
            value = other
        ))),
    }
}

/// 平台参数里的「形状对了但值不安全」这一层。
///
/// core 只校验必填、类型和枚举；数组长度、二选一这种约束在这里补。
/// 宁可本地拒绝，也不把任意大的名单或空公告 id 丢给平台。
fn validate_call_params(method: &str, params: &Value) -> Result<(), CallError> {
    let empty = serde_json::Map::new();
    let map = match params {
        Value::Object(map) => map,
        Value::Null => &empty,
        _ => {
            return Err(CallError::BadParams(acb_i18n::t!(
                "adapter.onebot11.params_must_be_object"
            )));
        }
    };

    match method {
        "msg.face" => {
            let id = map
                .get("emoji_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim();
            if id.is_empty() || id.chars().count() > MAX_EMOJI_ID_LEN {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.emoji_id_invalid",
                    max = MAX_EMOJI_ID_LEN
                )));
            }
        }
        "member.kick_many" => {
            let valid = map
                .get("user_ids")
                .and_then(Value::as_array)
                .is_some_and(|ids| {
                    !ids.is_empty()
                        && ids.len() <= MAX_BATCH_KICK
                        && ids.iter().all(|id| id.is_i64() || id.is_u64())
                });
            if !valid {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.batch_kick_invalid",
                    max = MAX_BATCH_KICK
                )));
            }
        }
        "group.notice.delete" => {
            let present = ["fid", "notice_id"].iter().any(|key| {
                map.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
            });
            if !present {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.notice_delete_needs_id"
                )));
            }
        }
        "friend.set_category" => {
            let has_id = map
                .get("categoryId")
                .is_some_and(|value| value.as_i64().is_some() || value.as_u64().is_some());
            let has_name = map
                .get("categoryName")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            if !has_id && !has_name {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.friend_category_needs_target"
                )));
            }
        }
        "group.file.folder_create" => {
            required_text(map, "name")?;
        }
        "group.file.folder_delete" => {
            required_text(map, "folder_id")?;
        }
        "group.file.move" => {
            required_text(map, "file_id")?;
            required_text(map, "parent_directory")?;
            required_text(map, "target_directory")?;
        }
        "group.file.rename" => {
            required_text(map, "file_id")?;
            required_text(map, "new_name")?;
        }
        "group.file.folder_rename" => {
            required_text(map, "folder_id")?;
        }
        "group.file.trans" => {
            required_text(map, "file_id")?;
        }
        "file.share_link" => {
            required_text(map, "fileset_id")?;
        }
        "file.private_url" => {
            let positive = map
                .get("user_id")
                .and_then(Value::as_i64)
                .is_some_and(|id| id > 0);
            if !positive {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.private_file_target_required"
                )));
            }
            required_text(map, "file_id")?;
        }
        "msg.reactions" | "msg.reaction_users" => {
            required_text(map, "message_id")?;
            if method == "msg.reaction_users" {
                required_text(map, "emojiId")?;
            }
        }
        "group.reaction" => {
            required_text(map, "message_id")?;
            required_text(map, "code")?;
        }
        "face.custom_delete" | "face.custom_modify" => {
            required_text(map, "emoji_id")?;
        }
        "face.system_search" => {
            required_text(map, "query")?;
        }
        "face.entity" => {
            let valid = map
                .get("face_id")
                .and_then(Value::as_i64)
                .is_some_and(|id| id >= 0);
            if !valid {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.face_id_invalid"
                )));
            }
        }
        "msg.voice_text" => {
            required_text(map, "message_id")?;
        }
        "msg.ocr" => {
            let source = required_text(map, "image")?;
            if source.chars().count() > MAX_OCR_SOURCE_LEN {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "image",
                    max = MAX_OCR_SOURCE_LEN
                )));
            }
        }
        "file.info" => {
            let present = ["file_id", "file"].iter().any(|key| {
                map.get(*key)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.trim().is_empty())
            });
            if !present {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.file_info_needs_identifier"
                )));
            }
        }
        "group.ai_voice" | "group.ai_voice_send" => {
            required_text(map, "character")?;
            required_text(map, "text")?;
        }
        "group.album.media" => {
            required_text(map, "album_id")?;
        }
        "account.credentials" | "account.cookies" | "account.csrf_token" => {
            if let Some(domain) = optional_text(map, "domain")
                && domain.chars().count() > 253
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "domain",
                    max = 253
                )));
            }
        }
        "account.decrypt_key" => {
            let path = required_text(map, "db_path")?;
            if path.chars().count() > 1024 {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "db_path",
                    max = 1024
                )));
            }
        }
        "system.send_packet" => {
            let cmd = required_text(map, "cmd")?;
            if cmd.chars().count() > 128 {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "cmd",
                    max = 128
                )));
            }
            if let Some(data) = optional_text(map, "data") {
                if data.chars().count() > MAX_PACKET_HEX_CHARS {
                    return Err(CallError::BadParams(acb_i18n::t!(
                        "adapter.onebot11.value_too_long",
                        field = "data",
                        max = MAX_PACKET_HEX_CHARS
                    )));
                }
                if !data.chars().all(|ch| ch.is_ascii_hexdigit()) {
                    return Err(CallError::BadParams(acb_i18n::t!(
                        "adapter.onebot11.packet_data_invalid"
                    )));
                }
            }
        }
        "file.download" => {
            let url = optional_text(map, "url");
            let base64 = optional_text(map, "base64");
            match (url, base64) {
                (Some(url), None) => {
                    let lower = url.to_ascii_lowercase();
                    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.download_scheme"
                        )));
                    }
                    if url.chars().count() > MAX_DOWNLOAD_URL_LEN {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.value_too_long",
                            field = "url",
                            max = MAX_DOWNLOAD_URL_LEN
                        )));
                    }
                }
                (None, Some(base64)) => {
                    let limit = download_limit(map)?;
                    let encoded_limit = limit.saturating_mul(4) / 3 + 4;
                    if base64.len() as u64 > encoded_limit {
                        return Err(CallError::BadParams(acb_i18n::t!(
                            "adapter.onebot11.download_too_large",
                            max = limit
                        )));
                    }
                }
                (Some(_), Some(_)) | (None, None) => {
                    return Err(CallError::BadParams(acb_i18n::t!(
                        "adapter.onebot11.download_source_required"
                    )));
                }
            }
            if let Some(name) = optional_text(map, "name")
                && name.chars().count() > MAX_DOWNLOAD_NAME_LEN
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "name",
                    max = MAX_DOWNLOAD_NAME_LEN
                )));
            }
        }
        "stream.upload_file" => {
            let stream_id = required_text(map, "stream_id")?;
            if stream_id.chars().count() > MAX_STREAM_ID_LEN
                || !stream_id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.stream_id_invalid",
                    max = MAX_STREAM_ID_LEN
                )));
            }
            if let Some(chunk) = optional_text(map, "chunk_data")
                && chunk.len() as u64 > MAX_STREAM_CHUNK_BYTES.saturating_mul(4) / 3 + 4
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.stream_chunk_too_large",
                    max = MAX_STREAM_CHUNK_BYTES
                )));
            }
            if let Some(size) = map.get("file_size").and_then(Value::as_u64)
                && size > MAX_STREAM_FILE_BYTES
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.stream_file_too_large",
                    max = MAX_STREAM_FILE_BYTES
                )));
            }
            if let Some(total) = map.get("total_chunks").and_then(Value::as_u64) {
                if total == 0 || total > 65_536 {
                    return Err(CallError::BadParams(acb_i18n::t!(
                        "adapter.onebot11.value_too_long",
                        field = "total_chunks",
                        max = 65_536
                    )));
                }
                if let Some(index) = map.get("chunk_index").and_then(Value::as_u64)
                    && index >= total
                {
                    return Err(CallError::BadParams(acb_i18n::t!(
                        "adapter.onebot11.stream_index_invalid"
                    )));
                }
            }
            if let Some(retention) = map.get("file_retention").and_then(Value::as_u64)
                && retention > MAX_STREAM_RETENTION_MS
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "file_retention",
                    max = MAX_STREAM_RETENTION_MS
                )));
            }
        }
        "stream.download_image" | "stream.download_record" | "stream.download_file" => {
            if ["file", "file_id"]
                .iter()
                .all(|key| optional_text(map, key).is_none())
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.stream_source_required"
                )));
            }
            if let Some(file) = optional_text(map, "file")
                && file.chars().count() > MAX_DOWNLOAD_URL_LEN
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "file",
                    max = MAX_DOWNLOAD_URL_LEN
                )));
            }
            if let Some(size) = map.get("chunk_size").and_then(Value::as_u64)
                && (size == 0 || size > MAX_STREAM_CHUNK_BYTES)
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "chunk_size",
                    max = MAX_STREAM_CHUNK_BYTES
                )));
            }
        }
        "msg.forward_upload"
        | "msg.forward_upload_alias"
        | "msg.send_group_forward"
        | "msg.send_private_forward" => {
            validate_forward_payload(map)?;
        }
        "group.album.qun_list" => {
            if let Some(attach) = optional_text(map, "attach_info")
                && attach.chars().count() > MAX_ALBUM_FILE_LEN
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "attach_info",
                    max = MAX_ALBUM_FILE_LEN
                )));
            }
        }
        "group.album.upload_image" | "group.album.upload_video" => {
            let album_id = required_text(map, "album_id")?;
            required_text(map, "album_name")?;
            let file = required_text(map, "file")?;
            if album_id.chars().count() > MAX_ALBUM_FILE_LEN
                || file.chars().count() > MAX_ALBUM_FILE_LEN
            {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "album_id/file",
                    max = MAX_ALBUM_FILE_LEN
                )));
            }
        }
        "group.album.delete_media" => {
            required_text(map, "album_id")?;
            let lloc = required_text(map, "lloc")?;
            if lloc.chars().count() > MAX_ALBUM_FILE_LEN {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.value_too_long",
                    field = "lloc",
                    max = MAX_ALBUM_FILE_LEN
                )));
            }
        }
        "group.album.comment" => {
            required_text(map, "album_id")?;
            required_text(map, "lloc")?;
            let content = required_text(map, "content")?;
            if content.chars().count() > MAX_ALBUM_TEXT_CHARS {
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.album_text_too_long",
                    max = MAX_ALBUM_TEXT_CHARS
                )));
            }
        }
        "group.album.like" | "group.album.unlike" => {
            required_text(map, "album_id")?;
            required_text(map, "batch_id")?;
        }
        _ => {}
    }
    Ok(())
}

fn optional_text<'a>(map: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn download_limit(map: &serde_json::Map<String, Value>) -> Result<u64, CallError> {
    let limit = map
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DOWNLOAD_BYTES);
    if limit == 0 || limit > MAX_DOWNLOAD_BYTES {
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.download_limit_invalid",
            max = MAX_DOWNLOAD_BYTES
        )));
    }
    Ok(limit)
}

/// 转发预上传/专用发送的载荷上限：节点数 + 序列化字节数。
fn validate_forward_payload(map: &serde_json::Map<String, Value>) -> Result<(), CallError> {
    let nodes = ["messages", "message"]
        .iter()
        .find_map(|key| map.get(*key).filter(|value| !value.is_null()));
    let Some(nodes) = nodes else {
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.forward_payload_invalid",
            max = MAX_FORWARD_NODES
        )));
    };
    let Some(items) = nodes.as_array() else {
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.forward_payload_invalid",
            max = MAX_FORWARD_NODES
        )));
    };
    if items.is_empty() || items.len() > MAX_FORWARD_NODES {
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.forward_payload_invalid",
            max = MAX_FORWARD_NODES
        )));
    }
    let bytes = serde_json::to_string(nodes)
        .map(|text| text.len())
        .unwrap_or(usize::MAX);
    if bytes > MAX_FORWARD_BYTES {
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.forward_payload_too_large",
            max = MAX_FORWARD_BYTES
        )));
    }
    Ok(())
}

/// 必填字符串在 core 那层只查「给了没有」；这里再把空串/纯空白挡掉。
fn required_text<'a>(
    map: &'a serde_json::Map<String, Value>,
    key: &str,
) -> Result<&'a str, CallError> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            CallError::BadParams(acb_i18n::t!(
                "adapter.onebot11.required_value_empty",
                field = key
            ))
        })
}

/// 群方法：当前房间必须是 group；params 里给了 group_id 就只能等于当前群。
fn fill_matching_group_target(
    payload: &mut serde_json::Map<String, Value>,
    method: &str,
    key: &str,
    room: &RoomId,
) -> Result<(), CallError> {
    let Some(id) = group_id_of(room) else {
        if let Some(value) = payload.get(key) {
            let target = value.as_str().unwrap_or_default();
            return Err(CallError::BadParams(acb_i18n::t!(
                "adapter.onebot11.group_target_mismatch",
                method = method,
                key = key,
                target = target,
                room = room,
                target_room = target_room_text(room, "group", target)
            )));
        }
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.group_target_missing",
            method = method,
            key = key,
            room = room.key()
        )));
    };
    fill_room_target(payload, method, key, room, id)
}

/// 私聊方法：当前房间必须是 private；params 里给了 user_id 就只能等于当前私聊对象。
fn fill_matching_private_target(
    payload: &mut serde_json::Map<String, Value>,
    method: &str,
    key: &str,
    room: &RoomId,
) -> Result<(), CallError> {
    let Some(id) = private_id_of(room) else {
        if let Some(value) = payload.get(key) {
            let target = value.as_str().unwrap_or_default();
            return Err(CallError::BadParams(acb_i18n::t!(
                "adapter.onebot11.private_target_mismatch",
                method = method,
                key = key,
                target = target,
                room = room,
                target_room = target_room_text(room, "private", target)
            )));
        }
        return Err(CallError::BadParams(acb_i18n::t!(
            "adapter.onebot11.private_target_missing",
            method = method,
            key = key,
            room = room.key()
        )));
    };
    fill_room_target(payload, method, key, room, id)
}

/// 把房间目标参数填成房间里的 id；如果 params 里已有不同的值，拒绝。
fn fill_room_target(
    payload: &mut serde_json::Map<String, Value>,
    method: &str,
    key: &str,
    room: &RoomId,
    room_id: &str,
) -> Result<(), CallError> {
    match payload.get(key) {
        Some(value) => {
            let given = value.as_str().unwrap_or_default();
            if given != room_id {
                let kind = if key == "group_id" {
                    "group"
                } else {
                    "private"
                };
                return Err(CallError::BadParams(acb_i18n::t!(
                    "adapter.onebot11.target_mismatch",
                    method = method,
                    key = key,
                    given = given,
                    room = room,
                    target_room = target_room_text(room, kind, given)
                )));
            }
        }
        None => {
            payload.insert(key.to_owned(), Value::String(room_id.to_owned()));
        }
    }
    Ok(())
}

/// 给错误信息一个能直接照抄的 room 值：用当前连接的连接名 + 目标类型 + 目标 id。
fn target_room_text(room: &RoomId, kind: &str, id: &str) -> String {
    format!("{}:{}/{}", room.connection, kind, id)
}

fn group_id_of(room: &RoomId) -> Option<&str> {
    match room.path.as_slice() {
        [kind, id] if kind == "group" => Some(id),
        _ => None,
    }
}

fn private_id_of(room: &RoomId) -> Option<&str> {
    match room.path.as_slice() {
        [kind, id] if kind == "private" => Some(id),
        _ => None,
    }
}

/// 发送类方法的流水记录。core 不认识 OneBot 消息段，翻译只能在这里做。
///
/// 只认真正改变房间状态的方法：发消息、发合并转发、发群公告。拍一拍、点赞、
/// 加表情回应这类不改变「房间里的聊天记录」，不记账。
fn outgoing_record(method: &str, params: &Value, value: &Value) -> Option<OutgoingRecord> {
    let platform_id = platform_id_of(value);
    let record = match method {
        "msg.send" => {
            let message = params.get("message").cloned().unwrap_or(Value::Null);
            // 出站不需要判「@ 了我没有」，第二个返回值不用。
            let (elements, _) = from_onebot(&message, "");
            OutgoingRecord {
                kind: OutgoingKind::Message,
                elements,
                platform_id,
                at: None,
            }
        }
        "msg.face" => OutgoingRecord {
            kind: OutgoingKind::Message,
            elements: vec![Element::text("[自定义表情]")],
            platform_id,
            at: None,
        },
        "group.ai_voice_send" => OutgoingRecord {
            kind: OutgoingKind::Message,
            elements: vec![Element::text("[AI 语音]")],
            platform_id,
            at: None,
        },
        "msg.send_forward" => {
            let count = params
                .get("messages")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let text = if count > 0 {
                format!("[合并转发 {count} 条]")
            } else {
                "[合并转发]".to_owned()
            };
            OutgoingRecord {
                kind: OutgoingKind::Event("msg.send_forward".into()),
                elements: vec![Element::text(text)],
                platform_id,
                at: None,
            }
        }
        "group.notice.send" => {
            let content = params
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            OutgoingRecord {
                kind: OutgoingKind::Event("group_notice".into()),
                elements: vec![Element::text(content)],
                platform_id,
                at: None,
            }
        }
        _ => return None,
    };
    Some(record)
}

/// 平台回包的 `message_id`。数字和字符串都收，空串当没有。
fn platform_id_of(value: &Value) -> Option<String> {
    match value.get("message_id") {
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// 平台回包 → 调用结果。
///
/// 读方法的成功结果在 `data` 里。失败按 retcode 分类：让模型能分清「我参数错了」「我没权限」
/// 「我们还没实现」这三件事，而不是一律当成失败（ADR-0020）。
fn parse_call_response(method: &str, value: Value) -> Result<Value, CallError> {
    let retcode = value.get("retcode").and_then(Value::as_i64).unwrap_or(-1);
    // 0 = 成功；1 = 已受理（部分实现异步处理也这么回）。
    if retcode == 0 || retcode == 1 {
        return Ok(value.get("data").cloned().unwrap_or(Value::Null));
    }

    let fallback_reason = acb_i18n::t!("adapter.onebot11.no_platform_reason");
    let detail = value
        .get("wording")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| value.get("message").and_then(Value::as_str))
        .unwrap_or(&fallback_reason);
    let reason = format!("{detail} (retcode {retcode})");

    Err(match retcode {
        // go-cqhttp 系沿用 HTTP 风格码。
        1400 => CallError::BadParams(reason),
        1401 | 1403 => CallError::Forbidden(reason),
        1404 => CallError::NotImplemented(reason),
        _ => CallError::UnsupportedPlatform(format!("{method}: {reason}")),
    })
}

/// 房间写法决定调哪个动作。路径是「段/段」，第一段说场所类型。
fn action_of(room: &RoomId, elements: &[Element]) -> Result<(&'static str, Value), OutboundError> {
    // OneBot 11 没有标准的 file 消息段，文件要走平台的上传动作；而且上传动作
    // 只能单独发一个文件，不能和文字/图片混在一条消息里。
    if let [Element::File { url, name }] = elements {
        return match room.path.as_slice() {
            [kind, id] if kind == "group" => Ok((
                "upload_group_file",
                json!({"group_id": id, "file": url, "name": name}),
            )),
            [kind, id] if kind == "private" => Ok((
                "upload_private_file",
                json!({"user_id": id, "file": url, "name": name}),
            )),
            _ => Err(OutboundError::Unsupported(acb_i18n::t!(
                "adapter.onebot11.unrecognized_room",
                room = room.key()
            ))),
        };
    }

    let message =
        to_onebot(elements).map_err(|error| OutboundError::Unsupported(error.to_string()))?;

    match room.path.as_slice() {
        [kind, id] if kind == "group" => Ok((
            "send_group_msg",
            json!({"group_id": id, "message": message}),
        )),
        [kind, id] if kind == "private" => Ok((
            "send_private_msg",
            json!({"user_id": id, "message": message}),
        )),
        _ => Err(OutboundError::Unsupported(acb_i18n::t!(
            "adapter.onebot11.unrecognized_room",
            room = room.key()
        ))),
    }
}

/// 平台回包 → 发送结果。
fn parse_response(value: Value) -> Result<Sent, OutboundError> {
    let retcode = value.get("retcode").and_then(Value::as_i64).unwrap_or(-1);
    // 0 = 成功；1 = 已受理（异步发送的实现会这么回）。
    if retcode != 0 && retcode != 1 {
        let fallback_reason = acb_i18n::t!("adapter.onebot11.no_platform_reason");
        let detail = value
            .get("wording")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .or_else(|| value.get("message").and_then(Value::as_str))
            .unwrap_or(&fallback_reason);
        return Err(OutboundError::Rejected(format!(
            "{detail} (retcode {retcode})"
        )));
    }

    let platform_id = value
        .get("data")
        .and_then(platform_id_of)
        .unwrap_or_default();

    Ok(Sent {
        platform_id,
        at: now_millis(),
    })
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ALIASES;
    use acb_kernel::{Effect, OutgoingKind, RecordMode, Risk, Scope, TargetRole, Verification};

    #[test]
    fn group_room_maps_to_send_group_msg() {
        let room = RoomId::new("qq", "group/456").expect("合法");
        let (action, params) = action_of(&room, &[Element::text("hi")]).expect("能编");
        assert_eq!(action, "send_group_msg");
        assert_eq!(params["group_id"], "456");
        assert_eq!(params["message"][0]["type"], "text");
    }

    #[test]
    fn private_room_maps_to_send_private_msg() {
        let room = RoomId::new("qq", "private/10001").expect("合法");
        let (action, params) = action_of(&room, &[Element::text("hi")]).expect("能编");
        assert_eq!(action, "send_private_msg");
        assert_eq!(params["user_id"], "10001");
    }

    #[test]
    fn group_file_maps_to_upload_group_file() {
        let room = RoomId::new("qq", "group/456").expect("合法");
        let (action, params) = action_of(
            &room,
            &[Element::File {
                url: "/tmp/a.mp4".into(),
                name: "a.mp4".into(),
            }],
        )
        .expect("单个文件走上传");
        assert_eq!(action, "upload_group_file");
        assert_eq!(params["group_id"], "456");
        assert_eq!(params["file"], "/tmp/a.mp4");
        assert_eq!(params["name"], "a.mp4");
    }

    #[test]
    fn private_file_maps_to_upload_private_file() {
        let room = RoomId::new("qq", "private/10001").expect("合法");
        let (action, params) = action_of(
            &room,
            &[Element::File {
                url: "/tmp/a.zip".into(),
                name: "a.zip".into(),
            }],
        )
        .expect("单个文件走上传");
        assert_eq!(action, "upload_private_file");
        assert_eq!(params["user_id"], "10001");
    }

    #[test]
    fn file_mixed_with_text_is_refused() {
        let room = RoomId::new("qq", "group/456").expect("合法");
        let error = action_of(
            &room,
            &[
                Element::text("看这个"),
                Element::File {
                    url: "/tmp/a.zip".into(),
                    name: "a.zip".into(),
                },
            ],
        )
        .expect_err("文件不能和文字混在一条消息");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }

    #[test]
    fn unknown_room_shape_is_refused() {
        let room = RoomId::new("qq", "123").expect("合法");
        let error = action_of(&room, &[Element::text("hi")]).expect_err("认不出房间");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }

    #[test]
    fn uploads_get_a_longer_timeout_and_timeouts_say_result_is_unknown() {
        let outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        assert_eq!(outbound.send_timeout(&[Element::text("hi")]), SEND_TIMEOUT);
        assert_eq!(
            outbound.send_timeout(&[Element::Image {
                url: "/tmp/a.png".into(),
                alt: None,
            }]),
            UPLOAD_TIMEOUT
        );

        let detail = OneBotSnowLumaOutbound::timeout_detail(UPLOAD_TIMEOUT);
        assert!(detail.contains("45 seconds"), "{detail}");
        assert!(detail.contains("may already have executed"), "{detail}");
        assert!(detail.contains("Do not resend blindly"), "{detail}");
    }

    #[test]
    fn ok_response_yields_the_message_id() {
        let sent = parse_response(json!({
            "status": "ok",
            "retcode": 0,
            "data": {"message_id": 9001}
        }))
        .expect("应当成功");
        assert_eq!(sent.platform_id, "9001");
    }

    #[test]
    fn error_response_is_rejected_with_the_reason() {
        let error = parse_response(json!({
            "status": "failed",
            "retcode": 100,
            "wording": "群号不存在"
        }))
        .expect_err("应当报错");
        match error {
            OutboundError::Rejected(detail) => assert!(detail.contains("群号不存在")),
            other => panic!("应当是拒收，收到 {other:?}"),
        }
    }

    #[test]
    fn missing_retcode_is_treated_as_failure() {
        let error = parse_response(json!({"echo": "x"})).expect_err("没有 retcode 不算成功");
        assert!(matches!(error, OutboundError::Rejected(_)));
    }

    #[test]
    fn sending_before_the_connection_is_up_is_refused() {
        let mut outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/456").expect("合法");

        let error = outbound
            .send(&room, &[Element::text("x")])
            .expect_err("没连上就不该假装发成功");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }

    #[test]
    fn a_room_on_another_connection_is_refused() {
        let mut outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq2", "group/123").expect("合法");

        let error = outbound
            .send(&room, &[Element::text("x")])
            .expect_err("一条连接只管它自己那个号");
        assert!(matches!(error, OutboundError::Unavailable(_)));
    }

    #[test]
    fn a_deep_path_is_not_a_qq_room() {
        let room = RoomId::new("qq", "guild/9/channel/7").expect("合法");
        let error = action_of(&room, &[Element::text("hi")]).expect_err("认不出房间");
        assert!(matches!(error, OutboundError::Unsupported(_)));
    }

    // ── 平台方法目录与调用 ────────────────────────────────────────────────

    /// 目录要按「人能做的事」铺开，不是只挑安全的。风险靠分级 + 配置开关管，不靠删。
    #[test]
    fn catalog_covers_read_write_and_destructive() {
        let outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let methods = outbound.catalog("qq");
        assert!(methods.len() >= 40, "能力面太窄：只有 {} 个", methods.len());

        for risk in [Risk::Read, Risk::Write, Risk::Destructive] {
            assert!(
                methods.iter().any(|method| method.risk == risk),
                "没有 {risk:?} 级别的方法"
            );
        }

        for method in methods.iter() {
            // 强制 resource.method 写法，目录靠点号前那段分组。
            assert!(
                method.name.contains('.'),
                "{} 没按 resource.method 命名",
                method.name
            );
            // 说明里别出现平台方法名之外的内部词，模型看的是中文说明。
            assert!(!method.summary.is_empty(), "{} 没写说明", method.name);
            assert!(!method.tags.is_empty(), "{} 没打标签", method.name);
        }
    }

    /// 每个声明的方法都得有对应的平台动作，否则声明了也调不动；反过来路由也不许悬空。
    #[test]
    fn declared_methods_and_routes_match() {
        let outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        for method in outbound.catalog("qq").iter() {
            assert!(
                ROUTES
                    .iter()
                    .any(|route| route.method == method.name.as_ref()),
                "{} 声明了但没有路由",
                method.name
            );
        }
        for route in ROUTES {
            assert!(
                catalog()
                    .iter()
                    .any(|method| method.name.as_ref() == route.method),
                "{} 有路由但没声明，模型永远查不到它",
                route.method
            );
        }
    }

    /// 目录要把“房间目标”和“成员目标”声明清楚，core 才能兜底拒绝换房间。
    #[test]
    fn catalog_declares_room_and_member_targets() {
        let methods = catalog();
        let poke = methods
            .iter()
            .find(|method| method.name.as_ref() == "msg.poke")
            .expect("有 poke");
        assert!(
            poke.targets.iter().any(
                |target| target.param.as_ref() == "group_id" && target.role == TargetRole::Room
            )
        );
        assert!(
            poke.targets
                .iter()
                .any(|target| target.param.as_ref() == "user_id"
                    && target.role == TargetRole::Member)
        );

        let forward = methods
            .iter()
            .find(|method| method.name.as_ref() == "msg.send_forward")
            .expect("有合并转发");
        assert!(
            forward.targets.iter().any(
                |target| target.param.as_ref() == "group_id" && target.role == TargetRole::Room
            )
        );
        assert!(forward
            .targets
            .iter()
            .any(|target| target.param.as_ref() == "user_id" && target.role == TargetRole::Room));
    }

    /// 目录里不许出现重复方法名——否则 platform_call 会静默用第一条。
    #[test]
    fn method_names_are_unique() {
        let outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let mut seen = std::collections::HashSet::new();
        for method in outbound.catalog("qq").iter() {
            assert!(
                seen.insert(method.name.as_ref()),
                "{} 重复声明了",
                method.name
            );
        }
    }

    /// 「人在群里能做的」这一类必须都在：发言、看图、查成员、踢人、禁言、改名片。
    #[test]
    fn catalog_covers_what_a_person_can_do_in_a_group() {
        let outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let methods = outbound.catalog("qq");
        let names: Vec<&str> = methods.iter().map(|m| m.name.as_ref()).collect();
        for expected in [
            "msg.send",
            "msg.history",
            "msg.recall",
            "msg.forward",
            "member.list",
            "group.notice.get",
            "group.file.list",
            "member.kick",
            "member.kick_many",
            "group.mute",
            "group.mute_all",
            "group.member_permissions",
            "group.invite_policy",
            "group.history_visibility",
            "group.admin_settings",
            "group.share_link",
            "group.detail",
            "group.shut_list",
            "group.signed_list",
            "group.ignored_requests",
            "group.notice.delete",
            "member.card",
            "group.name",
            "group.file.upload",
            "group.file.folder_create",
            "group.file.folder_delete",
            "group.file.move",
            "group.file.rename",
            "msg.face",
            "msg.reactions",
            "msg.reaction_users",
            "group.reaction",
            "msg.voice_text",
            "msg.ocr",
            "group.ai_characters",
            "group.ai_voice",
            "group.ai_voice_send",
            "group.album.list",
            "group.album.media",
            "msg.poke",
            "like.send",
        ] {
            assert!(names.contains(&expected), "缺 {expected}");
        }
    }

    /// 「号上的事」和「房间里的事」要分清：前者不该逼模型先编一个群号出来。
    #[test]
    fn account_level_methods_are_connection_scoped() {
        let outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        for method in outbound.catalog("qq").iter() {
            let account_level = matches!(
                method.name.as_ref(),
                "account.login_info"
                    | "account.status"
                    | "account.version"
                    | "friend.list"
                    | "friend.get_info"
                    | "friend.approve"
                    | "friend.delete"
                    | "friend.remark"
                    | "friend.categories"
                    | "friend.set_category"
                    | "friend.one_way"
                    | "friend.doubt_list"
                    | "friend.doubt_approve"
                    | "group.list"
                    | "group.ignored_requests"
                    | "file.private_url"
                    | "file.info"
                    | "file.share_link"
                    | "group.tag_list"
                    | "group.tag_add"
                    | "group.tag_delete"
                    | "face.custom_list"
                    | "face.custom_detail"
                    | "face.custom_delete"
                    | "face.custom_modify"
                    | "face.custom_add"
                    | "face.system_list"
                    | "face.system_search"
                    | "face.entity"
                    | "like.send"
                    | "system.check_url"
                    | "account.credentials"
                    | "account.cookies"
                    | "account.csrf_token"
                    | "account.clientkey"
                    | "account.decrypt_key"
                    | "system.send_packet"
                    | "system.bot_exit"
                    | "system.restart"
                    | "system.clean_cache"
                    | "file.download"
                    | "stream.upload_file"
                    | "stream.download_image"
                    | "stream.download_record"
                    | "stream.download_file"
                    | "stream.test_download"
                    | "stream.clean"
                    // 批次 9：账号/文件/空间级动作不走房间目标。
                    | "friend.dress"
                    | "msg.mark_all_read"
                    | "account.online_clients"
                    | "account.profile_likes"
                    | "account.user_status"
                    | "account.diy_status"
                    | "account.input_status"
                    | "account.online_status"
                    | "account.avatar"
                    | "account.profile"
                    | "account.long_nick"
                    | "account.can_send_image"
                    | "account.can_send_record"
                    | "account.rkey"
                    | "account.rkey_server"
                    | "file.collection_list"
                    | "file.flash_task_create"
                    | "file.flash_file_list"
                    | "file.flash_file_url"
                    | "file.flash_file_delete"
                    | "file.flash_file_rename"
                    | "file.flash_msg_send"
                    | "file.fileset_list"
                    | "file.fileset_id"
                    | "file.fileset_info"
                    | "file.fileset_download"
                    | "face.custom_move_front"
                    | "msg.mini_app_ark"
                    | "msg.translate"
                    | "face.super_id"
                    | "account.qzone.feeds"
                    | "account.qzone.list"
                    | "account.qzone.detail"
                    | "account.qzone.send"
                    | "account.qzone.comment"
                    | "account.qzone.delete"
                    | "account.qzone.like"
                    | "account.qzone.unlike"
                    | "account.qzone.ban"
                    | "account.qzone.right"
                    | "account.model_show"
                    | "account.model_show_set"
                    | "msg.word_slices"
                    | "file.collection_create"
                    | "friend.recent_contacts"
                    | "system.packet_status"
            );
            let expected = if account_level {
                Scope::Connection
            } else {
                Scope::Room
            };
            assert_eq!(method.scope, expected, "{} 的作用域标错了", method.name);
        }
    }

    #[test]
    fn calling_before_the_connection_is_up_is_refused() {
        let mut outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/456").expect("合法");
        let error = outbound
            .call(&room, "msg.forward", &json!({"id": "x"}))
            .expect_err("没连上就不该假装调成功");
        assert!(matches!(error, CallError::Unavailable(_)));
    }

    #[test]
    fn a_call_for_another_connection_is_refused() {
        let mut outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq2", "group/123").expect("合法");
        let error = outbound
            .call(&room, "msg.get", &json!({"message_id": "1"}))
            .expect_err("一条连接只管它自己那个号");
        assert!(matches!(error, CallError::Forbidden(_)));
    }

    #[test]
    fn an_undeclared_method_is_unknown() {
        let mut outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/123").expect("合法");
        let error = outbound
            .call(&room, "group.transfer_owner", &json!({}))
            .expect_err("没声明就调不动");
        assert!(matches!(error, CallError::UnknownMethod { .. }));
    }

    #[test]
    fn success_returns_only_the_data_payload() {
        let result = parse_call_response(
            "msg.forward",
            json!({"status": "ok", "retcode": 0, "data": {"messages": [1, 2]}}),
        )
        .expect("应当成功");
        assert_eq!(result["messages"][1], 2);
    }

    /// 区分「我们还没实现」和「参数错了」——模型靠这个自己纠正。
    #[test]
    fn platform_retcodes_map_to_distinct_errors() {
        let not_implemented = parse_call_response(
            "msg.forward",
            json!({"retcode": 1404, "wording": "API 不存在"}),
        )
        .expect_err("1404 是实现层面缺");
        assert!(matches!(not_implemented, CallError::NotImplemented(_)));

        let bad = parse_call_response(
            "member.info",
            json!({"retcode": 1400, "wording": "参数错误"}),
        )
        .expect_err("1400 是参数错");
        assert!(matches!(bad, CallError::BadParams(_)));

        let denied =
            parse_call_response("msg.get", json!({"retcode": 1403, "wording": "权限不足"}))
                .expect_err("1403 是没权限");
        assert!(matches!(denied, CallError::Forbidden(_)));
    }

    /// 合并转发不给 id：先说清楚要什么，别拿着空参数去平台上撞。
    #[test]
    fn forward_content_without_an_id_is_a_param_error() {
        let mut outbound = OneBotSnowLumaOutbound::new(LinkSlot::new(), "qq");
        let room = RoomId::new("qq", "group/1").expect("合法");
        let error = outbound
            .call(&room, "msg.forward", &json!({}))
            .expect_err("要 id");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");
    }

    #[test]
    fn every_op_is_hierarchical_tagged_and_routed() {
        let methods = catalog();
        assert_eq!(methods.len(), 192, "白名单清零后应有 192 个层级 op");
        assert!(!methods.is_empty());
        for method in methods.iter() {
            let name = method.name.as_ref();
            assert!(name.contains('.'), "{name} 不是层级 op");
            assert!(!method.tags.is_empty(), "{name} 没有标签");
            assert!(
                method.has_tag(match method.risk {
                    Risk::Read => acb_kernel::outbound::tags::READ,
                    Risk::Write => acb_kernel::outbound::tags::WRITE,
                    Risk::Destructive | Risk::Critical => {
                        acb_kernel::outbound::tags::DESTRUCTIVE
                    }
                }),
                "{name} 缺少风险标签"
            );
            assert!(
                method.has_tag(match method.scope {
                    Scope::Room => acb_kernel::outbound::tags::ROOM,
                    Scope::Connection => acb_kernel::outbound::tags::CONNECTION,
                }),
                "{name} 缺少作用范围标签"
            );
            assert!(
                ROUTES.iter().any(|route| route.method == name),
                "{name} 没有路由"
            );
            // 目标声明要么为空（连接级/无房间目标），要么每个参数名都写清楚。
            for target in method.targets.iter() {
                assert!(!target.param.is_empty(), "{name} 的 TargetSpec 参数名为空");
                assert!(
                    !target.room_kind.is_empty(),
                    "{name} 的 TargetSpec 房间类型为空"
                );
            }
            match &method.verification {
                Verification::Verified { source } => {
                    assert!(!source.is_empty(), "{name} 的 Verified 没有出处");
                }
                Verification::Unverified { reason, experiment } => {
                    assert!(!reason.is_empty(), "{name} 的 Unverified 没有原因");
                    assert!(!experiment.is_empty(), "{name} 的 Unverified 没有实验");
                }
            }
        }
    }

    /// 别名挂在主 op 上，目录里没有别名条目；解析由 core 的 `find_platform_op`
    /// 归一化（risk / verification 都是主 op 的）。
    #[test]
    fn aliases_attach_to_the_main_op_and_stay_out_of_the_catalog() {
        let methods = catalog();
        for (alias, main, _action) in ALIASES {
            assert_ne!(alias, main, "别名不能等于主 op");
            assert!(
                !methods.iter().any(|method| method.name.as_ref() == *alias),
                "目录里不能出现别名条目 {alias}"
            );
            let spec = methods
                .iter()
                .find(|method| method.name.as_ref() == *main)
                .unwrap_or_else(|| panic!("主 op {main} 不在目录里"));
            assert!(
                spec.aliases.iter().any(|name| name.as_ref() == *alias),
                "{alias} 没有挂到主 op {main} 上"
            );
            assert!(
                spec.verification.is_verified(),
                "别名继承主 op 的 verification：{main} 必须是 Verified"
            );
        }
    }

    /// 覆盖矩阵是唯一一份 200 个动作的清单。测试直接读文档：
    /// - 已暴露的动作必须真的在 ROUTES 里；
    /// - 未暴露的动作必须命中白名单，且每条都写了理由；
    /// - 白名单非空时不允许出现空理由。
    #[test]
    fn action_coverage_matrix_is_complete_and_exceptions_have_reasons() {
        const COVERAGE: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../docs/plan/snowluma-action-coverage.md"
        ));
        // 白名单只允许放「技术上无法调用」的动作；8 个平台占位/未实现动作都有
        // 合法协议调用，因此白名单为空。常量保留是为了以后真有不可调用动作时
        // 仍能要求逐条理由。
        const UNEXPOSED: &[(&str, &str)] = &[];

        let rows: Vec<(String, String)> = COVERAGE
            .lines()
            .filter(|line| line.trim_start().starts_with("| `"))
            .filter_map(|line| {
                let cells: Vec<&str> = line
                    .trim()
                    .trim_matches('|')
                    .split('|')
                    .map(str::trim)
                    .collect();
                let action = cells.first()?.trim_matches('`').to_owned();
                let status = (*cells.get(1)?).to_owned();
                Some((action, status))
            })
            .collect();
        assert_eq!(rows.len(), 200, "覆盖矩阵必须覆盖 200 个唯一动作");

        let routed: std::collections::BTreeSet<&str> = ROUTES
            .iter()
            .flat_map(|route| match route.target {
                Target::Fixed(action) => vec![action],
                Target::ByRoom { group, private } => vec![group, private],
            })
            .chain(ALIASES.iter().map(|(_, _, action)| *action))
            .collect();

        let mut exposed = 0usize;
        let mut unexposed: Vec<&str> = Vec::new();
        for (action, status) in &rows {
            if status.contains("已暴露") {
                exposed += 1;
                assert!(
                    routed.contains(action.as_str()),
                    "矩阵标已暴露但 ROUTES 里没有动作 {action}"
                );
            } else if status.contains("未暴露") {
                unexposed.push(action);
            }
        }
        assert_eq!(exposed, 200, "200 个动作都必须有 op 或别名");
        assert_eq!(
            unexposed.len(),
            UNEXPOSED.len(),
            "未暴露清单必须逐字等于白名单：{unexposed:?}"
        );
        for (action, reason) in UNEXPOSED {
            assert!(
                unexposed.contains(action),
                "白名单里的 {action} 在矩阵里不是未暴露"
            );
            assert!(!reason.trim().is_empty(), "例外 {action} 必须有理由");
        }
    }

    #[test]
    fn unified_send_and_history_dispatch_by_room() {
        let room = |path: &str| RoomId::new("qq", path).expect("合法");
        let send = ROUTES
            .iter()
            .find(|route| route.method == "msg.send")
            .expect("msg.send 路由");
        assert_eq!(
            action_of_route(send, &room("group/456")).expect("群"),
            "send_group_msg"
        );
        assert_eq!(
            action_of_route(send, &room("private/10002")).expect("私聊"),
            "send_private_msg"
        );

        let history = ROUTES
            .iter()
            .find(|route| route.method == "msg.history")
            .expect("msg.history 路由");
        assert_eq!(
            action_of_route(history, &room("group/456")).expect("群"),
            "get_group_msg_history"
        );
        assert_eq!(
            action_of_route(history, &room("private/10002")).expect("私聊"),
            "get_friend_msg_history"
        );
    }

    /// 在群里调 `group.*` 不用重复填群号。
    #[test]
    fn group_id_is_filled_from_the_room() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "member.list")
            .expect("有这个路由");
        let room = RoomId::new("qq", "group/456").expect("合法");

        let payload = call_payload(route, &room, &json!({})).expect("能补");
        assert_eq!(payload["group_id"], "456");

        // 显式给了别的群：拒绝，不允许在 params 里换房间。
        let error = call_payload(route, &room, &json!({"group_id": "999"}))
            .expect_err("不能在 params 里换群");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");
    }

    #[test]
    fn poke_in_group_keeps_the_member_target_and_fills_the_group() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "msg.poke")
            .expect("有 poke 路由");
        let room = RoomId::new("qq", "group/456").expect("合法");

        let payload = call_payload(route, &room, &json!({"user_id": "10002"})).expect("能补");
        assert_eq!(payload["group_id"], "456");
        assert_eq!(payload["user_id"], "10002", "群里的 user_id 是拍谁");

        let error = call_payload(route, &room, &json!({})).expect_err("群里必须给 user_id");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");
    }

    #[test]
    fn poke_in_private_fills_the_target() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "msg.poke")
            .expect("有 poke 路由");
        let room = RoomId::new("qq", "private/10002").expect("合法");

        let payload = call_payload(route, &room, &json!({})).expect("能补");
        assert_eq!(payload["user_id"], "10002");
        assert!(payload.get("group_id").is_none());
    }

    #[test]
    fn send_forward_uses_the_room_target_and_refuses_the_other_kind() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "msg.send_forward")
            .expect("有转发布路由");
        let group = RoomId::new("qq", "group/456").expect("合法");
        let private = RoomId::new("qq", "private/10002").expect("合法");
        let messages = json!([{"type": "node", "data": {}}]);

        let payload = call_payload(route, &group, &json!({"messages": messages})).expect("能补");
        assert_eq!(payload["group_id"], "456");
        assert!(payload.get("user_id").is_none());

        let error = call_payload(
            route,
            &group,
            &json!({"messages": messages, "user_id": "10002"}),
        )
        .expect_err("群里不能把目标换成私聊");
        assert!(matches!(error, CallError::BadParams(_)), "{error:?}");

        let payload = call_payload(route, &private, &json!({"messages": messages})).expect("能补");
        assert_eq!(payload["user_id"], "10002");
    }

    /// 私聊里调要群号的方法又不给群号：明确报参数不对。
    #[test]
    fn group_method_without_a_group_room_is_a_param_error() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "member.info")
            .expect("有这个路由");
        let room = RoomId::new("qq", "private/10001").expect("合法");
        let error = call_payload(route, &room, &json!({"user_id": "1"})).expect_err("补不出来");
        assert!(matches!(error, CallError::BadParams(_)));
    }

    /// 发送类方法的流水记录：内容由适配器翻译，core 不认识 OneBot 消息段。
    #[test]
    fn outgoing_sends_become_transcript_records() {
        let params = json!({"group_id": "456", "message": [
            {"type": "text", "data": {"text": "看这个"}},
            {"type": "image", "data": {"file": "https://example.com/a.png"}},
        ]});
        let record =
            outgoing_record("msg.send", &params, &json!({"message_id": 9001})).expect("发送要记账");
        assert_eq!(record.kind, OutgoingKind::Message);
        assert_eq!(record.platform_id.as_deref(), Some("9001"));
        assert_eq!(record.elements.len(), 2);
        assert!(matches!(record.elements[0], Element::Text { .. }));

        // 读方法不记账：它们不改变房间里的聊天记录。
        assert!(outgoing_record("msg.get", &json!({"message_id": "1"}), &Value::Null).is_none());
    }

    /// 公告和合并转发在流水里是事件：内容概括成一两句话，不是原样重放。
    #[test]
    fn group_notice_and_forward_are_events() {
        let notice = outgoing_record(
            "group.notice.send",
            &json!({"content": "明天维护"}),
            &json!({"notice_id": 1}),
        )
        .expect("公告要记账");
        assert!(
            matches!(&notice.kind, OutgoingKind::Event(name) if name == "group_notice"),
            "{:?}",
            notice.kind
        );
        assert_eq!(notice.elements, vec![Element::text("明天维护")]);

        let forward = outgoing_record(
            "msg.send_forward",
            &json!({"messages": [{}, {}, {}]}),
            &json!({"message_id": "f-1"}),
        )
        .expect("转发要记账");
        assert!(
            matches!(&forward.kind, OutgoingKind::Event(name) if name == "msg.send_forward"),
            "{:?}",
            forward.kind
        );
        assert_eq!(forward.elements, vec![Element::text("[合并转发 3 条]")]);
    }

    /// 目录的执行契约：哪些方法会改变房间，core 靠这几个字段判断。
    #[test]
    fn catalog_declares_effect_and_record() {
        let methods = catalog();
        let method = |name: &str| {
            methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"))
        };

        let send = method("msg.send");
        assert_eq!(send.effect, Effect::Send);
        assert_eq!(send.record, RecordMode::Message);

        let notice = method("group.notice.send");
        assert_eq!(notice.effect, Effect::Send, "公告也是“发出去”");
        assert_eq!(notice.record, RecordMode::Event);

        let kick = method("member.kick");
        assert_eq!(kick.effect, Effect::Write);
        assert_eq!(kick.record, RecordMode::None, "踢人不写聊天流水");

        let recall = method("msg.recall");
        assert_eq!(recall.effect, Effect::Delete);
        assert_eq!(recall.record, RecordMode::None);

        let face = method("msg.face");
        assert_eq!(face.effect, Effect::Send);
        assert_eq!(face.record, RecordMode::Message, "收藏表情也是一条消息");

        let notice = method("group.notice.delete");
        assert_eq!(notice.effect, Effect::Delete);
        assert_eq!(notice.record, RecordMode::None);

        let folder = method("group.file.folder_delete");
        assert_eq!(folder.effect, Effect::Delete, "删文件夹要标成删除");
        assert_eq!(folder.record, RecordMode::None);

        let face = method("face.custom_delete");
        assert_eq!(face.effect, Effect::Delete, "删收藏表情要标成删除");
        assert_eq!(face.record, RecordMode::None);
    }

    /// 批次 4 的新 op：risk 显式、路由确定；删除类必须能看出「删了什么」。
    #[test]
    fn batch_four_ops_have_explicit_risk_and_routes() {
        let methods = catalog();
        for (name, risk, action, effect) in [
            (
                "group.file.folder_create",
                Risk::Write,
                "create_group_file_folder",
                Effect::Write,
            ),
            (
                "group.file.folder_delete",
                Risk::Destructive,
                "delete_group_file_folder",
                Effect::Delete,
            ),
            (
                "group.file.move",
                Risk::Write,
                "move_group_file",
                Effect::Write,
            ),
            (
                "group.file.rename",
                Risk::Write,
                "rename_group_file",
                Effect::Write,
            ),
            (
                "file.private_url",
                Risk::Read,
                "get_private_file_url",
                Effect::Read,
            ),
            (
                "msg.reactions",
                Risk::Read,
                "get_msg_emoji_likes",
                Effect::Read,
            ),
            (
                "msg.reaction_users",
                Risk::Read,
                "fetch_emoji_like",
                Effect::Read,
            ),
            (
                "group.reaction",
                Risk::Write,
                "set_group_reaction",
                Effect::Write,
            ),
            (
                "face.custom_list",
                Risk::Read,
                "fetch_custom_face",
                Effect::Read,
            ),
            (
                "face.custom_detail",
                Risk::Read,
                "fetch_custom_face_detail",
                Effect::Read,
            ),
            (
                "face.custom_delete",
                Risk::Destructive,
                "delete_custom_face",
                Effect::Delete,
            ),
            (
                "face.custom_modify",
                Risk::Write,
                "modify_custom_face",
                Effect::Write,
            ),
            (
                "face.system_list",
                Risk::Read,
                "fetch_sys_faces",
                Effect::Read,
            ),
            (
                "face.system_search",
                Risk::Read,
                "search_sys_faces",
                Effect::Read,
            ),
            ("face.entity", Risk::Read, "fetch_face_entity", Effect::Read),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert_eq!(method.risk, risk, "{name} 的 risk 标错了");
            assert_eq!(method.effect, effect, "{name} 的 effect 标错了");
            if risk == Risk::Destructive {
                assert!(
                    method.has_tag(acb_kernel::outbound::tags::DESTRUCTIVE),
                    "{name} 缺少 destructive 标签"
                );
            }
            let route = ROUTES
                .iter()
                .find(|route| route.method == name)
                .unwrap_or_else(|| panic!("{name} 没有路由"));
            match route.target {
                Target::Fixed(target) => assert_eq!(target, action, "{name} 路由不对"),
                Target::ByRoom { .. } => panic!("{name} 不该按房间分叉"),
            }
        }
    }

    /// 删除/移动这类有状态前提的动作，前提必须写在描述里，不能让模型去猜。
    #[test]
    fn state_dependent_ops_state_their_preconditions() {
        let methods = catalog();
        for (name, keyword) in [
            ("group.file.folder_delete", "必须存在"),
            ("group.file.move", "必须存在于"),
            ("group.file.rename", "必须存在于"),
            ("face.custom_delete", "必须存在于"),
            ("msg.reaction_users", "必须在该消息的回应里"),
            ("group.reaction", "必须存在"),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert!(
                method.summary.contains(keyword),
                "{name} 的描述没写清前提「{keyword}」：{}",
                method.summary
            );
        }
    }

    #[test]
    fn group_file_crud_requires_non_empty_values() {
        assert!(validate_call_params("group.file.folder_create", &json!({"name": "  "})).is_err());
        assert!(
            validate_call_params("group.file.folder_delete", &json!({"folder_id": ""})).is_err()
        );
        assert!(
            validate_call_params(
                "group.file.move",
                &json!({"file_id": "1", "parent_directory": "/", "target_directory": ""})
            )
            .is_err()
        );
        assert!(
            validate_call_params(
                "group.file.rename",
                &json!({"file_id": "1", "new_name": " "})
            )
            .is_err()
        );
        assert!(
            validate_call_params(
                "group.file.move",
                &json!({"file_id": "1", "parent_directory": "/", "target_directory": "/a"})
            )
            .is_ok()
        );
    }

    #[test]
    fn private_file_url_requires_a_positive_user() {
        assert!(
            validate_call_params("file.private_url", &json!({"user_id": 0, "file_id": "f1"}))
                .is_err()
        );
        assert!(
            validate_call_params(
                "file.private_url",
                &json!({"user_id": 10001, "file_id": "f1"})
            )
            .is_ok()
        );
    }

    #[test]
    fn face_and_reaction_identifiers_are_validated() {
        assert!(validate_call_params("face.custom_delete", &json!({"emoji_id": ""})).is_err());
        assert!(validate_call_params("face.system_search", &json!({"query": " "})).is_err());
        assert!(validate_call_params("face.entity", &json!({"face_id": -1})).is_err());
        assert!(
            validate_call_params(
                "msg.reaction_users",
                &json!({"message_id": "1", "emojiId": ""})
            )
            .is_err()
        );
        assert!(
            validate_call_params("group.reaction", &json!({"message_id": "1", "code": ""}))
                .is_err()
        );
        assert!(
            validate_call_params("group.reaction", &json!({"message_id": "1", "code": "abc"}))
                .is_ok()
        );
    }

    /// 批次 3 的新 op 必须逐个显式标 risk，并有确定的路由；破坏性群管理默认被
    /// `allow_destructive` 挡住，标签只表达「需要管理员」，不在 core 里猜身份。
    #[test]
    fn batch_three_ops_have_explicit_risk_and_routes() {
        let methods = catalog();
        for (name, risk, action) in [
            ("msg.face", Risk::Write, "send_custom_face"),
            (
                "member.kick_many",
                Risk::Destructive,
                "set_group_kick_members",
            ),
            (
                "group.member_permissions",
                Risk::Destructive,
                "set_group_member_permissions",
            ),
            (
                "group.invite_policy",
                Risk::Destructive,
                "set_group_member_invite_policy",
            ),
            (
                "group.history_visibility",
                Risk::Destructive,
                "set_group_new_member_history_visibility",
            ),
            (
                "group.admin_settings",
                Risk::Read,
                "get_group_admin_settings",
            ),
            ("group.share_link", Risk::Read, "get_group_share_link"),
            ("group.detail", Risk::Read, "get_group_detail_info"),
            ("group.shut_list", Risk::Read, "get_group_shut_list"),
            ("group.signed_list", Risk::Read, "get_group_signed_list"),
            (
                "group.ignored_requests",
                Risk::Read,
                "get_group_ignored_notifies",
            ),
            ("friend.remark", Risk::Write, "set_friend_remark"),
            ("friend.categories", Risk::Read, "get_friends_with_category"),
            ("friend.set_category", Risk::Write, "set_friends_category"),
            (
                "friend.one_way",
                Risk::Read,
                "get_unidirectional_friend_list",
            ),
            (
                "friend.doubt_list",
                Risk::Read,
                "get_doubt_friends_add_request",
            ),
            (
                "friend.doubt_approve",
                Risk::Write,
                "set_doubt_friends_add_request",
            ),
            (
                "group.notice.delete",
                Risk::Destructive,
                "_del_group_notice",
            ),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert_eq!(method.risk, risk, "{name} 的 risk 标错了");
            if risk == Risk::Destructive {
                assert!(
                    method.has_tag(acb_kernel::outbound::tags::REQUIRES_ADMIN),
                    "{name} 是破坏性群管理，应带 requires_admin 标签"
                );
            }
            let route = ROUTES
                .iter()
                .find(|route| route.method == name)
                .unwrap_or_else(|| panic!("{name} 没有路由"));
            match route.target {
                Target::Fixed(target) => assert_eq!(target, action, "{name} 路由不对"),
                Target::ByRoom { .. } => panic!("{name} 不该按房间分叉"),
            }
        }
    }

    #[test]
    fn custom_face_rejects_empty_and_oversized_ids() {
        assert!(validate_call_params("msg.face", &json!({"emoji_id": "  "})).is_err());
        let long = "a".repeat(MAX_EMOJI_ID_LEN + 1);
        assert!(validate_call_params("msg.face", &json!({"emoji_id": long})).is_err());
        assert!(
            validate_call_params(
                "msg.face",
                &json!({"emoji_id": "0123456789abcdef0123456789abcdef"})
            )
            .is_ok()
        );
    }

    #[test]
    fn batch_kick_rejects_empty_oversized_or_non_integer_lists() {
        assert!(validate_call_params("member.kick_many", &json!({"user_ids": []})).is_err());
        let ids: Vec<u32> = (0..=MAX_BATCH_KICK as u32).collect();
        assert!(validate_call_params("member.kick_many", &json!({"user_ids": ids})).is_err());
        assert!(validate_call_params("member.kick_many", &json!({"user_ids": ["10001"]})).is_err());
        assert!(
            validate_call_params("member.kick_many", &json!({"user_ids": [10001, 10002]})).is_ok()
        );
    }

    #[test]
    fn notice_delete_requires_fid_or_notice_id() {
        assert!(validate_call_params("group.notice.delete", &json!({})).is_err());
        assert!(
            validate_call_params("group.notice.delete", &json!({"fid": " ", "notice_id": ""}))
                .is_err()
        );
        assert!(validate_call_params("group.notice.delete", &json!({"notice_id": "42"})).is_ok());
    }

    #[test]
    fn friend_category_requires_a_target() {
        assert!(validate_call_params("friend.set_category", &json!({"uin": 10000})).is_err());
        assert!(
            validate_call_params(
                "friend.set_category",
                &json!({"uin": 10000, "categoryId": 1})
            )
            .is_ok()
        );
        assert!(
            validate_call_params(
                "friend.set_category",
                &json!({"uin": 10000, "categoryName": "同学"})
            )
            .is_ok()
        );
    }

    #[test]
    fn custom_face_uses_the_current_room_as_target() {
        let route = ROUTES
            .iter()
            .find(|route| route.method == "msg.face")
            .expect("有 msg.face 路由");
        let group = RoomId::new("qq", "group/456").expect("合法");
        let payload = call_payload(route, &group, &json!({"emoji_id": "abc"})).expect("能补群号");
        assert_eq!(payload["group_id"], "456");
        assert!(payload.get("user_id").is_none());

        let private = RoomId::new("qq", "private/10002").expect("合法");
        let payload =
            call_payload(route, &private, &json!({"emoji_id": "abc"})).expect("能补私聊对象");
        assert_eq!(payload["user_id"], "10002");
        assert!(payload.get("group_id").is_none());
    }

    #[test]
    fn custom_face_is_recorded_as_a_message() {
        let record = outgoing_record(
            "msg.face",
            &json!({"emoji_id": "abc"}),
            &json!({"message_id": 77}),
        )
        .expect("发送要记账");
        assert_eq!(record.kind, OutgoingKind::Message);
        assert_eq!(record.platform_id.as_deref(), Some("77"));
        assert_eq!(record.elements, vec![Element::text("[自定义表情]")]);
    }

    /// 批次 6 的媒体/语音 op：显式 risk、确定路由；发送 AI 语音要标成 Send。
    #[test]
    fn batch_six_media_ops_have_explicit_risk_and_routes() {
        let methods = catalog();
        for (name, risk, action, effect) in [
            ("msg.voice_text", Risk::Read, "fetch_ptt_text", Effect::Read),
            ("msg.ocr", Risk::Read, "ocr_image", Effect::Read),
            ("file.info", Risk::Read, "get_file", Effect::Read),
            (
                "group.ai_characters",
                Risk::Read,
                "get_ai_characters",
                Effect::Read,
            ),
            (
                "group.ai_voice",
                Risk::Write,
                "get_ai_record",
                Effect::Write,
            ),
            (
                "group.ai_voice_send",
                Risk::Write,
                "send_group_ai_record",
                Effect::Send,
            ),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert_eq!(method.risk, risk, "{name} 的 risk 标错了");
            assert_eq!(method.effect, effect, "{name} 的 effect 标错了");
            let route = ROUTES
                .iter()
                .find(|route| route.method == name)
                .unwrap_or_else(|| panic!("{name} 没有路由"));
            match route.target {
                Target::Fixed(target) => assert_eq!(target, action, "{name} 路由不对"),
                Target::ByRoom { .. } => panic!("{name} 不该按房间分叉"),
            }
        }

        let send = methods
            .iter()
            .find(|method| method.name.as_ref() == "group.ai_voice_send")
            .expect("有 AI 语音发送");
        assert_eq!(
            send.record,
            RecordMode::Message,
            "AI 语音发送是一条消息，要进流水"
        );
    }

    /// 语音/OCR/AI 语音的前提取自平台文档，不让模型猜。
    #[test]
    fn media_ops_state_their_preconditions() {
        let methods = catalog();
        for (name, keyword) in [
            ("msg.voice_text", "必须是一条语音消息"),
            ("msg.ocr", "识别不了时平台会报错"),
            ("file.info", "至少给一个"),
            ("group.ai_voice", "不能为空"),
            ("group.ai_voice_send", "不能为空"),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert!(
                method.summary.contains(keyword),
                "{name} 的描述没写清前提「{keyword}」：{}",
                method.summary
            );
        }
    }

    #[test]
    fn media_and_ai_identifiers_are_validated() {
        assert!(validate_call_params("msg.voice_text", &json!({"message_id": ""})).is_err());
        assert!(validate_call_params("msg.ocr", &json!({"image": ""})).is_err());
        let long = "a".repeat(MAX_OCR_SOURCE_LEN + 1);
        assert!(validate_call_params("msg.ocr", &json!({"image": long})).is_err());
        assert!(validate_call_params("file.info", &json!({})).is_err());
        assert!(validate_call_params("file.info", &json!({"file_id": "f1"})).is_ok());
        assert!(
            validate_call_params("group.ai_voice", &json!({"character": "c1", "text": " "}))
                .is_err()
        );
        assert!(
            validate_call_params(
                "group.ai_voice_send",
                &json!({"character": "c1", "text": "你好"})
            )
            .is_ok()
        );
    }

    #[test]
    fn ai_voice_send_is_recorded_as_a_message() {
        let record = outgoing_record(
            "group.ai_voice_send",
            &json!({"character": "c1", "text": "你好"}),
            &json!({"message_id": 88}),
        )
        .expect("发送要记账");
        assert_eq!(record.kind, OutgoingKind::Message);
        assert_eq!(record.platform_id.as_deref(), Some("88"));
        assert_eq!(record.elements, vec![Element::text("[AI 语音]")]);
    }

    /// 批次 7 的只读主路径仍在；新一批把相册写操作补进 catalog（风险分档）。
    #[test]
    fn batch_seven_album_ops_are_read_only_and_routed() {
        let methods = catalog();
        for (name, action) in [
            ("group.album.list", "get_group_album_list"),
            ("group.album.media", "get_group_album_media_list"),
            ("group.album.qun_list", "get_qun_album_list"),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert_eq!(method.risk, Risk::Read, "{name} 只能是只读");
            assert_eq!(method.effect, Effect::Read, "{name} 只能是只读");
            let route = ROUTES
                .iter()
                .find(|route| route.method == name)
                .unwrap_or_else(|| panic!("{name} 没有路由"));
            match route.target {
                Target::Fixed(target) => assert_eq!(target, action, "{name} 路由不对"),
                Target::ByRoom { .. } => panic!("{name} 不该按房间分叉"),
            }
        }
        for (name, action, risk) in [
            (
                "group.album.upload_image",
                "upload_image_to_qun_album",
                Risk::Write,
            ),
            (
                "group.album.upload_video",
                "upload_video_to_qun_album",
                Risk::Write,
            ),
            ("group.album.comment", "do_group_album_comment", Risk::Write),
            (
                "group.album.like",
                "set_group_album_media_like",
                Risk::Write,
            ),
            (
                "group.album.unlike",
                "cancel_group_album_media_like",
                Risk::Write,
            ),
            (
                "group.album.delete_media",
                "del_group_album_media",
                Risk::Destructive,
            ),
        ] {
            let method = methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"));
            assert_eq!(method.risk, risk, "{name} 风险档不对");
            let route = ROUTES
                .iter()
                .find(|route| route.method == name)
                .unwrap_or_else(|| panic!("{name} 没有路由"));
            match route.target {
                Target::Fixed(target) => assert_eq!(target, action, "{name} 路由不对"),
                Target::ByRoom { .. } => panic!("{name} 不该按房间分叉"),
            }
        }
    }

    #[test]
    fn media_and_critical_ops_are_routed_with_limits() {
        let methods = catalog();
        let find = |name: &str| {
            methods
                .iter()
                .find(|method| method.name.as_ref() == name)
                .unwrap_or_else(|| panic!("没有 {name}"))
        };
        assert_eq!(find("account.cookies").risk, Risk::Critical);
        assert_eq!(
            find("account.cookies").access_kind(),
            acb_kernel::AccessTargetKind::Account,
            "account op 必须声明账号目标"
        );
        assert_eq!(
            find("file.download").access_kind(),
            acb_kernel::AccessTargetKind::Account,
            "file op 必须声明账号目标"
        );
        assert_eq!(
            find("stream.upload_file").access_kind(),
            acb_kernel::AccessTargetKind::Account
        );
        assert_eq!(
            find("system.send_packet").access_kind(),
            acb_kernel::AccessTargetKind::Connection
        );
        assert_eq!(
            find("msg.send").access_kind(),
            acb_kernel::AccessTargetKind::Room
        );
        assert_eq!(find("file.download").risk, Risk::Destructive);
        assert_eq!(find("stream.download_file").risk, Risk::Read);
        assert_eq!(find("msg.send_private_forward").risk, Risk::Write);
        assert_eq!(find("group.album.delete_media").effect, Effect::Delete);
        for (name, action) in [
            ("account.cookies", "get_cookies"),
            ("account.csrf_token", "get_csrf_token"),
            ("file.download", "download_file"),
            ("stream.upload_file", "upload_file_stream"),
            ("stream.download_image", "download_file_image_stream"),
            ("stream.download_record", "download_file_record_stream"),
            ("stream.download_file", "download_file_stream"),
            ("stream.test_download", "test_download_stream"),
            ("stream.clean", "clean_stream_temp_file"),
            ("msg.forward_upload", "upload_forward_msg"),
            ("msg.forward_upload_alias", "upload_foward_msg"),
            ("msg.send_group_forward", "send_group_forward_msg"),
            ("msg.send_private_forward", "send_private_forward_msg"),
            ("group.album.upload_image", "upload_image_to_qun_album"),
            ("group.album.upload_video", "upload_video_to_qun_album"),
            ("group.album.delete_media", "del_group_album_media"),
            ("group.album.comment", "do_group_album_comment"),
            ("group.album.like", "set_group_album_media_like"),
            ("group.album.unlike", "cancel_group_album_media_like"),
        ] {
            let route = ROUTES
                .iter()
                .find(|route| route.method == name)
                .unwrap_or_else(|| panic!("{name} 没有路由"));
            match route.target {
                Target::Fixed(target) => assert_eq!(target, action, "{name} 路由不对"),
                Target::ByRoom { .. } => panic!("{name} 不该按房间分叉"),
            }
        }

        assert!(
            validate_call_params("file.download", &json!({"url": "ftp://x/a"})).is_err(),
            "file.download 不接受非 http(s) scheme"
        );
        assert!(validate_call_params("file.download", &json!({"url": "https://x/a"})).is_ok());
        assert!(
            validate_call_params(
                "file.download",
                &json!({"base64": "AAAAAAAA", "max_bytes": 1})
            )
            .is_err(),
            "base64 超过 max_bytes 必须在本地拒绝"
        );
        assert!(
            validate_call_params("msg.forward_upload", &json!({"messages": []})).is_err(),
            "空转发载荷拒绝"
        );
        let too_many: Vec<Value> = (0..=MAX_FORWARD_NODES)
            .map(|index| json!({"type": "node", "data": {"content": index}}))
            .collect();
        assert!(
            validate_call_params("msg.forward_upload", &json!({ "messages": too_many })).is_err(),
            "超过节点上限拒绝"
        );
        assert!(
            validate_call_params("stream.upload_file", &json!({"stream_id": "id with space"}))
                .is_err(),
            "stream_id 非法字符拒绝"
        );
        assert!(
            validate_call_params(
                "stream.upload_file",
                &json!({"stream_id": "ok", "file_size": MAX_STREAM_FILE_BYTES + 1})
            )
            .is_err(),
            "流式总大小超限拒绝"
        );
    }

    /// catalog 声明的目标种类必须与 grants 判定的种类一致：同一个 op 不能
    /// catalog 说 Connection、grants 却在比 Room。这条测试把对应关系钉死。
    #[test]
    fn catalog_target_kinds_match_grant_kinds() {
        use acb_kernel::AccessTargetKind;

        let methods = catalog();
        for spec in methods.iter() {
            let room_targets = spec
                .targets
                .iter()
                .filter(|target| target.role == TargetRole::Room)
                .count();
            let member_targets = spec
                .targets
                .iter()
                .filter(|target| target.role == TargetRole::Member)
                .count();
            let account_namespace = ["account.", "file.", "stream."]
                .iter()
                .any(|prefix| spec.name.starts_with(prefix));

            match spec.scope {
                Scope::Room => {
                    assert_eq!(
                        spec.access_kind(),
                        AccessTargetKind::Room,
                        "{} 是房间 op，grants 必须比 room 目标",
                        spec.name
                    );
                }
                Scope::Connection => {
                    assert_eq!(
                        room_targets, 0,
                        "{} 是连接 op，catalog 不该声明 room 目标参数",
                        spec.name
                    );
                    assert_eq!(
                        spec.access_kind(),
                        if account_namespace {
                            AccessTargetKind::Account
                        } else {
                            AccessTargetKind::Connection
                        },
                        "{} 是连接 op，grants 不能拿房间目标去比",
                        spec.name
                    );
                }
            }
            if room_targets > 0 || member_targets > 0 {
                assert_eq!(
                    spec.scope,
                    Scope::Room,
                    "{} 声明了房间/成员目标，必须同时声明 room scope",
                    spec.name
                );
            }
        }
    }

    /// 批次 8 的参数归一化：平台细节不能漏给模型，也不能让模型猜默认值。
    #[test]
    fn batch8_params_are_normalized_before_the_platform() {
        let mut add = json!({"add_type": 55, "question": "Q", "answer": "A"});
        normalize_call_params("group.add_option", &mut add).expect("55 是合法别名");
        assert_eq!(add["add_type"], 5, "55 必须归一化为 5");
        assert!(add.get("answer").is_none(), "5 不写 answer");
        assert!(add.get("question").is_some());

        assert!(
            normalize_call_params("group.add_option", &mut json!({"add_type": 6})).is_err(),
            "0-5/55 之外拒绝"
        );
        assert!(
            normalize_call_params(
                "group.add_option",
                &mut json!({"add_type": 4, "question": "Q"})
            )
            .is_err(),
            "add_type=4 要求 question+answer"
        );

        let mut search = json!({"name_search": "close", "code_search": "open"});
        normalize_call_params("group.search", &mut search).expect("open/close 合法");
        assert_eq!(search["no_finger_open"], 1);
        assert_eq!(search["no_code_finger_open"], 0);
        assert!(search.get("name_search").is_none(), "友好字段不发给平台");
        assert!(
            normalize_call_params("group.search", &mut json!({})).is_err(),
            "两个开关都不给要本地拒绝"
        );

        let mut rename = json!({"folder_id": "f1", "name": "B"});
        normalize_call_params("group.file.folder_rename", &mut rename).expect("name 是别名");
        assert_eq!(rename["new_folder_name"], "B");
        assert!(rename.get("name").is_none(), "别名归一化后要移除");

        let mut rename = json!({"folder_id": "f1", "name": "B", "new_folder_name": "A"});
        normalize_call_params("group.file.folder_rename", &mut rename).expect("两个都给");
        assert_eq!(rename["new_folder_name"], "A", "new_folder_name 优先");
        assert!(
            normalize_call_params("group.file.folder_rename", &mut json!({"folder_id": "f1"}))
                .is_err(),
            "两个名称都不给要拒绝"
        );

        assert!(
            normalize_call_params("group.tag_list", &mut json!({"group_class": -1})).is_err(),
            "group_class 不能是负数"
        );
        assert!(normalize_call_params("group.tag_list", &mut json!({"group_class": 7})).is_ok());
        assert!(validate_call_params("file.share_link", &json!({"fileset_id": " "})).is_err());
        assert!(validate_call_params("group.file.trans", &json!({"file_id": ""})).is_err());

        assert!(
            normalize_call_params("face.custom_add", &mut json!({"file": "ftp://x/a.png"}))
                .is_err(),
            "未知 scheme 拒绝"
        );
        for source in [
            "/tmp/a.png",
            "file:///tmp/a.png",
            "https://x/a.png",
            "base64://AAAA",
            "data:image/png;base64,AAAA",
        ] {
            assert!(
                normalize_call_params("face.custom_add", &mut json!({"file": source})).is_ok(),
                "{source} 应被接受"
            );
        }

        assert!(
            validate_call_params(
                "file.download",
                &json!({"base64": "AAAA", "max_bytes": MAX_DOWNLOAD_BYTES + 1})
            )
            .is_err(),
            "超过平台 1 GiB 上限要拒绝"
        );
    }

    /// unverified 的 op 清单是安全边界：多标或少标都会让门槛漂移。
    #[test]
    fn catalog_lists_the_unverified_ops() {
        let methods = catalog();
        let mut unverified: Vec<&str> = methods
            .iter()
            .filter(|method| method.verification.is_unverified())
            .map(|method| method.name.as_ref())
            .collect();
        unverified.sort_unstable();
        assert_eq!(
            unverified,
            [
                "account.model_show",
                "account.model_show_set",
                "account.qzone.detail",
                "account.qzone.unlike",
                "file.collection_create",
                "friend.recent_contacts",
                "group.add_option",
                "group.anonymous",
                "group.anonymous_ban",
                "group.tag_add",
                "group.tag_delete",
                "group.tag_list",
                "msg.tuwen_ark",
                "msg.word_slices",
                "system.packet_status",
            ],
            "只有 research §3 里需要实验的 op 才能标 unverified"
        );
        for method in methods.iter() {
            if method.verification.is_unverified() {
                assert!(
                    method.verification.reason().is_some(),
                    "{} 缺少 reason",
                    method.name
                );
                assert!(
                    method.verification.experiment().is_some(),
                    "{} 缺少 experiment",
                    method.name
                );
            } else {
                assert!(
                    method.verification.source().is_some(),
                    "{} 缺少 source",
                    method.name
                );
            }
        }
    }

    #[test]
    fn album_media_requires_an_album_id() {
        assert!(validate_call_params("group.album.media", &json!({"album_id": " "})).is_err());
        assert!(
            validate_call_params(
                "group.album.media",
                &json!({"album_id": "a1", "attach_info": ""})
            )
            .is_ok()
        );
    }
}
