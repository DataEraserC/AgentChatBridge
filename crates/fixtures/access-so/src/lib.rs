//! 参考 `.so` 载体：ABI v2 + `UNLOAD_SAFE`，单个 access 决策 provider。
//!
//! 决策规则与参考 wasm 模块完全一致：
//! - 目标房间是 `qq:group/1` → `allow` / `room allowlisted`
//! - 其他房间 → `deny` / `room not allowlisted`
//! - 没有目标房间 → `deny` / `no room`
//!
//! 只用于 conformance；由 `acb-wasm-host` 的 `native-so` feature 构建。

use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::size_of;

use acb_adapter_api::abi::{
    ABI_VERSION_V2, AcbPluginV2, HostV1, PLUGIN_FLAG_UNLOAD_SAFE,
};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireAccessRequest, WireError, WireManifest, WirePluginMetadata,
    WireResult, decode_line,
};
use serde_json::Value;

const PROVIDER_ID: &str = "demo.access_allowlist";

static TABLE: AcbPluginV2 = AcbPluginV2 {
    abi_version: ABI_VERSION_V2,
    struct_size: size_of::<AcbPluginV2>() as u32,
    flags: PLUGIN_FLAG_UNLOAD_SAFE,
    manifest: Some(fixture_manifest),
    create: Some(fixture_create),
    invoke: Some(fixture_invoke),
    destroy: Some(fixture_destroy),
    free_string: Some(fixture_free_string),
};

/// 唯一的导出符号。
#[unsafe(no_mangle)]
pub extern "C" fn acb_plugin_entry_v2() -> *const AcbPluginV2 {
    &TABLE
}

unsafe extern "C" fn fixture_manifest() -> *mut c_char {
    let manifest = WireManifest {
        id: PROVIDER_ID.to_owned(),
        platform: "demo".to_owned(),
        kind: String::new(),
        transports: Vec::new(),
        api_version: 1,
        capabilities: vec!["access_decision".to_owned()],
        events: Vec::new(),
        event_tags: Vec::new(),
        supports_at: false,
        supports_reply: false,
        text_limit: None,
        extension: "access".to_owned(),
        metadata: WirePluginMetadata {
            title: Some("参考 .so access provider".to_owned()),
            summary: Some("ABI v2 UNLOAD_SAFE 参考实现：按房间返回 allow/deny".to_owned()),
            description: None,
            tags: vec!["fixture".to_owned(), "access".to_owned()],
            effects: Vec::new(),
            docs_url: None,
            options_schema: None,
        },
    };
    into_c_string(serde_json::to_string(&manifest).expect("manifest 可序列化"))
}

unsafe extern "C" fn fixture_create(
    _config_json: *const c_char,
    _host: *const HostV1,
) -> *mut c_void {
    Box::into_raw(Box::new(0_u8)) as *mut c_void
}

unsafe extern "C" fn fixture_invoke(handle: *mut c_void, request_json: *const c_char) -> *mut c_char {
    if handle.is_null() || request_json.is_null() {
        return into_c_string("{\"type\":\"reply\",\"id\":0,\"result\":{\"outcome\":\"error\",\"error\":{\"kind\":\"bad_params\",\"detail\":\"null handle or request\"}}}");
    }
    let text = unsafe { CStr::from_ptr(request_json) }
        .to_string_lossy()
        .into_owned();
    let message: CoreMessage = match decode_line(&text) {
        Ok(message) => message,
        Err(error) => {
            return into_c_string(
                &serde_json::json!({
                    "type": "reply",
                    "id": 0,
                    "result": {
                        "outcome": "error",
                        "error": { "kind": "bad_params", "detail": error.to_string() }
                    }
                })
                .to_string(),
            );
        }
    };

    let reply = match message {
        CoreMessage::Decide {
            id,
            point,
            request,
            ..
        } if point == "access" => {
            let (decision, reason) = decide(&request);
            AdapterMessage::Reply {
                id,
                result: WireResult::Decision {
                    decision: decision.to_owned(),
                    reason: Some(reason.to_owned()),
                    delay_ms: 0,
                },
            }
        }
        CoreMessage::Decide { id, .. } => AdapterMessage::Reply {
            id,
            result: WireResult::Error {
                error: WireError::NotImplemented {
                    detail: "only access decisions are implemented".to_owned(),
                },
            },
        },
        CoreMessage::Shutdown => AdapterMessage::Reply {
            id: 0,
            result: WireResult::Error {
                error: WireError::NotImplemented {
                    detail: "shutdown is handled by the loader".to_owned(),
                },
            },
        },
        CoreMessage::Describe { id }
        | CoreMessage::Send { id, .. }
        | CoreMessage::Call { id, .. } => AdapterMessage::Reply {
            id,
            result: WireResult::Error {
                error: WireError::NotImplemented {
                    detail: "adapter operations are not implemented".to_owned(),
                },
            },
        },
    };
    into_c_string(serde_json::to_string(&reply).expect("reply 可序列化"))
}

fn decide(request: &Value) -> (&'static str, &'static str) {
    let access: WireAccessRequest = match serde_json::from_value(request.clone()) {
        Ok(access) => access,
        Err(_) => return ("deny", "no room"),
    };
    match access.room.as_deref() {
        Some("qq:group/1") => ("allow", "room allowlisted"),
        Some(_) => ("deny", "room not allowlisted"),
        None => ("deny", "no room"),
    }
}

unsafe extern "C" fn fixture_destroy(handle: *mut c_void) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle as *mut u8) });
    }
}

unsafe extern "C" fn fixture_free_string(text: *mut c_char) {
    if !text.is_null() {
        drop(unsafe { CString::from_raw(text) });
    }
}

fn into_c_string(text: impl AsRef<str>) -> *mut c_char {
    CString::new(text.as_ref())
        .expect("no NUL in fixture strings")
        .into_raw()
}
