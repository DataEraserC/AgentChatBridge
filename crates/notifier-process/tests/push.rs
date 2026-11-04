//! notifier 子进程的端到端测试：真的 spawn 二进制，用本地 stub HTTP server
//! 断言 ntfy 请求形状与去重行为，不依赖外网。

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

struct Stub {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Stub {
    fn start(statuses: Vec<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("能监听");
        listener.set_nonblocking(true).expect("能设非阻塞");
        let addr = listener.local_addr().expect("有地址");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let statuses = Arc::new(Mutex::new(VecDeque::from(statuses)));
        let seen = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).ok();
                        let status = statuses
                            .lock()
                            .expect("没被毒死")
                            .pop_front()
                            .unwrap_or(200);
                        if let Some(request) = read_request(stream, status) {
                            seen.lock().expect("没被毒死").push(request);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("没被毒死").clone()
    }

    fn wait_for(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.requests().len() < count {
            assert!(
                Instant::now() < deadline,
                "等不到第 {count} 个请求，当前 {} 个",
                self.requests().len()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(mut stream: TcpStream, status: u16) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let head_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buffer[..head_end]);
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        let value = value.trim();
        if key.eq_ignore_ascii_case("Content-Length") {
            content_length = value.parse().unwrap_or(0);
        }
        headers.push((key.trim().to_owned(), value.to_owned()));
    }
    let mut body = buffer[head_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    let response =
        format!("HTTP/1.1 {status} Stub\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(response.as_bytes()).ok()?;
    Some(Request {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

struct NotifierChild {
    child: Child,
    stdin: ChildStdin,
}

impl NotifierChild {
    fn spawn(url: &str, topic: &str, token: Option<&str>, min_interval_secs: u64) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_acb-notifier"));
        command
            .arg("--url")
            .arg(url)
            .arg("--topic")
            .arg(topic)
            .arg("--min-interval-secs")
            .arg(min_interval_secs.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if let Some(token) = token {
            command.arg("--token").arg(token);
        }
        let mut child = command.spawn().expect("能启动 notifier");
        let stdin = child.stdin.take().expect("stdin 可用");
        Self { child, stdin }
    }

    fn send(&mut self, frame: &str) {
        self.stdin
            .write_all(frame.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .expect("能写进子进程");
    }
}

impl Drop for NotifierChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn edge(kind: &str, id: &str, from: &str, to: &str, class: &str) -> String {
    edge_at(kind, id, from, to, class, 1_793_772_538_000)
}

fn edge_at(kind: &str, id: &str, from: &str, to: &str, class: &str, since: i64) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","method":"notify","params":{{"event":"dependency_edge","kind":"{kind}","id":"{id}","from":"{from}","to":"{to}","since":{since},"last_ok_at":1793772537000,"class":"{class}","detail":"connect refused","attempts":12}}}}"#
    )
}

#[test]
fn sends_ntfy_compatible_request_and_dedupes_same_edge() {
    let stub = Stub::start(vec![200, 200]);
    let mut child = NotifierChild::spawn(&stub.url(), "acb", Some("secret"), 600);

    let down = edge("platform", "qq", "up", "down", "unreachable");
    child.send(&down);
    child.send(&down);
    child.send(&edge("harness", "dsh", "up", "down", "closed"));
    stub.wait_for(2);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(stub.requests().len(), 2, "同一依赖边沿 10 分钟内只推一次");

    let requests = stub.requests();
    let first = &requests[0];
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/acb");
    assert_eq!(first.header("Title"), Some("[acb] platform/qq is down"));
    assert_eq!(first.header("Priority"), Some("high"));
    assert_eq!(first.header("Tags"), Some("acb,platform,down"));
    assert_eq!(first.header("Authorization"), Some("Bearer secret"));
    assert!(
        first.body.contains("component=platform/qq"),
        "{}",
        first.body
    );
    assert!(first.body.contains("transition=up->down"), "{}", first.body);
    assert!(first.body.contains("state=down since="), "{}", first.body);
    assert!(first.body.contains("class=unreachable"), "{}", first.body);
    assert!(
        first.body.contains("lastError=connect refused"),
        "{}",
        first.body
    );
    assert!(first.body.contains("attempts=12"), "{}", first.body);
}

#[test]
fn recovery_carries_down_since_and_duration() {
    let stub = Stub::start(vec![200, 200]);
    let mut child = NotifierChild::spawn(&stub.url(), "acb", None, 600);

    child.send(&edge("harness", "dsh", "up", "down", "closed"));
    stub.wait_for(1);
    child.send(&edge_at(
        "harness",
        "dsh",
        "down",
        "up",
        "internal",
        1_793_772_541_000,
    ));
    stub.wait_for(2);

    let requests = stub.requests();
    let recovery = &requests[1];
    assert_eq!(
        recovery.header("Title"),
        Some("[acb] harness/dsh recovered")
    );
    assert_eq!(recovery.header("Priority"), Some("default"));
    assert!(
        recovery.body.contains("transition=down->up"),
        "{}",
        recovery.body
    );
    assert!(recovery.body.contains("down_since="), "{}", recovery.body);
    assert!(recovery.body.contains("duration=3s"), "{}", recovery.body);
}

#[test]
fn unpaired_recovery_is_not_pushed() {
    let stub = Stub::start(vec![200]);
    let mut child = NotifierChild::spawn(&stub.url(), "acb", None, 600);
    child.send(&edge("harness", "dsh", "unknown", "up", "internal"));
    thread::sleep(Duration::from_millis(300));
    assert!(
        stub.requests().is_empty(),
        "没有推送过 down 就不配对 up：{:?}",
        stub.requests()
    );
}

#[test]
fn push_retries_are_bounded() {
    let stub = Stub::start(vec![500, 500, 500, 200]);
    let mut child = NotifierChild::spawn(&stub.url(), "acb", None, 600);
    child.send(&edge(
        "model",
        "test/model",
        "unknown",
        "down",
        "model_unavailable",
    ));
    stub.wait_for(4);
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(
        stub.requests().len(),
        4,
        "首次 + 3 次重试后停止，不无限重试"
    );
}

#[test]
fn duplicate_turn_suppression_is_pushed_once() {
    let stub = Stub::start(vec![200]);
    let mut child = NotifierChild::spawn(&stub.url(), "acb", None, 600);
    let suppressed = r#"{"jsonrpc":"2.0","method":"notify","params":{"event":"turn_suppressed","session":"main","room":"qq:group/123","component":"model","component_id":"test/model","class":"model_unavailable","reason":"duplicate","fingerprint":"16ff5911729d45ee"}}"#;
    child.send(suppressed);
    child.send(suppressed);
    stub.wait_for(1);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(stub.requests().len(), 1);

    let request = &stub.requests()[0];
    assert_eq!(
        request.header("Title"),
        Some("[acb] turn failure suppressed (main)")
    );
    assert_eq!(request.header("Priority"), Some("low"));
    assert!(request.body.contains("session=main"), "{}", request.body);
    assert!(
        request.body.contains("room=qq:group/123"),
        "{}",
        request.body
    );
    assert!(
        request.body.contains("reason=duplicate"),
        "{}",
        request.body
    );
}
