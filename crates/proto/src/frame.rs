//! 协议帧。每帧一行 JSON，`frame` 字段是帧名。
//!
//! 帧集合是封闭的：加帧要改这里并补测试。加帧属于兼容演进（旧的一端会因为不认识的
//! 帧名而报错，所以升级要两端一起），改已有帧的语义则不是。

use serde::{Deserialize, Serialize};

use crate::error::{DecodeError, ProtoError};
use crate::record::Record;
use crate::room::RoomId;
use crate::ticket::Ticket;

/// 协议版本。握手时两端必须一致。
///
/// 2：`ready` 从「绑一台设备」改成「绑一组设备」——一把接入令牌可以代表同一份记忆的
/// 多台设备，`send_message_to` 因此才能跨设备发。这是改语义，不是加帧，所以要升版本。
/// 3：device 概念升级为「harness 会话」——`hello` 加 `harness` 字段，`delivery` 的
/// `device`/`memory` 合并成 `session`，串行键改成「harness 名 + 会话 id」（ADR-0017）。
/// 4：加控制面——`control`（桥→harness 发指令）和 `control_result`（harness 回报），
/// 供桥侧统一控制台触达 harness 内部（ADR-0018）。
/// 5：`delivery` 增加可选 `wake_reason`，让 agent 知道「这次为什么醒」。
/// 6：事件记录带 `tags` / `payload` / `summary`——平台事件不再只有一个类型名，
///    agent 和唤醒策略都能看到跨平台语义标签（`RecordKind::Event` 扩字段）。
/// 7：`turn_end` 增加可选结构化 `failure`，harness 可以标注失败属于模型还是后端，
///    以及 `model_unavailable` / `model_unconfigured` / `auth_failed` 等分类。
///    字段可选，v6 harness 不带 `failure` 时走原有文本去重路径。
/// 8：`delivery` 的投递格式从「按条数截断正文」改为「触发前后 N 条正文 + 完整未读
///    编号区间」——加 `trigger_id` / `unread_ranges` / `unread_count` /
///    `unread_redelivered` / `max_times`。`more` / `omitted` 保留，半升级时仍能用。
pub const PROTOCOL_VERSION: u32 = 8;

/// 单帧上限：1 MiB。超了直接断连，不做截断。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 一次投递：开一轮时发给 harness 的东西。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// 本轮的票。
    pub ticket: Ticket,
    /// 本轮替哪个会话说话。
    pub session: String,
    /// 本轮只涉及这一个房间。
    pub room: RoomId,
    /// 要投递的记录，可能为空（时钟策略的例行检查）。
    pub records: Vec<Record>,
    /// 为什么开这一轮。
    pub reason: TurnReason,
    /// wake 策略给出的命中原因（如「@ 了我」「攒够 5 条未读」）。
    /// 只给 agent 看，不参与调度；时钟轮次通常为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_reason: Option<String>,
    /// 这个会话引用的记忆：每份的 id + worktree 路径。harness 据此让 agent 操作记忆
    /// （文件 + git），`memory_*` 工具用记忆 id 指认要碰哪一份。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<MemoryInfo>,
    /// 这个会话的身份信息（名称、归哪个 harness、用哪个模型）。harness 据此知道
    /// 「该以什么身份说话、用哪个模型」。可空是为了兼容旧协议帧。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_info: Option<SessionInfo>,
    /// 这个会话的人格卡正文。桥只给语义，怎么塞进模型上下文由 harness 适配器决定
    /// （system prompt、首轮 context 消息、或每轮开头的一段说明都可以）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<PersonaInfo>,
    /// 还有多少条没投进来（桥按 `delivery_limit` 截断了）。`None` = 全投了，没有截断。
    /// 默认策略从旧到新连续投，这部分会在后续轮次接着投；harness 提示模型时
    /// 应以「按顺序等下一轮」为准，不要假设未投的都在最新一端。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub more: Option<usize>,
    /// 被 composer 主动跳过的中段：编号区间 + 条数。默认连续投递不会设置；
    /// 自定义 composer 跳段时才用，和 `more` 一起给，让模型能直接按编号回看。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<Omitted>,
    /// 触发这一轮的那条记录。时钟轮次、攒批唤醒、没有具体触发记录时为空。
    /// 模型据此知道「这一轮是围着哪条消息展开的」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<u64>,
    /// 本轮**完整**的未读编号区间，闭区间，按编号升序，例如 `[[1,20],[30,40]]`。
    /// 不截断、不抽样：正文没投的编号也在里面，模型可以按需回看再确认。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread_ranges: Vec<[u64; 2]>,
    /// 上面这些区间覆盖的未读总条数（编号有空洞，不等于区间长度之和）。
    #[serde(default)]
    pub unread_count: usize,
    /// 未读里已经被投过 ≥2 次的条数。用来提示「这些反复出现，该确认或说明不处理」。
    #[serde(default)]
    pub unread_redelivered: usize,
    /// 未读里被投过的最高次数。
    #[serde(default)]
    pub max_times: u32,
    /// 这一轮最多跑多久（毫秒）。`None` = 桥没带（harness 用自己的默认兜底）；
    /// `Some(0)` = 桥明确关闭了超时，harness 别自己掐——等到 turn/end 为止；
    /// `Some(n)` = 超过 n 毫秒还没结束就**主动打断**本侧会话（能打断的打断，
    /// 打断不了的按各自兜底处理），打断算这轮失败，走桥的失败通知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_timeout_ms: Option<u64>,
    /// 插件贡献的上下文片段。core 已按预算裁剪并排好序；空 = 没有注入。
    /// 旧 harness 不认识这个字段时直接忽略，行为不变。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prompts: Vec<PromptFragment>,
}

