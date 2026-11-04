//! 带外通知事件。
//!
//! kernel 只定义事件形状和 sink 接口，不包含任何 HTTP/推送实现；真正的
//! notifier 在进程外，通过 stdio 收事件后自己决定怎么发。

use serde::Serialize;

/// 一条可以走带外通道的事件。字段只来自依赖表与 in-band 抑制路径，
/// 不读取 harness 内部状态。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum NoticeEvent {
    /// 依赖状态发生迁移。
    DependencyEdge {
        /// `harness` / `platform` / `model`。
        kind: String,
        id: String,
        from: String,
        to: String,
        /// 新状态的开始时间（Unix 毫秒）。
        since: i64,
        last_ok_at: Option<i64>,
        class: Option<String>,
        detail: Option<String>,
        attempts: u64,
    },
    /// 一次 turn 失败通知被 in-band 去重/限流抑制。
    TurnSuppressed {
        session: String,
        room: String,
        component: Option<String>,
        component_id: Option<String>,
        class: Option<String>,
        /// `duplicate` 或 `room_limit`。
        reason: String,
        fingerprint: String,
    },
}

/// 带外通知接收端。实现必须是线程安全的，且 `notify` 不能阻塞主流程。
pub trait NoticeSink: Send + Sync {
    fn notify(&self, event: NoticeEvent);
}
