//! LKG 状态机：候选 → 安全点提升 → 连续失败回滚 → 重启恢复。
//!
//! 用假 loader 替代 `.so`，只验证 Bridge 的安全点与状态文件；真实 dlopen/dlclose
//! 由 `acb-adapter-dynamic` 覆盖。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    Bridge, Capability, Config, Element, ExtensionPoint, Health, LoaderKind, LoaderManifest,
    ManagerManifest, ManagerPlugin, MemoryStore, Outbound, OutboundError, PLUGIN_API_VERSION,
    PluginArtifact, PluginError, PluginInfo, PluginLifecycle, PluginLkg, PluginLoader,
    PluginManager, PluginMetadata, PluginSource, ProviderDescriptor, ProviderId, ProviderInstance,
    ProviderOptions, RecordDraft, RoomId, Sender, Sent, WakeContext, WakeDecision, WakePolicy,
    WakeProvider,
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
}

struct TestManager {
    unhealthy: Arc<AtomicBool>,
}

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

    fn health(&self) -> Health {
        if self.unhealthy.load(Ordering::SeqCst) {
            Health::Unhealthy("test unhealthy".to_owned())
        } else {
            Health::Healthy
        }
    }
}

struct TestLoader {
    installed: AtomicBool,
    unloads: AtomicUsize,
    reloads: AtomicUsize,
    /// 当前产物路径；回滚时由 `set_artifact_path` 改写。
    path: Mutex<String>,
    version: Mutex<String>,
    unhealthy: Arc<AtomicBool>,
    inactive: Mutex<Option<PluginInfo>>,
}

impl TestLoader {
    fn new(path: &str, version: &str) -> Self {
        Self {
            installed: AtomicBool::new(false),
            unloads: AtomicUsize::new(0),
            reloads: AtomicUsize::new(0),
            path: Mutex::new(path.to_owned()),
            version: Mutex::new(version.to_owned()),
            unhealthy: Arc::new(AtomicBool::new(false)),
            inactive: Mutex::new(None),
        }
    }

    fn set_unhealthy(&self, unhealthy: bool) {
        self.unhealthy.store(unhealthy, Ordering::SeqCst);
    }

