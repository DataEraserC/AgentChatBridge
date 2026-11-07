//! `acb` 的库入口：只编译 profile generation 与指针提交需要的模块。
//!
//! `acb-launcher` 直接调用 [`profile::add_payload`] / [`profile::activate_generation`]
//! 完成升级 stage，不再 shell out 到 `acbctl profile add`。CLI 仍由同一份源码的
//! `acbctl` bin 提供。

pub mod contract;
pub mod fsutil;
mod pins;
pub mod profile;
