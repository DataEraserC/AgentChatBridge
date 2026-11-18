//! 网关 dispatch 事件 → [`acb_kernel::RecordDraft`]。
//!
//! 范围（与「官方机器人功能有限」对齐）：群 @ 消息、好友单聊消息是正文；其它
//! dispatch 类型记一条 suppressed 事件（不投递、不唤醒），流水仍有迹可循。
//! 频道（DIRECT_MESSAGE_CREATE）后续要接时在这里加分支。

use serde_json::Value;

use acb_kernel::{Element, RecordDraft, RecordKind, RoomId, RoomScope, Sender};

use crate::outbound::ReplyState;

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("missing field: {0}")]
    Missing(String),
    #[error("bad room: {0}")]
    BadRoom(String),
}

/// 解析结果：要么一条记录，要么「这类型不产生记录」。
#[derive(Debug)]
pub enum Parsed {
    Record(RecordDraft),
    /// 一条记录 + 随它一起要下载的附件。
    ///
    /// 记录里已经放好**占位元素**（不带 URL），附件下载完成后由
    /// `patch_element` 把占位换成本地路径。分开返回而不是塞进 draft：
    /// draft 是要入库的，下载是入库之后的 IO，两者的生命周期不该绑在一起。
    WithAttachments {
        draft: RecordDraft,
        attachments: Vec<acb_adapter_attachment::PendingAttachment>,
    },
    /// 心跳/确认类：直接丢弃。
    Ignored,
}

/// 网关 dispatch（`op == 0`）的入口。`event_type` 是 `t`，`payload` 是 `d`。
/// 这条消息算不算「冲机器人来的」（`at_self`）。
///
/// 为什么不是 bool：`GROUP_AT_MESSAGE_CREATE` 天然是 @ 版本，恒为真；而
/// `GROUP_MESSAGE_CREATE`（全量模式）每条群消息都会来，必须看 `mentions`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AtSelf {
    /// 事件本身保证是冲机器人来的（@ 版本、私聊）。
    Always,
    /// 看 `mentions[].is_you`（全量模式）。
    FromMentions,
}

pub fn parse(
    connection: &str,
    event_type: &str,
    payload: &Value,
    reply_state: &ReplyState,
    at: i64,
) -> Result<Parsed, EventError> {
    match event_type {
        "GROUP_AT_MESSAGE_CREATE" => message(
            connection,
            payload,
            reply_state,
            at,
            "group",
            "group_openid",
            AtSelf::Always,
        ),
        // 群消息**全量模式**：机器人在群设置里开了「接收全部消息」后，平台改推
        // 这个事件（不再推 `GROUP_AT_MESSAGE_CREATE`）。字段与 @ 版本完全一致
        // （官方文档原文：「各字段含义与 GROUP_AT_MESSAGE_CREATE 完全一致」）。
        //
        // 2026-11-18 实测：用户打开全量后 @ 机器人不再有回复——事件掉进了兜底
        // `notice()`，房间被写成 `group/GROUP_MESSAGE_CREATE`（事件名，不是 openid），
        // 于是「没有订阅这个房间」被丢弃。
        //
        // `at_self` 不能写死 true：全量模式下每条群消息都会来，只有 `mentions`
        // 里 `is_you = true` 的那条才是冲机器人来的。写死会让 `at_mention`
        // 唤醒策略失效（每条消息都当 @）。
        "GROUP_MESSAGE_CREATE" => message(
            connection,
            payload,
            reply_state,
            at,
            "group",
            "group_openid",
            AtSelf::FromMentions,
        ),
        "C2C_MESSAGE_CREATE" => message(
            connection,
            payload,
            reply_state,
            at,
            "private",
            "user_openid",
            // 私聊消息天然「冲我来」：唤醒策略的 at_mention 才能在单聊里工作。
            AtSelf::Always,
        ),
        // 机器人被加入/移出群聊。
        //
        // **这两个事件是「房间发现」的关键**：官方没有「列出机器人所在群」的
        // 接口，但入群那一刻平台会推 `group_openid`。把它写成一条**非 suppressed**
        // 的记录落到 `qqbot-official:group/<openid>` 房间，`records` 表就有了这个
        // 房间——控制台的 `seen` 候选立刻能看到它，不用等用户在群里 @ 机器人。
        //
        // 两个事件都在 intents `1<<25`（`GROUP_AND_C2C_EVENT`）里，现有连接已经
        // 订阅了那一位，所以不需要改 Identify。
        "GROUP_ADD_ROBOT" | "GROUP_DEL_ROBOT" => {
            membership(connection, event_type, payload, at, "get_status/group")
        }
        // 用户添加/删除机器人好友（单聊关系的建立与解除）。
        //
        // 与入群同理：写进 `private/<openid>` 房间，让控制台能看到这个私聊对象存在，
        // 而不是必须等对方先发消息。
        "FRIEND_ADD" | "FRIEND_DEL" => {
            membership(connection, event_type, payload, at, "get_status/private")
        }
        // FRIEND_VERIFY：有人申请加好友。记一条**非 suppressed** 事件——
        // 审批动作（官方接口形态待文档核实）未实现前，至少让流水与会话
        // 看得见申请的存在。其余类型（interaction、audit…）仍记 suppressed。
        "FRIEND_VERIFY" => {
            let openid = text_of(payload.get("openid"))
                .or_else(|| {
                    payload
                        .get("author")
                        .and_then(|value| text_of(value.get("openid")))
                })
                .unwrap_or_default();
            let summary = format!("好友验证申请（openid={openid}）——审批接口待接入");
            let mut record = notice(connection, event_type, payload, at)?;
            record.suppressed = false;
            record.elements = vec![Element::Text { text: summary }];
            Ok(Parsed::Record(record))
        }
        // 其余类型（interaction、audit…）记 suppressed 事件流水。
        _ => Ok(Parsed::Record(notice(connection, event_type, payload, at)?)),
    }
}