    fn current_path(&self) -> String {
        self.path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn install_manager(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        manager.install_manager(Arc::new(TestManager {
            unhealthy: Arc::clone(&self.unhealthy),
        }))
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
        self.install_manager(manager)?;
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
        self.install_manager(manager)?;
        self.installed.store(true, Ordering::SeqCst);
        *self
            .inactive
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.reloads.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn artifact(&self, id: &ProviderId) -> Option<PluginArtifact> {
        if id != &test_id() {
            return None;
        }
        Some(PluginArtifact {
            path: self.current_path(),
            version: Some(
                self.version
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone(),
            ),
        })
    }

    fn set_artifact_path(&self, id: &ProviderId, path: &str) -> Result<(), PluginError> {
        if id != &test_id() {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        *self
            .path
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = path.to_owned();
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
        token = {{ secret = "t-main" }}

        [[sessions.subscriptions]]
        connection = "qq"
        room = "group/123"

        [sessions.subscriptions.wake]
        provider = "{provider}"
        "#
    )
}

fn temp_path(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("acb-lkg-it-{name}-{nanos}"))
}

fn artifact_file(root: &std::path::Path, name: &str, content: &str) -> String {
    let path = root.join(name);
    std::fs::write(&path, content).expect("能写测试插件文件");
    path.to_string_lossy().into_owned()
}

fn bridge_with_lkg(
    text: &str,
    loader: Arc<TestLoader>,
    lkg_path: std::path::PathBuf,
) -> Bridge<MemoryStore> {
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(loader)
        .expect("测试 loader 应当能安装");
    Bridge::with_plugins_lkg(
        Config::parse_fixture(text).expect("配置合法"),
        MemoryStore::new(),
        Box::new(NoopOutbound),
        plugins,
        lkg_path,
    )
}

fn catalog_entry(bridge: &Bridge<MemoryStore>, id: &str) -> Value {
    bridge
        .plugin_catalog()
        .into_iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("目录里没有 {id}"))
}

fn lkg_state(bridge: &Bridge<MemoryStore>, id: &str) -> String {
    catalog_entry(bridge, id)
        .get("lkgState")
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
fn candidate_promotes_at_turn_boundary() {
    let root = temp_path("promote");
    std::fs::create_dir_all(&root).expect("临时目录");
    let v1 = artifact_file(&root, "v1.so", "version one");
    let loader = Arc::new(TestLoader::new(&v1, "1.0.0"));
    let lkg_path = root.join("plugins-lkg.toml");
    let mut bridge = bridge_with_lkg(&config_with_wake("acb.wake.default"), loader, lkg_path);

    assert_eq!(lkg_state(&bridge, "testplg.wake"), "candidate");
    assert!(bridge.poll_turn().expect("能检查队列").is_none());
    assert_eq!(lkg_state(&bridge, "testplg.wake"), "lkg");
    let version = catalog_entry(&bridge, "testplg.wake")
        .get("lkgVersion")
        .and_then(Value::as_str)
        .map(str::to_owned);
    assert_eq!(version.as_deref(), Some("1.0.0"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failure_blocks_promotion() {
    let root = temp_path("blocked");
    std::fs::create_dir_all(&root).expect("临时目录");
    let v1 = artifact_file(&root, "v1.so", "version one");
    let loader = Arc::new(TestLoader::new(&v1, "1.0.0"));
    let lkg_path = root.join("plugins-lkg.toml");
    let mut bridge = bridge_with_lkg(
        &config_with_wake("acb.wake.default"),
        loader.clone(),
        lkg_path,
    );

    loader.set_unhealthy(true);
    let _ = bridge.poll_turn().expect("能检查队列");
    assert_eq!(lkg_state(&bridge, "testplg.wake"), "candidate");
    loader.set_unhealthy(false);
    let _ = bridge.poll_turn().expect("能检查队列");
    assert_eq!(
        lkg_state(&bridge, "testplg.wake"),
        "candidate",
        "出现失败后，本次加载不能再提升"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn three_failures_trigger_rollback_and_service_continues() {
    let root = temp_path("rollback");
    std::fs::create_dir_all(&root).expect("临时目录");
    let v1 = artifact_file(&root, "v1.so", "version one");
    let v2 = artifact_file(&root, "v2.so", "version two");
    let lkg_path = root.join("plugins-lkg.toml");

    // 先种一条历史 LKG（v1），再让桥以 v2 启动。
    let digest_v1 = acb_kernel::lkg::digest_file(std::path::Path::new(&v1)).expect("摘要");
    let mut seeded = PluginLkg::new(lkg_path.clone());
    seeded.register_candidate(test_id(), "1.0.0".into(), digest_v1, v1.clone());
    assert!(seeded.promote(&test_id(), AT));
    drop(seeded);

    let loader = Arc::new(TestLoader::new(&v2, "2.0.0"));
    let mut bridge = bridge_with_lkg(
        &config_with_wake("testplg.wake"),
        loader.clone(),
        lkg_path.clone(),
    );

    for index in 0..3 {
        loader.set_unhealthy(true);
        let _ = bridge.poll_turn().expect("能检查队列");
        if index < 2 {
            loader.set_unhealthy(false);
            let _ = bridge.poll_turn().expect("能检查队列");
        }
    }
    assert_eq!(lkg_state(&bridge, "testplg.wake"), "rolledBack");
    assert_eq!(loader.current_path(), v1, "应改回上一条 LKG 的路径");
    assert_eq!(loader.unloads.load(Ordering::SeqCst), 1);
    assert_eq!(loader.reloads.load(Ordering::SeqCst), 1);
    assert!(
        bridge.validate_plugins().is_ok(),
        "回滚后 provider 仍应可解析"
    );

    // 下一次安全点把回滚目标重新提升为 LKG。
    let _ = bridge.poll_turn().expect("能检查队列");
    assert_eq!(lkg_state(&bridge, "testplg.wake"), "lkg");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn restart_restores_lkg_history_from_file() {
    let root = temp_path("restart");
    std::fs::create_dir_all(&root).expect("临时目录");
    let v1 = artifact_file(&root, "v1.so", "version one");
    let lkg_path = root.join("plugins-lkg.toml");
    let digest = acb_kernel::lkg::digest_file(std::path::Path::new(&v1)).expect("摘要");
    let mut seeded = PluginLkg::new(lkg_path.clone());
    seeded.register_candidate(test_id(), "1.0.0".into(), digest.clone(), v1.clone());
    assert!(seeded.promote(&test_id(), AT));
    drop(seeded);

    let restored = PluginLkg::new(lkg_path);
    let entry = restored.latest_lkg(&test_id()).expect("能从文件恢复");
    assert_eq!(entry.version, "1.0.0");
    assert_eq!(entry.digest, digest);
    assert_eq!(entry.promoted_at, AT);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn turn_boundary_helper_is_idle_before_opening_a_turn() {
    let root = temp_path("active");
    std::fs::create_dir_all(&root).expect("临时目录");
    let v1 = artifact_file(&root, "v1.so", "version one");
    let loader = Arc::new(TestLoader::new(&v1, "1.0.0"));
    let lkg_path = root.join("plugins-lkg.toml");
    let mut bridge = bridge_with_lkg(&config_with_wake("acb.wake.default"), loader, lkg_path);

    let ingested = bridge
        .ingest(message("qq:group/123", true))
        .expect("能记下来");
    assert!(matches!(ingested, acb_kernel::Ingest::Recorded { .. }));
    // `poll_turn` 先做安全点提升，再开轮；开轮后 active 非空，后续安全点不再提升。
    let ticket = bridge.poll_turn().expect("能开轮").expect("有活").ticket;
    assert_eq!(lkg_state(&bridge, "testplg.wake"), "lkg");
    let _ = bridge.poll_turn().expect("能检查队列");
    assert_eq!(
        lkg_state(&bridge, "testplg.wake"),
        "lkg",
        "有在途轮次时不得再做提升/回滚"
    );
    bridge.end_turn(&ticket, None).expect("能收工");
    let _ = std::fs::remove_dir_all(root);
}
