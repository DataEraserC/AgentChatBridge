//! 带外通知子进程。
//!
//! 输入是 stdio 上的行 JSON-RPC（bridge 只发 `notify`，不等待回复）。
//! 输出是 ntfy 兼容的 HTTP 请求：`POST {url}/{topic}`，标题/优先级/标签走
//! header，正文是纯文本摘要。去重、恢复配对和失败重试都在这里，core 不知道
//! HTTP 的存在。

use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead};
use std::thread;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// 一条通知最多重试几次（不含首次）；总共最多 4 次尝试。
const MAX_RETRIES: usize = 3;
/// 每次重试前等多久。
const RETRY_DELAYS_MS: [u64; MAX_RETRIES] = [250, 500, 1000];
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Deserialize)]
struct Notification {
    method: String,
    params: Event,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    DependencyEdge {
        kind: String,
        id: String,
        from: String,
        to: String,
        since: i64,
        last_ok_at: Option<i64>,
        class: Option<String>,
        detail: Option<String>,
        attempts: u64,
    },
    TurnSuppressed {
        session: String,
        room: String,
        component: Option<String>,
        component_id: Option<String>,
        class: Option<String>,
        reason: String,
        fingerprint: String,
    },
}

struct Args {
    endpoint: String,
    token: Option<String>,
    min_interval: Duration,
}

#[derive(Clone)]
struct DownSnapshot {
    since: i64,
    class: Option<String>,
    detail: Option<String>,
}

struct EdgeBody<'a> {
    component: &'a str,
    from: &'a str,
    to: &'a str,
    since: i64,
    last_ok_at: Option<i64>,
    class: Option<&'a str>,
    detail: Option<&'a str>,
    attempts: u64,
    recovery: Option<&'a DownSnapshot>,
}

struct Notifier {
    endpoint: String,
    token: Option<String>,
    min_interval: Duration,
    agent: ureq::Agent,
    last_push: HashMap<String, Instant>,
    down: HashMap<String, DownSnapshot>,
    down_pushed: HashSet<String>,
    failures: u64,
}

impl Notifier {
    fn new(args: Args) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .build()
            .into();
        Self {
            endpoint: args.endpoint,
            token: args.token,
            min_interval: args.min_interval,
            agent,
            last_push: HashMap::new(),
            down: HashMap::new(),
            down_pushed: HashSet::new(),
            failures: 0,
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::DependencyEdge {
                kind,
                id,
                from,
                to,
                since,
                last_ok_at,
                class,
                detail,
                attempts,
            } => {
                let component = format!("{kind}/{id}");
                if to == "down" {
                    self.down.insert(
                        component.clone(),
                        DownSnapshot {
                            since,
                            class: class.clone(),
                            detail: detail.clone(),
                        },
                    );
                }
                let key = format!(
                    "dep|{component}|{from}|{to}|{}",
                    class.as_deref().unwrap_or("-")
                );
                if self.duplicate(&key) {
                    return;
                }
                if to == "up" && !self.down_pushed.contains(&component) {
                    // 没有成功推送过 down 就不配对 up，避免「只报好不报坏」。
                    return;
                }
                let recovery = (to == "up")
                    .then(|| self.down.get(&component))
                    .flatten()
                    .map(|down| DownSnapshot {
                        since: down.since,
                        class: down.class.clone(),
                        detail: down.detail.clone(),
                    });
                let title = match to.as_str() {
                    "down" => format!("[acb] {component} is down"),
                    "up" => format!("[acb] {component} recovered"),
                    other => format!("[acb] {component} is {other}"),
                };
                let priority = if to == "down" { "high" } else { "default" };
                let tags = format!("acb,{kind},{to}");
                let body = dependency_body(EdgeBody {
                    component: &component,
                    from: &from,
                    to: &to,
                    since,
                    last_ok_at,
                    class: class.as_deref(),
                    detail: detail.as_deref(),
                    attempts,
                    recovery: recovery.as_ref(),
                });
                let ok = self.push(&title, priority, &tags, &body);
                self.last_push.insert(key, Instant::now());
                if ok && to == "up" {
                    self.down_pushed.remove(&component);
                } else if ok && to == "down" {
                    self.down_pushed.insert(component.clone());
                }
                if !ok && to == "up" {
                    // 恢复没发出去：保留 down_pushed，下一次 up 还能再配对。
                }
            }
            Event::TurnSuppressed {
                session,
                room,
                component,
                component_id,
                class,
                reason,
                fingerprint,
            } => {
                let label = match (&component, &component_id) {
                    (Some(component), Some(id)) => format!("{component}/{id}"),
                    (Some(component), None) => component.clone(),
                    _ => format!("session/{session}"),
                };
                let key = format!(
                    "turn|{session}|{room}|{label}|{}|{reason}",
                    class.as_deref().unwrap_or("-")
                );
                if self.duplicate(&key) {
                    return;
                }
                let title = format!("[acb] turn failure suppressed ({session})");
                let body = format!(
                    "component={label}\nsession={session}\nroom={room}\nreason={reason}\nclass={}\nfingerprint={fingerprint}",
                    class.as_deref().unwrap_or("-")
                );
                self.push(&title, "low", "acb,turn,suppressed", &body);
                self.last_push.insert(key, Instant::now());
            }
        }
    }

    fn duplicate(&mut self, key: &str) -> bool {
        let now = Instant::now();
        self.last_push
            .retain(|_, at| now.duration_since(*at) < self.min_interval);
        self.last_push.contains_key(key)
    }

    fn push(&mut self, title: &str, priority: &str, tags: &str, body: &str) -> bool {
        for attempt in 0..=MAX_RETRIES {
            let mut request = self
                .agent
                .post(&self.endpoint)
                .header("Title", title)
                .header("Priority", priority)
                .header("Tags", tags)
                .header("Content-Type", "text/plain; charset=utf-8");
            if let Some(token) = self.token.as_deref() {
                request = request.header("Authorization", &format!("Bearer {token}"));
            }
            match request.send(body) {
                Ok(_) => return true,
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    eprintln!(
                        "notifier: push failed ({}/{}): {error}; failures={}",
                        attempt + 1,
                        MAX_RETRIES + 1,
                        self.failures
                    );
                }
            }
            if let Some(delay) = RETRY_DELAYS_MS.get(attempt) {
                thread::sleep(Duration::from_millis(*delay));
            }
        }
        false
    }
}

