//! 外置进程策略 provider 载体（最小面：access）。
//!
//! 子进程协议见 [`protocol`]：第一行 `provider_manifest`，之后 core 发
//! `CoreMessage::Decide`，宿主回 `AdapterMessage::Reply`。
//!
//! 这个 crate 不认识 wasm、不需要任何 wasm 运行时；它只认「一个命令 + stdio」。

mod channel;
mod config;
mod loader;
mod protocol;
mod provider;

pub use config::ProcessProviderConfig;
pub use loader::ProcessProviderLoader;
