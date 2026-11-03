//! extensions_* authoring 工具：管理员判定、路径 jail、写入后显式 reload。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_extensions::{DynamicRouteManager, MANIFEST_FILE, NAMESPACE};
use acb_kernel::{
    Bridge, Config, Element, ErrorCode, MemoryStore, Outbound, OutboundError, PluginManager,
    RecordDraft, RoomId, Sender, Sent,
};
use serde_json::{Value, json};

fn temp_root() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时钟")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("acb-ext-tool-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn write_module(root: &Path, dir: &str, manifest: &str) {
    let module = root.join(dir);
    fs::create_dir_all(&module).expect("建模块目录");
    fs::write(module.join(MANIFEST_FILE), manifest).expect("写 manifest");
}

fn manifest(id: &str, prefix: &str) -> String {
    format!(
        r#"
id = "{id}"
api_version = 1
carrier = "declarative"
extension = "route"
summary = "test route"
capabilities = ["route_decision"]

[match]
text_prefix = "{prefix}"

[action]
kind = "send"
text = "ok"
"#
    )
}

fn config(admin: bool) -> String {
    let admins = if admin { "[\"10001\"]" } else { "[]" };
    format!(
        r#"
        [[connections]]
        id = "qq"
        platform = "qq"
        kind = "onebot11"
        url = "ws://127.0.0.1:3001"
        account = "bot"
        admin_qqs = {admins}

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

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/1"
        "#
    )
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

fn bridge(root: &Path, admin: bool) -> Bridge<MemoryStore> {
    let manager = Arc::new(DynamicRouteManager::load(root).expect("加载目录"));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_manager(manager)
        .expect("安装动态扩展 manager");
    Bridge::with_plugins(
        Config::parse(&config(admin)).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )
}

fn message(at_self: bool) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse("qq:group/1").expect("房间"),
        Sender {
            id: "10001".into(),
            name: "admin".into(),
            role: None,
        },
        1,
        vec![Element::text("在吗")],
        at_self,
    )
}

fn ticket(bridge: &mut Bridge<MemoryStore>) -> acb_kernel::Ticket {
    bridge.ingest(message(true)).expect("入库");
    bridge.poll_turn().expect("开轮").expect("有活").ticket
}

fn call(
    bridge: &mut Bridge<MemoryStore>,
    ticket: &acb_kernel::Ticket,
    tool: &str,
    args: Value,
) -> Result<Value, acb_kernel::ToolError> {
    bridge.tool_call(ticket, &["main".to_owned()], tool, args)
}

#[test]
fn admin_can_list_read_write_and_reload() {
    let root = temp_root();
    write_module(&root, "echo", &manifest("dyn.echo", "/echo"));
    let mut bridge = bridge(&root, true);
    let ticket = ticket(&mut bridge);

    let listed = call(&mut bridge, &ticket, "extensions_list", json!({})).expect("list");
    assert_eq!(listed["installed"], true);
    assert_eq!(
        listed["details"]["modules"].as_array().map(Vec::len),
        Some(1)
    );

    let read = call(
        &mut bridge,
        &ticket,
        "extensions_read",
        json!({"path": "echo/manifest.toml"}),
    )
    .expect("read");
    assert!(read["text"].as_str().expect("text").contains("dyn.echo"));

    let bad_path = call(
        &mut bridge,
        &ticket,
        "extensions_read",
        json!({"path": "../escape.toml"}),
    )
    .expect_err(".. 必须拒绝");
    assert_eq!(bad_path.code, ErrorCode::BadRequest);

    let written = call(
        &mut bridge,
        &ticket,
        "extensions_write",
        json!({"path": "new/manifest.toml", "text": manifest("dyn.new", "/new")}),
    )
    .expect("write");
    assert_eq!(written["written"], true);

    let reload = call(&mut bridge, &ticket, "extensions_reload", json!({})).expect("reload");
    assert_eq!(reload["queued"], true);
    assert_eq!(bridge.extension_catalog(NAMESPACE)["pendingReload"], true);

    bridge.end_turn(&ticket, None).expect("收工");
    bridge.poll_turn().expect("边界");
    let modules = bridge.extension_catalog(NAMESPACE)["details"]["modules"]
        .as_array()
        .expect("modules")
        .len();
    assert_eq!(modules, 2, "新模块在轮次边界生效");
    assert!(root.join("new/manifest.toml").is_file());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn non_admin_is_refused() {
    let root = temp_root();
    write_module(&root, "echo", &manifest("dyn.echo", "/echo"));
    let mut bridge = bridge(&root, false);
    let ticket = ticket(&mut bridge);

    let error =
        call(&mut bridge, &ticket, "extensions_list", json!({})).expect_err("非管理员必须拒绝");
    assert_eq!(error.code, ErrorCode::ToolNotAllowed);
    assert!(error.message.contains("admin") || error.message.contains("管理员"));

    let _ = fs::remove_dir_all(&root);
}
