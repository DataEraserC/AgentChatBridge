//! 外置进程工具载体（MVP）。
//!
//! 第三方工具跑在 bridge 管理的子进程里，协议是 stdio 上的行 JSON-RPC：
//! `initialize` / `list_tools` / `call_tool` / `shutdown`。
//! 每调用有超时；崩溃按退避重启并标记 Unhealthy；Unhealthy 期间调用立即失败。

mod loader;
mod protocol;
mod provider;

pub use loader::ProcessToolLoader;
pub use provider::{Backoff, ProcessToolProvider};