fn dependency_body(edge: EdgeBody<'_>) -> String {
    let EdgeBody {
        component,
        from,
        to,
        since,
        last_ok_at,
        class,
        detail,
        attempts,
        recovery,
    } = edge;
    let mut body = format!(
        "component={component}\ntransition={from}->{to}\nstate={to} since={}\n",
        format_time(since)
    );
    if let Some(last_ok) = last_ok_at {
        body.push_str(&format!("last_ok={}\n", format_time(last_ok)));
    }
    if let Some(class) = class {
        body.push_str(&format!("class={class}\n"));
    }
    if let Some(detail) = detail {
        body.push_str(&format!("lastError={detail}\n"));
    }
    body.push_str(&format!("attempts={attempts}\n"));
    if let Some(down) = recovery {
        body.push_str(&format!(
            "down_since={}\nduration={}\n",
            format_time(down.since),
            format_duration((since.saturating_sub(down.since)).max(0) / 1000)
        ));
        if let Some(class) = down.class.as_deref() {
            body.push_str(&format!("down_class={class}\n"));
        }
        if let Some(detail) = down.detail.as_deref() {
            body.push_str(&format!("down_lastError={detail}\n"));
        }
    }
    body
}

fn parse_args() -> Result<Args, String> {
    let mut url = None;
    let mut topic = None;
    let mut token = None;
    let mut min_interval_secs = 600u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => url = args.next(),
            "--topic" => topic = args.next(),
            "--token" => token = args.next(),
            "--min-interval-secs" => {
                min_interval_secs = args
                    .next()
                    .ok_or_else(|| "missing value for --min-interval-secs".to_owned())?
                    .parse()
                    .map_err(|_| "invalid --min-interval-secs".to_owned())?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    let url = url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| "missing --url".to_owned())?;
    let topic = topic
        .as_deref()
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .ok_or_else(|| "missing --topic".to_owned())?;
    Ok(Args {
        endpoint: format!(
            "{}/{}",
            url.trim_end_matches('/'),
            topic.trim_start_matches('/')
        ),
        token,
        min_interval: Duration::from_secs(min_interval_secs.max(1)),
    })
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("notifier: {error}");
            std::process::exit(2);
        }
    };
    let mut notifier = Notifier::new(args);
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("notifier: cannot read stdin: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let frame: Notification = match serde_json::from_str(&line) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("notifier: invalid frame: {error}");
                continue;
            }
        };
        if frame.method != "notify" {
            continue;
        }
        notifier.handle(frame.params);
    }
}

fn format_time(ms: i64) -> String {
    if ms < 0 {
        return ms.to_string();
    }
    let total_secs = ms / 1000;
    let days = total_secs / 86_400;
    let seconds = total_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// Howard Hinnant 的 civil_from_days：Unix 天数 → (年, 月, 日)。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (
        year + if month <= 2 { 1 } else { 0 },
        month as u32,
        day as u32,
    )
}

fn format_duration(secs: i64) -> String {
    if secs < 0 {
        return format!("{secs}s");
    }
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}