/// 一段插件贡献的上下文。片段与记录/人格分开，不能改写对话历史。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptFragment {
    /// 来源 provider id，由 core 填写。
    pub source: String,
    /// 正文；core 已保证不超过单段上限。
    pub text: String,
    /// 数值越大越靠前；同优先级按 source 字典序。
    pub priority: i32,
    /// 是否被 core 从更长文本裁剪过。
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 本轮被 composer 主动跳过的未读区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Omitted {
    /// 区间里最早的编号。
    pub from: u64,
    /// 区间里最晚的编号。
    pub to: u64,
    /// 区间里实际有多少条未读（编号可能有空洞，所以不能用 `to - from + 1`）。
    pub count: usize,
}

/// 一个会话在本轮投递里的人格卡。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersonaInfo {
    pub id: String,
    pub name: String,
    /// 人格正文：桥原样转交，不认识里面的内容。
    pub text: String,
}

/// 一个会话在本轮投递里的身份信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// 会话 id（与 `Delivery.session` 一致）。
    pub id: String,
    /// 会话显示名（缺省 = id）。
    pub name: String,
    /// 归哪个 harness 接管。
    pub harness: String,
    /// 用哪个模型；没配就是 `None`，由 harness 侧决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 主模型被拒（内容审核 / 限流 / 模型不可用）时按顺序改用的后备模型，
    /// 每项是 `provider/model`。空 = 不重试。
    ///
    /// 为什么不升协议版本：这是**可选字段**，老驱动不认识就忽略（serde 默认丢弃
    /// 未知字段），行为退化成「不重试」，不会解析失败。链本身由桥持有（全局表
    /// `model_fallbacks`，或会话表 `session_model_fallbacks`），驱动只照单办事，
    /// 不自己猜该换哪个。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_fallbacks: Vec<String>,
}

/// 一条「acb 会话 ↔ harness 内部会话」的映射，握手时随 `Ready` 下发，
/// 让 harness 跨进程重启也能复用同一个内部会话（ADR-0018 的 session_maps 表）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMapRef {
    /// acb 会话 id。
    pub session: String,
    /// harness 内部会话 id（dsh 的 sessionId 之类）。
    pub harness_session: String,
}

/// 一份记忆在本轮投递里的信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// 记忆 id，`memory_*` 工具的 `memory` 参数用它。
    pub id: String,
    /// 这个会话在该记忆上的 worktree 路径（绝对路径）。agent 通过 `memory_*` 工具碰它，
    /// 桥负责把路径锁死在 worktree 内。
    pub worktree: String,
}

/// 开轮原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnReason {
    /// 被事件或消息叫醒（含攒够条数）。
    Wake,
    /// 时钟策略的例行检查。
    Clock,
}

/// 日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// 工具失败的错误码。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 参数不对。
    BadRequest,
    /// 票不对或已作废。
    Unauthorized,
    /// 这个设备不允许调用该工具。
    ToolNotAllowed,
    /// 目标房间不在订阅范围内。
    RoomNotSubscribed,
    /// 等超时或平台超时。
    Timeout,
    /// 桥自己出毛病了。
    Internal,
}

