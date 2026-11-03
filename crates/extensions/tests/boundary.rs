//! 动态扩展目录与 Bridge 轮次边界：queue → pending → 边界 swap → 下一轮生效。
//!
//! 这一层不启动真实适配器/端口，只验证 `Bridge::queue_extension_reload`、
//! `poll_turn` 的安全点，以及 manager 的“坏模块不替换线上快照”。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_extensions::{DynamicRouteManager, MANIFEST_FILE, NAMESPACE, ROUTES_PROVIDER};
use acb_kernel::{
    Bridge, Config, Element, MemoryStore, Outbound, OutboundError, PluginManager, ProviderId,
    RecordKind, RoomId, RouteDecision, RoutePolicy, RouteProvider, Sender, Sent,
};
use serde_json::Value;

fn temp_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时钟")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("acb-ext-bridge-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn write_module(root: &Path, dir: &str, manifest: &str) {
    let module = root.join(dir);
    fs::create_dir_all(&module).expect("建模块目录");
    fs::write(module.join(MANIFEST_FILE), manifest).expect("写 manifest");
}

fn module_manifest(id: &str, prefix: &str) -> String {
    format!(
        r#"
id = "{id}"
api_version = 1
carrier = "declarative"
extension = "route"
summary = "test route"
capabilities = ["route_decision"]
effects = ["may_drop"]

[match]
text_prefix = "{prefix}"

[action]
kind = "send"
text = "ok"
"#
    )
}

fn config() -> &'static str {
    r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"

        [[connections.routes]]
        provider = "dyn.routes"

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
    "#
}

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: 1,
        })
    }
}

fn context<'a>(
    room: &'a RoomId,
    sender: &'a Sender,
    kind: &'a RecordKind,
    text: &'a str,
    sessions: &'a [String],
) -> acb_kernel::RouteContext<'a> {
    acb_kernel::RouteContext {
        connection: &room.connection,
        platform: "qq",
        room,
        sender,
        is_admin: false,
        kind,
        elements: &[],
        text,
        at_self: true,
        reply_to_self: false,
        has_body: true,
        event: None,
        tags: &[],
        sessions,
    }
}

fn route_policy() -> RoutePolicy {
    RoutePolicy {
        provider: ProviderId::parse(ROUTES_PROVIDER).expect("合法"),
        options: serde_json::json!({}),
    }
}

fn route(plugin_manager: &PluginManager) -> Arc<dyn RouteProvider> {
    plugin_manager
        .resolve_route(
            &ProviderId::parse(ROUTES_PROVIDER).expect("合法"),
            &route_policy(),
        )
        .expect("dyn.routes 应当可解析")
}

fn decide(provider: &Arc<dyn RouteProvider>, text: &str) -> RouteDecision {
    let room = RoomId::parse("qq:group/1").expect("房间");
    let sender = Sender {
        id: "10001".into(),
        name: "u".into(),
        role: None,
    };
    let kind = RecordKind::Message;
    let sessions = vec!["main".to_owned()];
    provider.decide(
        &route_policy(),
        &context(&room, &sender, &kind, text, &sessions),
    )
}

#[test]
fn boundary_reload_swaps_modules_and_keeps_live_on_bad_candidate() {
    let root = temp_root();
    write_module(&root, "one", &module_manifest("dyn.one", "/one"));
    let manager = Arc::new(DynamicRouteManager::load(&root).expect("加载目录"));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(manager)
        .expect("动态扩展 manager 应当能安装");
    let route = route(&plugins);
    let mut bridge = Bridge::with_plugins(
        Config::parse(config()).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    );

    assert!(matches!(
        decide(&route, "/one hi"),
        RouteDecision::Handle { .. }
    ));

    write_module(&root, "two", &module_manifest("dyn.two", "/two"));
    bridge
        .queue_extension_reload(NAMESPACE)
        .expect("重载应当能入队");
    assert_eq!(catalog(&bridge)["pendingReload"], true);
    bridge.poll_turn().expect("能处理边界");
    assert_eq!(catalog(&bridge)["pendingReload"], false);
    assert_eq!(module_count(&bridge), 2);
    assert!(matches!(
        decide(&route, "/two hi"),
        RouteDecision::Handle { .. }
    ));

    write_module(
        &root,
        "bad",
        r#"
id = "dyn.bad"
api_version = 1
carrier = "declarative"
extension = "route"
summary = ""
capabilities = ["route_decision"]

[match]
text_prefix = "/bad"

[action]
kind = "send"
text = "no"
"#,
    );
    bridge
        .queue_extension_reload(NAMESPACE)
        .expect("重载应当能入队");
    bridge.poll_turn().expect("能处理边界");
    let snapshot = catalog(&bridge);
    assert!(snapshot["lastError"].as_str().is_some());
    assert_eq!(
        snapshot["details"]["errors"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(module_count(&bridge), 2, "坏模块不能替换线上快照");
    assert!(matches!(
        decide(&route, "/two hi"),
        RouteDecision::Handle { .. }
    ));
    assert!(matches!(decide(&route, "/bad hi"), RouteDecision::Pass));

    fs::remove_dir_all(root.join("bad")).expect("删坏模块");
    fs::remove_dir_all(root.join("two")).expect("删第二个模块");
    bridge
        .queue_extension_reload(NAMESPACE)
        .expect("重载应当能入队");
    bridge.poll_turn().expect("能处理边界");
    assert_eq!(catalog(&bridge)["lastError"], Value::Null);
    assert_eq!(module_count(&bridge), 1);
    assert_eq!(decide(&route, "/two hi"), RouteDecision::Pass);

    let _ = fs::remove_dir_all(&root);
}

fn catalog(bridge: &Bridge<MemoryStore>) -> Value {
    bridge.extension_catalog(NAMESPACE)
}

fn module_count(bridge: &Bridge<MemoryStore>) -> usize {
    catalog(bridge)["details"]["modules"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0)
}
