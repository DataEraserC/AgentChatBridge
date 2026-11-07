//! 健康探针必须能对着**带鉴权的 console** 工作。
//!
//! 这次线上事故的缺口不是探针逻辑本身，而是从来没有人用“401 的 /healthz”
//! 跑过它：`HttpProbe` 不带 Authorization，/healthz 一直 401，探针永远不健康，
//! 最终 78 放弃且 systemd 不再重试。这个集成测试把那条路径钉死。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

use acb_supervisor::{HttpProbe, Probe};

/// 一个只认 Bearer 的 /healthz：expected 为空时接受无鉴权请求（模拟公开端点）。
fn spawn_healthz(expected: Arc<Mutex<String>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind healthz");
    let address = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 2048];
            let count = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..count]);
            let expected = expected
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let authorized = expected.is_empty()
                || request.lines().any(|line| {
                    line.eq_ignore_ascii_case(&format!("authorization: bearer {expected}"))
                });
            let (status, body) = if authorized {
                ("200 OK", "ok")
            } else {
                ("401 Unauthorized", "unauthorized")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{address}/healthz")
}

fn token_file(name: &str, token: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "acb-health-auth-{name}-{}-{token}",
        std::process::id()
    ));
    std::fs::write(&path, token).expect("write token");
    path
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[test]
fn probe_without_token_is_unhealthy_when_console_requires_auth() {
    let url = spawn_healthz(Arc::new(Mutex::new("s3cret".to_owned())));
    let mut probe = HttpProbe::new(&url);
    assert!(!probe.healthy(), "401 must not be treated as healthy");
}

#[test]
fn probe_with_token_file_passes_bearer_auth() {
    let url = spawn_healthz(Arc::new(Mutex::new("s3cret".to_owned())));
    let path = token_file("ok", "s3cret\n");
    let mut probe = HttpProbe::with_token_file(&url, &path);
    assert!(probe.healthy(), "token file should authorize the probe");
    remove(&path);
}

#[test]
fn probe_rereads_token_after_rotation() {
    let expected = Arc::new(Mutex::new("first".to_owned()));
    let url = spawn_healthz(Arc::clone(&expected));
    let path = token_file("rotate", "first");
    let mut probe = HttpProbe::with_token_file(&url, &path);

    assert!(probe.healthy(), "initial token must work");
    // bridge 重启后换了 token：探针不能缓存旧值。
    std::fs::write(&path, "second").expect("rotate token");
    *expected.lock().unwrap() = "second".to_owned();
    assert!(probe.healthy(), "probe must re-read the rotated token");

    std::fs::write(&path, "first").expect("restore old token");
    assert!(!probe.healthy(), "stale token must fail after rotation");
    remove(&path);
}

#[test]
fn required_token_file_missing_is_unhealthy_and_does_not_fall_back() {
    // 服务端接受无鉴权请求：如果探针在缺文件时偷偷退回普通 GET，会误判健康。
    let url = spawn_healthz(Arc::new(Mutex::new(String::new())));
    let missing =
        std::env::temp_dir().join(format!("acb-health-auth-missing-{}", std::process::id()));
    let _ = std::fs::remove_file(&missing);
    let mut probe = HttpProbe::require_token_file(&url, &missing);
    assert!(
        !probe.healthy(),
        "explicitly configured token file must fail closed when missing"
    );
}

#[test]
fn auto_mode_without_token_file_still_probes_public_endpoint() {
    let url = spawn_healthz(Arc::new(Mutex::new(String::new())));
    let missing = std::env::temp_dir().join(format!("acb-health-auth-auto-{}", std::process::id()));
    let _ = std::fs::remove_file(&missing);
    let mut probe = HttpProbe::with_token_file(&url, &missing);
    assert!(
        probe.healthy(),
        "auto mode must keep working for endpoints without auth"
    );
}