/// 工具失败的描述。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ToolError {
    /// 常见错误码的构造捷径。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// harness 对一轮失败的结构化分类。全部可选：旧 harness 不带就只走 `error` 文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnFailure {
    /// 失败属于哪个组件：`model` 或 `backend`。core 不认识的值按无结构化信息处理。
    pub component: String,
    /// 具体 id：模型引用或 harness id；空 = 由 core 按会话配置补默认值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 失败分类：`model_unavailable` / `model_unconfigured` / `auth_failed` /
    /// `rate_limited` / `internal` 等。core 原样存进依赖表。
    pub class: String,
}

/// 一帧。
///
/// `TurnStart` 比其它变体大（约 432B），但帧是短命协议对象；为省一次堆分配给
/// 所有变体加 indirection 不划算，这里明确接受。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum Frame {
    /// harness → 桥：握手。
    Hello {
        version: u32,
        /// harness 的名字。串行键 = harness 名 + 会话 id（ADR-0017）。
        harness: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// 桥 → harness：握手成功，绑定到这个 harness 接管的一组会话。
    ///
    /// 顺序即配置里的顺序。单会话时只有一个元素。
    Ready {
        sessions: Vec<String>,
        /// 已落盘的会话映射（acb 会话 ↔ harness 内部会话）。harness 用它复用旧会话，
        /// 跨进程重启也不丢上下文。没有就是空列表。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        maps: Vec<SessionMapRef>,
    },
    /// 桥 → harness：开一轮。
    TurnStart { delivery: Delivery },
    /// harness → 桥：本轮收工。
    TurnEnd {
        ticket: Ticket,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        /// v7 可选：失败的结构化分类。v6 harness 不带这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<TurnFailure>,
    },
    /// harness → 桥：调用工具。
    ToolCall {
        id: String,
        ticket: Ticket,
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// 桥 → harness：工具结果。
    ToolResult {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ToolError>,
    },
    /// 桥 → harness：日志。
    Log { level: LogLevel, message: String },
    /// 桥 → harness：控制指令。控制台发起的、要 harness 自己执行的动作（如重置会话）。
    /// 桥只转发，不替 harness 做——具体语义由 harness 决定（ADR-0018）。
    Control {
        /// 控制指令 id，harness 回报时原样带回来，好配对。
        id: String,
        /// 作用于哪个会话。
        session: String,
        /// 指令名。目前只有 `reset`（重置该会话的映射）。
        command: String,
        /// 指令参数。
        #[serde(default)]
        args: serde_json::Value,
    },
    /// harness → 桥：控制指令的结果。
    ControlResult {
        /// 对应哪条 `control` 的 id。
        id: String,
        session: String,
        command: String,
        /// 执行结果。失败时是字符串错误，成功时是 harness 回的结构。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// 保活。
    Ping,
    /// 保活回应。
    Pong,
}

impl Frame {
    /// 帧名，和 JSON 里的 `frame` 字段一致。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "hello",
            Self::Ready { .. } => "ready",
            Self::TurnStart { .. } => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Log { .. } => "log",
            Self::Control { .. } => "control",
            Self::ControlResult { .. } => "control_result",
            Self::Ping => "ping",
            Self::Pong => "pong",
        }
    }
}

/// 协议版本是否兼容。v7/v8 增加的都是可选字段，旧 v6 harness 仍然可用。
pub fn version_compatible(version: u32) -> bool {
    (6..=PROTOCOL_VERSION).contains(&version)
}

/// 编码成一帧（含行尾换行）。
pub fn encode(frame: &Frame) -> Result<String, ProtoError> {
    let mut line =
        serde_json::to_string(frame).map_err(|error| ProtoError::Encode(error.to_string()))?;
    line.push('\n');
    Ok(line)
}

