//! QQ 官方机器人适配器（QQ 开放平台，v2 接口）。
//!
//! 与 OneBot（snowluma / NapCat 系）的区别：官方机器人走腾讯开放平台的
//! WebSocket 网关 + REST 接口，没有「任意发消息」的能力——群聊只有被 @ 时才
//! 收得到（GROUP_AT_MESSAGE_CREATE），回复必须在收到消息后的被动窗口内
//! （带 `msg_id` + 递增 `msg_seq`），超出窗口或配额就会失败。这是平台限制，
//! 不是适配器的偷工减料。
//!
//! 分工与 OneBot 适配器一致（ADR-0004 / 0005）：
//! - **进**：网关 dispatch 事件解成 [`acb_kernel::RecordDraft`]；
//! - **出**：文本内容编成被动回复，按房间调 REST 接口；
//! - 房间写法：`<连接>:group/<group_openid>`、`<连接>:private/<user_openid>`
//!   （官方机器人只见 openid，拿不到 QQ 号）。
//!
//! 鉴权（v2）：`POST {token_url}/app/getAppAccessToken`（appId + clientSecret）
//! 换 `access_token`（≤7200 秒），API 与网关 identify 都用 `QQBot <token>`。
//! Token 鉴权方式已被官方废弃，这里只实现 access token。
//!
//! 传输只支持 `ws-client`：桥主动连 `wss://api.sgroup.qq.com/websocket`。

pub mod event;
pub mod factory;
pub mod link;
pub mod outbound;
pub mod token;

pub use event::{EventError, Parsed};
pub use factory::{MANIFEST, QqBotOfficialAdapterFactory};
pub use link::supervise;
pub use outbound::{QqBotOfficialOutbound, ReplyState};
pub use token::TokenManager;