fn message(
    connection: &str,
    payload: &Value,
    reply_state: &ReplyState,
    at: i64,
    scope: &str,
    openid_field: &'static str,
    at_self: AtSelf,
) -> Result<Parsed, EventError> {
    // OpenID 的位置文档内外的形态都有：GROUP 在顶层（group_openid），C2C 在
    // author.user_openid / author.id。三种都试，读不到就把真实键名留在错误里
    // （2026-11-15 实测：C2C 载荷顶层没有 user_openid，一直 Missing 被丢弃）。
    let author = payload.get("author");
    let openid = text_of(payload.get(openid_field))
        .or_else(|| author.and_then(|value| text_of(value.get(openid_field))))
        .or_else(|| author.and_then(|value| text_of(value.get("id"))))
        .filter(|openid| !openid.is_empty())
        .ok_or_else(|| {
            let mut keys: Vec<String> = payload
                .as_object()
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            if let Some(map) = author.and_then(Value::as_object) {
                keys.extend(map.keys().map(|key| format!("author.{key}")));
            }
            EventError::Missing(format!("{openid_field} (payload keys: {keys:?})"))
        })?;
    let room = RoomId::new(connection, format!("{scope}/{openid}"))
        .map_err(|error| EventError::BadRoom(error.to_string()))?;
    let content = text_of(payload.get("content")).unwrap_or_default();
    let msg_id = text_of(payload.get("id")).unwrap_or_default();
    // 被动回复的窗口按 msg_id 计：收到的每条消息都记下，出站才有的回复。
    if !msg_id.is_empty() {
        reply_state.observe(&room, &msg_id);
    }
    // author 是对象时 id 即发送者 openid；是字符串时直接当发送者。
    let sender_id = author
        .and_then(|value| text_of(value.get("id")))
        .unwrap_or_else(|| openid.clone());

    let at_self = match at_self {
        AtSelf::Always => true,
        AtSelf::FromMentions => payload
            .get("mentions")
            .and_then(Value::as_array)
            // `is_you` 是官方给的「是否是@的当前bot」；`<@all>`（@全体）也算
            // 冲机器人来的一次提醒，但**不算** @ 到它——两者在唤醒策略里不同。
            .is_some_and(|list| {
                list.iter()
                    .any(|item| item.get("is_you").and_then(Value::as_bool).unwrap_or(false))
            }),
    };
    // 附件：正文里只留文本，媒体各自建成一个**占位元素**（不含 URL）。
    //
    // 为什么不把 URL 写进占位：平台 CDN 地址带 `rkey` 预签名，单条 300~400
    // 字符，每条记录都塞一遍就是拿签名串喂上下文（反馈 #12）。占位只说
    // 「有什么东西、正在下载」；下载完成后由 `patch_element` 换成
    // `state_dir/attachments/...` 下的本地路径。
    let mut elements = vec![Element::Text {
        text: content.clone(),
    }];
    let mut attachments = Vec::new();
    if let Some(list) = payload.get("attachments").and_then(Value::as_array) {
        for item in list {
            let content_type = text_of(item.get("content_type")).unwrap_or_default();
            let filename = text_of(item.get("filename")).unwrap_or_default();
            let url = text_of(item.get("url")).unwrap_or_default();
            // 没有 URL 的附件（平台只给了元信息）没法下载，只留一句占位。
            let index = elements.len();
            elements.push(Element::Text {
                text: acb_adapter_attachment::placeholder(&content_type, &filename),
            });
            if url.is_empty() || msg_id.is_empty() {
                continue;
            }
            attachments.push(acb_adapter_attachment::PendingAttachment {
                room: room.to_string(),
                platform_id: msg_id.clone(),
                index,
                url,
                filename,
                content_type,
            });
        }
    }
    let draft = RecordDraft::message(
        room.clone(),
        Sender {
            id: sender_id.clone(),
            name: sender_id,
            role: None,
        },
        at,
        elements,
        at_self,
    )
    .with_room_scope(if scope == "private" {
        RoomScope::Direct
    } else {
        RoomScope::MultiParty
    })
    .with_platform_id(msg_id.clone());
    if attachments.is_empty() {
        return Ok(Parsed::Record(draft));
    }
    Ok(Parsed::WithAttachments { draft, attachments })
}

