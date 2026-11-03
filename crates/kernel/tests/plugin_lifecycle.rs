//! 插件生命周期：从目录状态到轮次边界卸载/重载。
//!
//! 用一个假 loader 替代真实 `.so`，只验证 Root/Bridge 的状态机：
//! queue → pending → 轮次边界执行 → unloaded → reload → unloadable。
//! 真实 dlopen/dlclose 由 `acb-adapter-dynamic` 的 unload_v2 测试覆盖。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, Ingest, LoaderKind, LoaderManifest,
    ManagerManifest, ManagerPlugin, MemoryStore, Outbound, OutboundError, PLUGIN_API_VERSION,
    PluginError, PluginInfo, PluginLifecycle, PluginLoader, PluginManager, PluginMetadata,
    PluginSource, ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, RecordDraft,
    RoomId, Sender, Sent, WakeContext, WakeDecision, WakePolicy, WakeProvider,
};
use serde_json::Value;

const AT: i64 = 1_793_000_000_000;

fn test_id() -> ProviderId {
    ProviderId::parse("testplg.wake").expect("合法 provider id")
}

struct AlwaysWake;

impl WakeProvider for AlwaysWake {
    fn decide(&self, _options: &WakePolicy, _context: &WakeContext<'_>) -> WakeDecision {
        WakeDecision::Wake
    }

    fn decide_with_reason(
        &self,
        _options: &WakePolicy,
        _context: &WakeContext<'_>,
    ) -> acb_kernel::WakeVerdict {
        acb_kernel::WakeVerdict::wake("test loader")
    }
}

struct TestManager;

impl ManagerPlugin for TestManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            "testplg",
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                test_id(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
            .with_source(PluginSource::Dynamic)
            .with_metadata(PluginMetadata {
                summary: Some("测试插件".into()),
                ..PluginMetadata::EMPTY
            }),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id == &test_id() {
            Ok(ProviderInstance::Wake(Arc::new(AlwaysWake)))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }
}

struct TestLoader {
    installed: AtomicBool,
    fail_unload: AtomicBool,
    unloads: AtomicUsize,
    reloads: AtomicUsize,
    inactive: Mutex<Option<PluginInfo>>,
}

impl TestLoader {
    fn new() -> Self {
        Self {
            installed: AtomicBool::new(false),
            fail_unload: AtomicBool::new(false),
            unloads: AtomicUsize::new(0),
            reloads: AtomicUsize::new(0),
            inactive: Mutex::new(None),
        }
    }
}

impl PluginLoader for TestLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Dynamic,
            PLUGIN_API_VERSION,
            vec![ExtensionPoint::Wake],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        if self.installed.load(Ordering::SeqCst) {
            return Ok(());
        }
        manager.install_manager(Arc::new(TestManager))?;
        self.installed.store(true, Ordering::SeqCst);
        *self
            .inactive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(())
    }

    fn unload(&self, manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        if id != &test_id() {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if self.fail_unload.load(Ordering::SeqCst) {
            return Err(PluginError::Loader("test unload failure".to_owned()));
        }
        if !self.installed.load(Ordering::SeqCst) {
            return Err(PluginError::Loader("test plugin is not loaded".to_owned()));
        }
        let mut info = manager
            .info(id)
            .ok_or_else(|| PluginError::UnknownProvider(id.to_string()))?;
        manager.uninstall_manager(id.namespace())?;
        info.lifecycle = PluginLifecycle::Unloaded;
        *self
            .inactive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(info);
        self.installed.store(false, Ordering::SeqCst);
        self.unloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn can_unload(&self, id: &ProviderId) -> bool {
        id == &test_id()
    }

    fn inactive(&self) -> Vec<PluginInfo> {
        self.inactive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .into_iter()
            .collect()
    }

    fn reload(&self, manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        if id != &test_id() {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        if self.installed.load(Ordering::SeqCst) {
            return Err(PluginError::Loader(
                "test plugin is already loaded".to_owned(),
            ));
        }
        manager.install_manager(Arc::new(TestManager))?;
        self.installed.store(true, Ordering::SeqCst);
        *self
            .inactive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.reloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct NoopOutbound;

impl Outbound for NoopOutbound {
    fn send(&mut self, _room: &RoomId, _elements: &[Element]) -> Result<Sent, OutboundError> {
        Ok(Sent {
            platform_id: "test".to_owned(),
            at: AT,
        })
    }
}

fn config_with_wake(provider: &str) -> String {
    format!(
        r#"
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
        room = "group/123"

        [sessions.subscriptions.wake]
        provider = "{provider}"
        "#
    )
}

fn bridge(text: &str, loader: Arc<TestLoader>) -> Bridge<MemoryStore> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(loader)
        .expect("测试 loader 应当能安装");
    Bridge::with_plugins(
        Config::parse(text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
    )
}

fn catalog_entry(bridge: &Bridge<MemoryStore>, id: &str) -> Value {
    bridge
        .plugin_catalog()
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("目录里没有 {id}"))
}

fn state(bridge: &Bridge<MemoryStore>, id: &str) -> String {
    catalog_entry(bridge, id)
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn message(room: &str, at_self: bool) -> RecordDraft {
    RecordDraft::message(
        RoomId::parse(room).expect("房间合法"),
        Sender {
            id: "10001".to_owned(),
            name: "老王".to_owned(),
            role: None,
        },
        AT,
        vec![Element::text("在吗")],
        at_self,
    )
}

#[test]
fn unload_marks_pending_then_applies_at_boundary_and_shows_unloaded() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader.clone());

    assert_eq!(state(&bridge, "testplg.wake"), "unloadable");

    bridge
        .queue_unload("testplg.wake")
        .expect("可卸载插件应当能入队");
    assert_eq!(state(&bridge, "testplg.wake"), "pending_unload");
    assert_eq!(
        catalog_entry(&bridge, "testplg.wake")
            .get("pending")
            .and_then(Value::as_str),
        Some("unload")
    );

    assert!(bridge.poll_turn().expect("能检查队列").is_none());
    assert_eq!(state(&bridge, "testplg.wake"), "unloaded");
    assert_eq!(loader.unloads.load(Ordering::SeqCst), 1);
}

#[test]
fn reload_from_unloaded_returns_to_unloadable() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader.clone());

    bridge.queue_unload("testplg.wake").expect("卸载入队");
    assert!(bridge.poll_turn().expect("能检查队列").is_none());
    assert_eq!(state(&bridge, "testplg.wake"), "unloaded");

    bridge
        .queue_reload("testplg.wake")
        .expect("已卸载插件可以重载");
    assert_eq!(state(&bridge, "testplg.wake"), "pending_reload");
    assert!(bridge.poll_turn().expect("能检查队列").is_none());

    assert_eq!(state(&bridge, "testplg.wake"), "unloadable");
    assert_eq!(loader.unloads.load(Ordering::SeqCst), 1);
    assert_eq!(loader.reloads.load(Ordering::SeqCst), 1);
}