/// 解开一行。容忍行尾的 `\n` / `\r`，其余一律严格。
pub fn decode(line: &str) -> Result<Frame, DecodeError> {
    let trimmed = line.trim_end_matches(['\n', '\r']);
    if trimmed.len() > MAX_FRAME_BYTES {
        return Err(DecodeError::TooLarge {
            size: trimmed.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|error| DecodeError::NotJson {
            detail: error.to_string(),
        })?;
    let tag = value
        .get("frame")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| DecodeError::MissingField {
            detail: acb_i18n::t!("proto.frame.missing_frame_field"),
        })?;

    match tag {
        "hello" | "ready" | "turn_start" | "turn_end" | "tool_call" | "tool_result" | "log"
        | "control" | "control_result" | "ping" | "pong" => {
            serde_json::from_value(value).map_err(|error| DecodeError::MissingField {
                detail: error.to_string(),
            })
        }
        other => Err(DecodeError::UnknownFrame {
            frame: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::Element;
    use crate::record::{RecordKind, Sender};

    fn sample_delivery() -> Delivery {
        Delivery {
            ticket: Ticket::new("t-1").expect("合法"),
            session: "main".into(),
            room: RoomId::new("qq", "group/123").expect("合法"),
            records: vec![Record {
                id: 1,
                room: RoomId::new("qq", "group/123").expect("合法"),
                kind: RecordKind::Message,
                sender: Sender {
                    id: "10001".into(),
                    name: "老王".into(),
                    role: None,
                },
                at: 1_793_000_000_000,
                elements: vec![Element::text("在吗")],
                at_self: true,
                reply_to_self: false,
                platform_id: None,
                quote_ref: None,
                labels: Vec::new(),
                suppressed: false,
                redelivery: None,
                note: None,
            }],
            reason: TurnReason::Wake,
            wake_reason: Some("@ 了我".into()),
            memories: Vec::new(),
            session_info: Some(SessionInfo {
                id: "main".into(),
                name: "main".into(),
                harness: "dsh".into(),
                model: None,
                model_fallbacks: Vec::new(),
            }),
            persona: None,
            more: None,
            omitted: None,
            trigger_id: Some(1),
            unread_ranges: vec![[1, 3]],
            unread_count: 3,
            unread_redelivered: 1,
            max_times: 2,
            turn_timeout_ms: None,
            prompts: Vec::new(),
        }
    }

    fn samples() -> Vec<Frame> {
        vec![
            Frame::Hello {
                version: PROTOCOL_VERSION,
                harness: "dsh".into(),
                token: Some("secret".into()),
            },
            Frame::Ready {
                sessions: vec!["main".into(), "second".into()],
                maps: vec![SessionMapRef {
                    session: "main".into(),
                    harness_session: "session-acb-main".into(),
                }],
            },
            Frame::TurnStart {
                delivery: sample_delivery(),
            },
            Frame::TurnEnd {
                ticket: Ticket::new("t-1").expect("合法"),
                error: None,
                failure: None,
            },
            Frame::TurnEnd {
                ticket: Ticket::new("t-1").expect("合法"),
                error: Some("模型网关不可用".into()),
                failure: Some(TurnFailure {
                    component: "model".into(),
                    id: Some("openai/gpt-4".into()),
                    class: "model_unavailable".into(),
                }),
            },
            Frame::ToolCall {
                id: "c-1".into(),
                ticket: Ticket::new("t-1").expect("合法"),
                tool: "send_message".into(),
                args: serde_json::json!({"elements": [{"type": "text", "text": "hi"}]}),
            },
            Frame::ToolResult {
                id: "c-1".into(),
                result: Some(serde_json::json!({"ok": true})),
                error: None,
            },
            Frame::ToolResult {
                id: "c-2".into(),
                result: None,
                error: Some(ToolError::new(
                    ErrorCode::RoomNotSubscribed,
                    "没有订阅这个房间",
                )),
            },
            Frame::Log {
                level: LogLevel::Warn,
                message: "上游拒绝".into(),
            },
            Frame::Control {
                id: "ctl-1".into(),
                session: "main".into(),
                command: "reset".into(),
                args: serde_json::json!({}),
            },
            Frame::ControlResult {
                id: "ctl-1".into(),
                session: "main".into(),
                command: "reset".into(),
                result: Some(serde_json::json!({"new_session": "session-abc"})),
                error: None,
            },
            Frame::Ping,
            Frame::Pong,
        ]
    }

    #[test]
    fn empty_prompts_are_omitted_for_old_harnesses() {
        let frame = Frame::TurnStart {
            delivery: sample_delivery(),
        };
        let line = encode(&frame).expect("可编码");
        assert!(
            !line.contains("\"prompts\""),
            "没有片段时不能凭空多一个字段：{line}"
        );
        let back = decode(&line).expect("旧形状仍可解码");
        let Frame::TurnStart { delivery } = back else {
            panic!("应当是 turn_start");
        };
        assert!(delivery.prompts.is_empty());
    }

    #[test]
    fn every_frame_round_trips_through_one_line() {
        for frame in samples() {
            let line = encode(&frame).expect("可编码");
            assert!(line.ends_with('\n'), "每帧一行");
            assert_eq!(line.matches('\n').count(), 1, "帧内不能有裸换行");
            let back = decode(&line).expect("可解码");
            assert_eq!(back, frame, "往返应当相等：{line}");
        }
    }

    #[test]
    fn frame_tag_matches_kind() {
        for frame in samples() {
            let line = encode(&frame).expect("可编码");
            let value: serde_json::Value = serde_json::from_str(line.trim()).expect("是 JSON");
            assert_eq!(value["frame"], frame.kind());
        }
    }

    #[test]
    fn clock_delivery_may_carry_no_records() {
        let mut delivery = sample_delivery();
        delivery.records.clear();
        delivery.reason = TurnReason::Clock;
        let line = encode(&Frame::TurnStart { delivery }).expect("可编码");
        let back = decode(&line).expect("可解码");
        match back {
            Frame::TurnStart { delivery } => {
                assert!(delivery.records.is_empty());
                assert_eq!(delivery.reason, TurnReason::Clock);
            }
            other => panic!("应当解成 turn_start，实际是 {}", other.kind()),
        }
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(matches!(
            decode("not json"),
            Err(DecodeError::NotJson { .. })
        ));
        assert!(matches!(
            decode(r#"{"no_frame":true}"#),
            Err(DecodeError::MissingField { .. })
        ));
        assert!(matches!(
            decode(r#"{"frame":"teleport"}"#),
            Err(DecodeError::UnknownFrame { .. })
        ));
    }

    #[test]
    fn decode_reports_oversized_frames() {
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(matches!(decode(&huge), Err(DecodeError::TooLarge { .. })));
    }

    #[test]
    fn version_accepts_v6_during_transition() {
        assert!(version_compatible(PROTOCOL_VERSION));
        assert!(
            version_compatible(7),
            "只多了可选字段，v7 harness 不必强制换"
        );
        assert!(version_compatible(6), "v6 harness 仍要能连上新桥");
        assert!(!version_compatible(5));
        assert!(!version_compatible(PROTOCOL_VERSION + 1));
    }

    #[test]
    fn v7_delivery_decodes_without_new_fields() {
        // 新桥还会给 `more` / `omitted`，所以旧 harness 的渲染路径仍能工作；
        // 反过来，v7 桥发来的 delivery 没有区间字段，新 harness 必须照常解出来。
        let legacy = serde_json::json!({
            "frame": "turn_start",
            "delivery": {
                "ticket": "t-1",
                "session": "main",
                "room": { "connection": "qq", "path": ["group", "123"] },
                "records": [],
                "reason": "wake"
            }
        });
        let frame = decode(&legacy.to_string()).expect("缺新字段要能解");
        match frame {
            Frame::TurnStart { delivery } => {
                assert!(delivery.unread_ranges.is_empty());
                assert_eq!(delivery.unread_count, 0);
                assert!(delivery.trigger_id.is_none());
            }
            other => panic!("应当解成 delivery，实际是 {}", other.kind()),
        }
    }

    #[test]
    fn v6_turn_end_decodes_without_failure() {
        let back = decode(r#"{"frame":"turn_end","ticket":"t-1","error":"boom"}"#)
            .expect("旧 harness 的帧要能解");
        match back {
            Frame::TurnEnd { failure, .. } => assert!(failure.is_none()),
            other => panic!("应当解成 turn_end，实际是 {}", other.kind()),
        }
    }

    #[test]
    fn session_info_tolerates_missing_model_fallbacks() {
        // 后备链是后加的字段：老桥发来的 session_info 没有它，新驱动必须照常解出来，
        // 否则一次升级就会让所有还没换代的桥连不上新驱动。
        let legacy = r#"{"id":"main","name":"main","harness":"dsh","model":"trae/x"}"#;
        let info: SessionInfo = serde_json::from_str(legacy).expect("缺字段要能解");
        assert_eq!(info.model.as_deref(), Some("trae/x"));
        assert!(info.model_fallbacks.is_empty(), "缺字段 = 不重试");
    }

    #[test]
    fn session_info_carries_model_fallbacks_in_order() {
        let info = SessionInfo {
            id: "main".into(),
            name: "main".into(),
            harness: "dsh".into(),
            model: Some("trae/a".into()),
            model_fallbacks: vec!["trae/b".into(), "trae/c".into()],
        };
        let wire = serde_json::to_string(&info).expect("序列化");
        let back: SessionInfo = serde_json::from_str(&wire).expect("反序列化");
        assert_eq!(
            back.model_fallbacks,
            vec!["trae/b", "trae/c"],
            "顺序就是优先级"
        );
    }

    #[test]
    fn tool_result_error_is_optional_in_wire_form() {
        let frame = Frame::ToolResult {
            id: "c-1".into(),
            result: Some(serde_json::json!(1)),
            error: None,
        };
        let line = encode(&frame).expect("可编码");
        assert!(!line.contains("error"), "没有错误时不该出现 error 字段");
    }
}
