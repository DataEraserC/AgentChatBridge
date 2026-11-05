//! 参考 WASM 插件：access 决策。
//!
//! 输入是宿主传进来的 access 请求 JSON；输出是
//! `{"decision":"allow"|"deny","reason":"..."}`。
//!
//! 规则（与 `.so` 参考实现一致）：
//! - 目标房间是 `qq:group/1` → allow / `room allowlisted`
//! - 其他房间 → deny / `room not allowlisted`
//! - 没有目标房间 → deny / `no room`
//!
//! 单文件、`no_std`、无任何外部依赖；用 wasm32-unknown-unknown 编成 cdylib。
//! 重新构建见 `build-access-wasm.sh`。

#![no_std]
#![allow(static_mut_refs)]

const HEAP_SIZE: usize = 2048;
const OUT_SIZE: usize = 128;
const ALLOW: &[u8] = b"{\"decision\":\"allow\",\"reason\":\"room allowlisted\"}";
const DENY: &[u8] = b"{\"decision\":\"deny\",\"reason\":\"room not allowlisted\"}";
const NO_ROOM: &[u8] = b"{\"decision\":\"deny\",\"reason\":\"no room\"}";
const NEEDLE: &[u8] = b"\"room\":\"";

static mut HEAP: [u8; HEAP_SIZE] = [0; HEAP_SIZE];
static mut HEAP_TOP: usize = 0;
static mut OUT: [u8; OUT_SIZE] = [0; OUT_SIZE];
static mut OUT_LEN: i32 = 0;

#[no_mangle]
pub extern "C" fn acb_alloc(len: i32) -> i32 {
    if len <= 0 || len as usize > HEAP_SIZE {
        return -1;
    }
    unsafe {
        if HEAP_TOP + len as usize > HEAP_SIZE {
            HEAP_TOP = 0;
        }
        let base = core::ptr::addr_of_mut!(HEAP) as *mut u8;
        let offset = HEAP_TOP;
        HEAP_TOP += len as usize;
        base.add(offset) as i32
    }
}

#[no_mangle]
pub extern "C" fn acb_decide(ptr: i32, len: i32) -> i32 {
    if ptr < 0 || len <= 0 {
        return write_result(NO_ROOM);
    }
    let input = unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let room = find_room(input);
    let text = match room {
        Some(room) if room == b"qq:group/1" => ALLOW,
        Some(_) => DENY,
        None => NO_ROOM,
    };
    write_result(text)
}

#[no_mangle]
pub extern "C" fn acb_result_len() -> i32 {
    unsafe { OUT_LEN }
}

fn find_room(input: &[u8]) -> Option<&[u8]> {
    let start = find(input, NEEDLE)? + NEEDLE.len();
    let rest = &input[start..];
    let end = rest.iter().position(|byte| *byte == b'"')?;
    Some(&rest[..end])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .find(|index| &haystack[*index..*index + needle.len()] == needle)
}

fn write_result(text: &[u8]) -> i32 {
    let base = core::ptr::addr_of_mut!(OUT) as *mut u8;
    let len = text.len().min(OUT_SIZE);
    unsafe {
        for (index, byte) in text[..len].iter().enumerate() {
            core::ptr::write(base.add(index), *byte);
        }
        OUT_LEN = len as i32;
    }
    base as i32
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
