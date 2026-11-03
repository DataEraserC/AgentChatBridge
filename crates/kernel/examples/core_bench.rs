//! core 内存与延迟基准（可复跑）。
//!
//! 用法：
//!   cargo run --release -p acb-kernel --example core_bench
//!
//! 父进程会为 0 / 100 / 500 个观察者各起一个干净子进程，避免同一进程里分配器
//! 高水位污染对比。子进程输出 JSON 行，父进程原样转发。
//!
//! 指标口径：
//! - `rss_kb`：/proc/self/status 的 VmRSS（当前常驻）；
//! - `hwm_kb`：VmHWM（进程峰值常驻）；
//! - `turn_us`：ingest → poll_turn → end_turn 一整轮的墙钟耗时；
//! - 观察者是空实现，数字代表“插件数量本身的成本”，不含插件内部逻辑。

use std::sync::Arc;
use std::time::Instant;

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, MemoryStore, Outbound, OutboundError,
    PLUGIN_API_VERSION, PluginManager, ProviderDescriptor, ProviderId, Record, RecordDraft,
    RecordObserver, RoomId, Sender, Sent,
};

const CONFIG: &str = r#"
[[connections]]
id = "qq"
platform = "qq"
kind = "onebot11"
url = "ws://127.0.0.1:3001"
account = "bot"

[[personas]]
id = "p"
name = "人格"
text = "你是桥。"

[[harnesses]]
id = "dsh"
name = "DSH"

[[sessions]]
id = "main"
harness = "dsh"
persona = "p"
token = "t-main"

[[sessions.subscriptions]]
connection = "qq"
room = "group/1"
"#;

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "bench".to_owned(),
            at: 1,
        })
    }
}

struct NullObserver;

impl RecordObserver for NullObserver {
    fn observe(&self, _record: &Record) {}
}

fn rss_kb(field: &str) -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find(|line| line.starts_with(field))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

fn build(observers: usize) -> (Bridge<MemoryStore>, u128) {
    let started = Instant::now();
    let mut plugins = PluginManager::with_native_builtins();
    for index in 0..observers {
        plugins
            .register_observer(
                ProviderDescriptor::new(
                    ProviderId::parse(&format!("bench.obs{index}")).expect("合法 id"),
                    ExtensionPoint::Observer,
                    PLUGIN_API_VERSION,
                    vec![Capability::ObservedRecord],
                ),
                Arc::new(NullObserver),
            )
            .expect("注册观察者");
    }
    let bridge = Bridge::with_plugins(
        Config::parse(CONFIG).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    );
    (bridge, started.elapsed().as_micros())
}

fn run_turns(bridge: &mut Bridge<MemoryStore>, turns: usize) -> Vec<u128> {
    let room = RoomId::parse("qq:group/1").expect("房间");
    let mut times = Vec::with_capacity(turns);
    for index in 0..turns {
        let started = Instant::now();
        bridge
            .ingest(RecordDraft::message(
                room.clone(),
                Sender {
                    id: "10001".into(),
                    name: "u".into(),
                    role: None,
                },
                1_793_000_000_000 + index as i64,
                vec![Element::text("在吗")],
                true,
            ))
            .expect("入库");
        let delivery = bridge.poll_turn().expect("能开轮").expect("有活");
        bridge.end_turn(&delivery.ticket, None).expect("能收轮");
        times.push(started.elapsed().as_micros());
    }
    times
}

fn percentile(sorted: &[u128], percent: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let index = ((sorted.len() - 1) * percent) / 100;
    sorted[index]
}

fn child(observers: usize, turns: usize) {
    let start_rss = rss_kb("VmRSS:");
    let (mut bridge, build_us) = build(observers);
    let bridge_rss = rss_kb("VmRSS:");
    let mut times = run_turns(&mut bridge, turns);
    let peak_rss = rss_kb("VmHWM:");
    times.sort_unstable();

    println!(
        "{{\"metric\":\"rss_kb\",\"observers\":{observers},\"phase\":\"start\",\"value\":{start_rss}}}"
    );
    println!(
        "{{\"metric\":\"rss_kb\",\"observers\":{observers},\"phase\":\"bridge\",\"value\":{bridge_rss}}}"
    );
    println!("{{\"metric\":\"hwm_kb\",\"observers\":{observers},\"value\":{peak_rss}}}");
    println!("{{\"metric\":\"build_us\",\"observers\":{observers},\"value\":{build_us}}}");
    println!(
        "{{\"metric\":\"turn_us\",\"observers\":{observers},\"turns\":{turns},\"p50\":{},\"p95\":{},\"p99\":{},\"max\":{}}}",
        percentile(&times, 50),
        percentile(&times, 95),
        percentile(&times, 99),
        times.last().copied().unwrap_or(0),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(position) = args.iter().position(|arg| arg == "--observers") {
        let observers = args
            .get(position + 1)
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let turns = args
            .iter()
            .position(|arg| arg == "--turns")
            .and_then(|position| args.get(position + 1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(50);
        child(observers, turns);
        return;
    }

    let executable = std::env::current_exe().expect("能定位自己");
    for observers in [0_usize, 100, 500] {
        let status = std::process::Command::new(&executable)
            .args(["--observers", &observers.to_string(), "--turns", "50"])
            .status()
            .expect("能起子进程");
        assert!(status.success(), "子进程 {observers} 失败");
    }
}