/// 群/好友关系变动（入群、退群、加好友、删好友）→ 一条落在**真实房间**里的记录。
///
/// 与 [`notice`] 的区别：`notice` 把房间写成 `group/<事件名>`（那只是个兜底的
/// 归档位，不对应任何真实房间）；这里必须落到 `group/<openid>` / `private/<openid>`，
/// 因为控制台的房间候选就是按真实房间 id 查 `records` 的（ADR-0035）。
fn membership(
    connection: &str,
    event_type: &str,
    payload: &Value,
    at: i64,
    kind_hint: &str,
) -> Result<Parsed, EventError> {
    // 群的字段是 `group_openid`；好友的字段是 `openid`（也可能落在 author 里）。
    let openid = text_of(payload.get("group_openid"))
        .or_else(|| text_of(payload.get("openid")))
        .or_else(|| {
            payload
                .get("author")
                .and_then(|value| text_of(value.get("openid")))
        })
        .filter(|openid| !openid.is_empty())
        .ok_or_else(|| {
            let keys: Vec<String> = payload
                .as_object()
                .map(|map| map.keys().cloned().collect())
                .unwrap_or_default();
            EventError::Missing(format!("openid (payload keys: {keys:?})"))
        })?;
    let scope = if event_type.starts_with("GROUP_") {
        "group"
    } else {
        "private"
    };
    let room = RoomId::new(connection, format!("{scope}/{openid}"))
        .map_err(|error| EventError::BadRoom(error.to_string()))?;
    // 摘要给人和 agent 看：入群/退群/加好友/删好友，一眼知道发生了什么。
    let summary = match event_type {
        "GROUP_ADD_ROBOT" => "机器人被加入群聊".to_owned(),
        "GROUP_DEL_ROBOT" => "机器人被移出群聊".to_owned(),
        "FRIEND_ADD" => "用户添加机器人为好友".to_owned(),
        "FRIEND_DEL" => "用户删除机器人好友".to_owned(),
        other => other.to_owned(),
    };
    Ok(Parsed::Record(RecordDraft {
        room,
        kind: RecordKind::Event {
            event: format!("qq.{event_type}"),
            tags: vec!["notice".to_owned(), "membership".to_owned()],
            payload: Some(payload.clone()),
            summary: Some(summary.clone()),
            body: false,
        },
        sender: Sender {
            id: "qq".to_owned(),
            name: "QQ 官方".to_owned(),
            role: None,
        },
        at,
        elements: vec![Element::Text { text: summary }],
        at_self: false,
        platform_id: None,
        reply_to_self: false,
        target_platform_id: None,
        target_labels: Vec::new(),
        // 非 suppressed：这条记录要落库，房间才会出现在 `records` 里。
        // `suppressed` 的语义是「只给流水看、不进投递」，那会让房间发现失效。
        suppressed: false,
        room_scope: Some(RoomScope::MultiParty),
        room_labels: Vec::new(),
    }))
}