#[test]
fn reload_loaded_queues_unload_and_reload_then_returns_to_unloadable() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader.clone());

    bridge
        .queue_reload("testplg.wake")
        .expect("已加载插件可以重载");
    assert_eq!(state(&bridge, "testplg.wake"), "pending_reload");

    assert!(bridge.poll_turn().expect("能检查队列").is_none());
    assert_eq!(state(&bridge, "testplg.wake"), "unloadable");
    assert_eq!(loader.unloads.load(Ordering::SeqCst), 1);
    assert_eq!(loader.reloads.load(Ordering::SeqCst), 1);
}

#[test]
fn unload_is_held_while_a_turn_is_active() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader.clone());

    let ingested = bridge
        .ingest(message("qq:group/123", true))
        .expect("能记下来");
    assert_eq!(
        ingested,
        Ingest::Recorded {
            id: 1,
            woke: vec!["main".to_owned()]
        }
    );
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;

    bridge.queue_unload("testplg.wake").expect("卸载入队");
    assert_eq!(state(&bridge, "testplg.wake"), "pending_unload");
    assert!(
        bridge.poll_turn().expect("能检查队列").is_none(),
        "有在途轮次时不能执行卸载"
    );
    assert_eq!(state(&bridge, "testplg.wake"), "pending_unload");

    bridge.end_turn(&ticket, None).expect("能收工");
    assert!(bridge.poll_turn().expect("能检查队列").is_none());
    assert_eq!(state(&bridge, "testplg.wake"), "unloaded");
}

#[test]
fn resident_plugin_rejects_unload_and_reload() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader);

    let unload = bridge.queue_unload("acb.wake.default");
    assert!(unload.is_err(), "内置 provider 是常驻，不能热卸载");
    let reload = bridge.queue_reload("acb.wake.default");
    assert!(reload.is_err(), "内置 provider 不能热重载");
    assert_eq!(state(&bridge, "acb.wake.default"), "resident");
}

#[test]
fn referenced_plugin_rejects_unload() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("testplg.wake"), loader);

    let unload = bridge.queue_unload("testplg.wake");
    let error = unload.err().expect("被引用时必须拒绝");
    let text = error.to_string();
    assert!(
        text.contains("testplg.wake") && text.contains("session:main wake"),
        "错误要说明被谁引用：{text}"
    );
    assert_eq!(state(&bridge, "testplg.wake"), "unloadable");

    let reload = bridge.queue_reload("testplg.wake");
    assert!(
        reload.is_err(),
        "已加载且被引用时，重载包含卸载，也必须拒绝"
    );
}

#[test]
fn unknown_plugin_rejects_unload_and_reload() {
    let loader = Arc::new(TestLoader::new());
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader);

    assert!(bridge.queue_unload("missing.plugin").is_err());
    assert!(bridge.queue_reload("missing.plugin").is_err());
}

#[test]
fn boundary_failure_sets_last_error_and_keeps_loaded() {
    let loader = Arc::new(TestLoader::new());
    loader.fail_unload.store(true, Ordering::SeqCst);
    let mut bridge = bridge(&config_with_wake("acb.wake.default"), loader);

    bridge.queue_unload("testplg.wake").expect("卸载入队");
    assert!(bridge.poll_turn().expect("能检查队列").is_none());

    let entry = catalog_entry(&bridge, "testplg.wake");
    assert_eq!(
        entry.get("state").and_then(Value::as_str),
        Some("unloadable"),
        "卸载失败时插件仍在加载状态"
    );
    let last_error = entry
        .get("lastError")
        .and_then(Value::as_str)
        .expect("失败原因必须可见");
    assert!(last_error.contains("test unload failure"), "{last_error}");
}
