//! ABI 版本不匹配的参考 `.so`：入口形状正确，但 `abi_version = 1`。
//!
//! loader 必须在读函数指针之前拒绝它，返回明确错误而不是段错误。

use std::mem::size_of;

use acb_adapter_api::abi::{AcbPluginV2, PLUGIN_FLAG_UNLOAD_SAFE};

static TABLE: AcbPluginV2 = AcbPluginV2 {
    abi_version: 1,
    struct_size: size_of::<AcbPluginV2>() as u32,
    flags: PLUGIN_FLAG_UNLOAD_SAFE,
    manifest: None,
    create: None,
    invoke: None,
    destroy: None,
    free_string: None,
};

#[unsafe(no_mangle)]
pub extern "C" fn acb_plugin_entry_v2() -> *const AcbPluginV2 {
    &TABLE
}