fn notice(
    connection: &str,
    event_type: &str,
    payload: &Value,
    at: i64,
) -> Result<RecordDraft, EventError> {
    let room = RoomId::new(connection, format!("group/{event_type}"))
        .map_err(|error| EventError::BadRoom(error.to_string()))?;
    Ok(RecordDraft {
        room,
        kind: RecordKind::Event {
            event: format!("qq.{event_type}"),
            tags: vec!["notice".to_owned()],
            payload: Some(payload.clone()),
            summary: None,
            body: false,
        },
        sender: Sender {
            id: "qq".to_owned(),
            name: "QQ 官方".to_owned(),
            role: None,
        },
        at,
        elements: Vec::new(),
        at_self: false,
        platform_id: None,
        reply_to_self: false,
        target_platform_id: None,
        target_labels: Vec::new(),
        suppressed: true,
        room_scope: Some(RoomScope::MultiParty),
        room_labels: Vec::new(),
    })
}

fn text_of(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        // author 是对象：openid 字段名按事件类型不同（user_openid）。
        Value::Object(map) => map
            .get("user_openid")
            .and_then(|inner| inner.as_str())
            .map(str::to_owned),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_c2c(payload: Value) -> Result<Parsed, EventError> {
        let reply = ReplyState::new();
        parse("qqbot-official", "C2C_MESSAGE_CREATE", &payload, &reply, 1)
    }

    fn parse_group(payload: Value) -> Result<Parsed, EventError> {
        let reply = ReplyState::new();
        parse(
            "qqbot-official",
            "GROUP_AT_MESSAGE_CREATE",
            &payload,
            &reply,
            1,
        )
    }

    #[test]
    fn c2c_openid_nested_in_author_is_found() {
        // 线上实测形态：顶层没有 user_openid，它在 author 里。
        let parsed = parse_c2c(json!({
            "author": {"id": "author-id", "user_openid": "OPENID_1"},
            "content": "在吗",
            "id": "msg-1"
        }))
        .expect("author 形态应能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("应产生记录");
        };
        assert_eq!(
            draft.room.to_string(),
            "qqbot-official:private/OPENID_1",
            "房间串拿去配订阅"
        );
        assert_eq!(draft.sender.id, "author-id");
        assert!(draft.at_self, "私聊消息天然冲我来（at_mention 唤醒依赖它）");
        assert_eq!(draft.platform_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn c2c_openid_top_level_still_works() {
        let parsed = parse_c2c(json!({
            "user_openid": "OPENID_2",
            "content": "hi",
            "id": "msg-2"
        }))
        .expect("顶层形态应能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("应产生记录");
        };
        assert_eq!(draft.room.to_string(), "qqbot-official:private/OPENID_2");
    }

    #[test]
    fn group_openid_stays_top_level() {
        let parsed = parse_group(json!({
            "group_openid": "GROUP_OPENID",
            "author": {"id": "member-openid"},
            "content": "@bot 你好",
            "id": "msg-3"
        }))
        .expect("群形态应能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("应产生记录");
        };
        assert_eq!(draft.room.to_string(), "qqbot-official:group/GROUP_OPENID");
        assert_eq!(draft.sender.id, "member-openid");
        assert!(draft.at_self, "群消息标 at_self");
    }

    #[test]
    fn c2c_attachments_become_placeholder_elements() {
        let parsed = parse_c2c(json!({
            "author": {"id": "author-id", "user_openid": "OPENID_9"},
            "content": "看看这个",
            "id": "msg-9",
            "attachments": [
                {"content_type": "video", "filename": "demo.mp4", "url": "https://cdn/x?rkey=sig"},
                {"content_type": "image", "filename": "a.png", "url": "https://cdn/y?rkey=sig"}
            ]
        }))
        .expect("带附件应能解析");
        let Parsed::WithAttachments { draft, attachments } = parsed else {
            panic!("带附件时应返回 WithAttachments");
        };

        // 正文只有用户说的话；附件各自是一个占位元素。
        assert!(matches!(&draft.elements[0], Element::Text { text } if text == "看看这个"));
        assert_eq!(draft.elements.len(), 3, "正文 + 两个附件占位");

        // **占位里不能有 URL**：签名串一个字符都不该进记录（反馈 #12）。
        let all = acb_proto::element::flatten_text(&draft.elements);
        assert!(all.contains("video") && all.contains("demo.mp4"), "{all}");
        assert!(
            !all.contains("rkey") && !all.contains("https://"),
            "占位不能带 CDN 地址，实际：{all}"
        );

        // 待下载列表记下了 URL 与元素下标，下载完成后按它改那条记录。
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[0].index, 1, "第一个附件在正文之后");
        assert_eq!(attachments[1].index, 2);
        assert_eq!(attachments[0].platform_id, "msg-9");
        assert!(attachments[0].url.contains("rkey=sig"));
        assert!(attachments[1].content_type == "image");
    }

    /// 没有 URL 的附件无法下载：只留占位，不产生待下载项（不该白起线程）。
    #[test]
    fn attachment_without_url_is_only_a_placeholder() {
        let parsed = parse_c2c(json!({
            "author": {"id": "a", "user_openid": "O"},
            "content": "看图",
            "id": "msg-1",
            "attachments": [{"content_type": "image", "filename": "a.png"}]
        }))
        .expect("能解析");
        let Parsed::Record(draft) = parsed else {
            panic!("没有可下载的附件时走普通 Record");
        };
        assert_eq!(draft.elements.len(), 2, "正文 + 一个占位");
    }

    #[test]
    fn missing_openid_error_lists_payload_keys() {
        let error = parse_c2c(json!({"content": "没有 openid 的怪载荷"}))
            .expect_err("读不到 openid 必须报错");
        let message = error.to_string();
        assert!(message.contains("user_openid"), "{message}");
        assert!(message.contains("payload keys"), "{message}");
        assert!(message.contains("content"), "{message}");
    }
    /// 入群事件必须落到**真实群房间**，而且不能是 suppressed。
    ///
    /// 这是「拉机器人进群就能看到房间」的实现基础（ADR-0035）：官方没有群列表
    /// 接口，但入群那一刻会推 `group_openid`。记录落到 `group/<openid>` 且不
    /// suppressed，`records` 表才有这个房间，控制台的 `seen` 候选才看得到。
    #[test]
    fn group_add_robot_lands_in_the_real_room() {
        let payload = json!({
            "group_openid": "GRP_OPENID",
            "op_member_openid": "OP_MEMBER",
            "timestamp": 1784570534
        });
        let parsed = parse(
            "qqbot-official",
            "GROUP_ADD_ROBOT",
            &payload,
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("入群应当产生一条记录");
        };
        assert_eq!(
            record.room.to_string(),
            "qqbot-official:group/GRP_OPENID",
            "必须落到真实群房间，不是 group/GROUP_ADD_ROBOT 这种兜底位"
        );
        assert!(
            !record.suppressed,
            "suppressed 的记录不进 records 表，房间发现会失效"
        );
        assert!(
            record.room_labels.is_empty() && record.room_scope.is_some(),
            "房间元数据齐全"
        );
    }

    /// 好友事件落到 `private/<openid>`。
    #[test]
    fn friend_add_lands_in_the_private_room() {
        let payload = json!({
            "openid": "USER_OPENID",
            "timestamp": 1784570523,
            "scene": 1001,
            "scene_param": "",
            "author": { "union_openid": "UNION" }
        });
        let parsed = parse(
            "qqbot-official",
            "FRIEND_ADD",
            &payload,
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("加好友应当产生一条记录");
        };
        assert_eq!(
            record.room.to_string(),
            "qqbot-official:private/USER_OPENID"
        );
        assert!(!record.suppressed);
    }

    /// 移出群 / 删好友也要落记录（房间可能还要被清理，但事实要留痕）。
    #[test]
    fn removal_events_also_record() {
        let payload = json!({ "group_openid": "G1", "timestamp": 1 });
        let parsed = parse(
            "qqbot-official",
            "GROUP_DEL_ROBOT",
            &payload,
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("退群应当产生一条记录");
        };
        assert_eq!(record.room.to_string(), "qqbot-official:group/G1");
    }
    /// 全量模式（`GROUP_MESSAGE_CREATE`）必须解析成**真实群房间**。
    ///
    /// 2026-11-18 实测事故：用户在群设置里打开「接收全部消息」后，平台改推
    /// `GROUP_MESSAGE_CREATE`，适配器没有这个分支 → 掉进 `notice()` 兜底 →
    /// 房间被写成 `group/GROUP_MESSAGE_CREATE` → 被当「未订阅房间」丢弃，
    /// **连 @ 机器人也不回**（全量模式下平台不再推 @ 版本）。
    #[test]
    fn full_group_message_lands_in_the_real_room() {
        let payload = json!({
            "id": "ROBOT1.0_xxx",
            "author": {
                "id": "AUTHOR_ID",
                "member_openid": "MEMBER_ID",
                "username": "小明",
                "bot": false
            },
            "content": "大家早上好呀",
            "group_openid": "B2C3D4E5F6",
            "message_type": 0,
            "timestamp": "2026-07-21T08:00:00+08:00"
        });
        let parsed = parse(
            "qqbot-official",
            "GROUP_MESSAGE_CREATE",
            &payload,
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("应当产生一条消息记录");
        };
        assert_eq!(
            record.room.to_string(),
            "qqbot-official:group/B2C3D4E5F6",
            "必须落到真实群房间，不是 group/GROUP_MESSAGE_CREATE 这种兜底位"
        );
    }

    /// 全量模式下 `at_self` 要看 `mentions[].is_you`，不能恒为真。
    ///
    /// 恒为真的后果：`at_mention` 唤醒策略失效——群里每一句闲聊都会被当成
    /// 「有人在叫机器人」并叫醒 agent。
    #[test]
    fn full_group_message_at_self_follows_mentions() {
        let base = |mentions: Value| {
            json!({
                "id": "ROBOT1.0_xxx",
                "author": { "id": "AUTHOR_ID", "username": "小明", "bot": false },
                "content": "内容",
                "group_openid": "G1",
                "message_type": 0,
                "mentions": mentions
            })
        };

        // 没 @ 我 → 不算冲我来
        let parsed = parse(
            "qqbot-official",
            "GROUP_MESSAGE_CREATE",
            &base(json!([])),
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("记录");
        };
        assert!(!record.at_self, "没 @ 我的群消息不该标 at_self");

        // @ 了别人 → 还是不算
        let parsed = parse(
            "qqbot-official",
            "GROUP_MESSAGE_CREATE",
            &base(json!([{ "id": "OTHER", "is_you": false }])),
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("记录");
        };
        assert!(!record.at_self, "@ 别人不算冲我来");

        // @ 了我 → 算
        let parsed = parse(
            "qqbot-official",
            "GROUP_MESSAGE_CREATE",
            &base(json!([{ "id": "ME", "is_you": true }])),
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("记录");
        };
        assert!(record.at_self, "@ 我必须标 at_self，否则唤醒策略收不到");
    }

    /// `GROUP_AT_MESSAGE_CREATE` 仍然恒为 at_self（@ 版本本身就是冲机器人来的）。
    #[test]
    fn at_mention_event_is_always_at_self() {
        let payload = json!({
            "id": "ROBOT1.0_xxx",
            "author": { "id": "A1", "username": "小明", "bot": false },
            "content": "@机器人 你好",
            "group_openid": "G2",
            "message_type": 0
        });
        let parsed = parse(
            "qqbot-official",
            "GROUP_AT_MESSAGE_CREATE",
            &payload,
            &ReplyState::new(),
            1,
        )
        .expect("能解析");
        let Parsed::Record(record) = parsed else {
            panic!("记录");
        };
        assert!(record.at_self);
    }
}
