//! 动态库平台适配器（ADR-0027 Phase 3 P3）。
//!
//! `.so` / `.dylib` 只导出一个符号 `acb_adapter_entry_v1`，返回一张
//! [`AdapterV1`] 函数表。跨 ABI 的所有文本都是 JSON，形状与外置进程共用
//! `acb-adapter-api` 的 wire：
//!
//! - `manifest()` → `WireManifest`；
//! - `create(WireConnection JSON, HostV1)` → 不透明 handle；
//! - `invoke(handle, CoreMessage JSON)` → `AdapterMessage` JSON；
//! - 插件通过 `HostV1.submit_event` 把入站记录交给 core。
//!
//! 配置：
//!
//! ```toml
//! [[adapters]]
//! path = "/path/to/libdemo_adapter.so"
//! kind = "demo"   # 可选；manifest 的 kind 必须等于它
//! ```

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use acb_adapter_api::abi::{
    ABI_VERSION, ABI_VERSION_V2, AcbPluginV2, AdapterV1, ENTRY_SYMBOL, ENTRY_SYMBOL_V2, EntryFn,
    EntryFnV2, HostV1, PLUGIN_FLAG_UNLOAD_SAFE,
};
use acb_adapter_api::{
    AdapterMessage, CoreMessage, WireAccessRequest, WireClockRequest, WireConnection,
    WireDeliveryRequest, WireError, WireInstanceConfig, WireManifest, WireReceiveRequest,
    WireResult, WireSendRequest, WireTurnErrorAction, WireTurnErrorRequest, WireWakeRequest,
    decode_line, encode_line,
};
use acb_kernel::wire::{call_error, convert_catalog, send_error};
use acb_kernel::{
    AccessDecision, AccessPolicy, AccessProvider, AccessRequest, AdapterError, AdapterFactory,
    AdapterHost, AdapterInstance, AdapterManifest, CallError, CallOutcome, Capability,
    ClockContext, ClockPolicy, ClockProvider, Connection, DefaultTurnErrorProvider,
    DeliveryComposer, DeliveryPlan, DeliveryPolicy, DeliveryRequest, Element, EventQueue,
    ExtensionPoint, Health, LoaderKind, LoaderManifest, ManagerManifest, ManagerPlugin, MethodSpec,
    Outbound, OutboundError, OutgoingRecord, PLUGIN_API_VERSION, PluginConfig, PluginError,
    PluginInfo, PluginLifecycle, PluginLoader, PluginManager, PluginMetadata, PluginSource,
    ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, ReceiveDecision,
    ReceivePolicy, ReceiveProvider, ReceiveRequest, RoomId, SendContext, SendHook, Sent,
    TurnErrorAction, TurnErrorPolicy, TurnErrorProvider, TurnErrorRequest, WakeContext,
    WakeDecision, WakePolicy, WakeProvider, WakeVerdict, default_delivery_plan, validate_manifest,
};
use libloading::Library;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

/// 动态库的一次发送/调用最多等这么久。超时只保证 core 不被卡死；插件线程本身
/// 无法安全打断，超时后会把该 handle 视为不可用（后续调用继续超时，重启桥即可）。
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// ABI v2 的独立装载器：只接受声明 `UNLOAD_SAFE` 的插件，调用方显式
/// `destroy` 全部 handle 后可以 `unload`（dlclose）。
///
/// 这是 ADR-0030 的第一层：ABI 和 loader 级卸载路径。Root 的安全点调度尚未接入，
/// 因此 v2 目前不注册进 PluginManager；接入时复用同一张函数表。
pub struct UnloadSafeLibrary {
    library: Library,
    table: AcbPluginV2,
    path: PathBuf,
}

impl UnloadSafeLibrary {
    pub fn load(path: &str) -> Result<Self, String> {
        let library = unsafe { Library::new(path) }
            .map_err(|error| acb_i18n::t!("adapter.dynamic.open_library_failed", error = error))?;
        let entry: libloading::Symbol<'_, EntryFnV2> = unsafe { library.get(ENTRY_SYMBOL_V2) }
            .map_err(|error| {
                acb_i18n::t!(
                    "adapter.dynamic.missing_export",
                    symbol = String::from_utf8_lossy(ENTRY_SYMBOL_V2),
                    error = error
                )
            })?;
        let table_ptr = unsafe { entry() };
        if table_ptr.is_null() {
            return Err(acb_i18n::t!("adapter.dynamic.empty_table"));
        }
        let table = unsafe { *table_ptr };
        if table.abi_version != ABI_VERSION_V2 {
            return Err(acb_i18n::t!(
                "adapter.dynamic.abi_incompatible",
                version = table.abi_version,
                required = ABI_VERSION_V2
            ));
        }
        let required_size = std::mem::size_of::<AcbPluginV2>() as u32;
        if table.struct_size < required_size {
            return Err(acb_i18n::t!(
                "adapter.dynamic.v2_struct_too_small",
                size = table.struct_size,
                required = required_size
            ));
        }
        if table.flags & PLUGIN_FLAG_UNLOAD_SAFE == 0 {
            return Err(acb_i18n::t!(
                "adapter.dynamic.v2_unload_safe_required",
                path = path
            ));
        }
        if table.manifest.is_none()
            || table.create.is_none()
            || table.invoke.is_none()
            || table.destroy.is_none()
            || table.free_string.is_none()
        {
            return Err(acb_i18n::t!("adapter.dynamic.incomplete_table"));
        }
        Ok(Self {
            library,
            table,
            path: PathBuf::from(path),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn manifest(&self) -> Result<String, String> {
        let function = self
            .table
            .manifest
            .ok_or_else(|| acb_i18n::t!("adapter.dynamic.incomplete_table"))?;
        let pointer = unsafe { function() };
        take_c_string(&self.table, pointer, "manifest")
    }

    pub fn create(&self, config_json: &str, host: &HostV1) -> Result<*mut c_void, String> {
        let function = self
            .table
            .create
            .ok_or_else(|| acb_i18n::t!("adapter.dynamic.incomplete_table"))?;
        let config = CString::new(config_json).map_err(|error| {
            acb_i18n::t!(
                "adapter.dynamic.v2_call_failed",
                call = "create",
                error = error
            )
        })?;
        let handle = unsafe { function(config.as_ptr(), host) };
        if handle.is_null() {
            return Err(acb_i18n::t!(
                "adapter.dynamic.v2_call_failed",
                call = "create",
                error = "null handle"
            ));
        }
        Ok(handle)
    }

    /// # Safety
    ///
    /// `handle` 必须来自本 loader 的 `create` 且尚未 `destroy`；调用方要保证同一
    /// handle 不被并发使用。
    pub unsafe fn invoke(&self, handle: *mut c_void, request_json: &str) -> Result<String, String> {
        let function = self
            .table
            .invoke
            .ok_or_else(|| acb_i18n::t!("adapter.dynamic.incomplete_table"))?;
        let request = CString::new(request_json).map_err(|error| {
            acb_i18n::t!(
                "adapter.dynamic.v2_call_failed",
                call = "invoke",
                error = error
            )
        })?;
        let pointer = unsafe { function(handle, request.as_ptr()) };
        take_c_string(&self.table, pointer, "invoke")
    }

    /// # Safety
    ///
    /// `handle` 必须来自本 loader 的 `create`，且只能 `destroy` 一次；调用后该 handle
    /// 不得再被使用。
    pub unsafe fn destroy(&self, handle: *mut c_void) {
        if let Some(function) = self.table.destroy {
            unsafe { function(handle) };
        }
    }

    /// 调用方必须先 destroy 所有 handle；之后丢弃 Library 触发 dlclose。
    pub fn unload(self) -> Result<(), String> {
        drop(self.library);
        Ok(())
    }
}

fn take_c_string(table: &AcbPluginV2, pointer: *mut c_char, call: &str) -> Result<String, String> {
    if pointer.is_null() {
        return Err(acb_i18n::t!(
            "adapter.dynamic.v2_call_failed",
            call = call,
            error = "null string"
        ));
    }
    let text = unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned();
    if let Some(free) = table.free_string {
        unsafe { free(pointer) };
    }
    Ok(text)
}

/// 动态库 loader：把配置里声明的 `.so` 装进 Root。
#[derive(Default)]
pub struct DynamicLoader {
    plugins: Vec<PluginConfig>,
    /// 只登记声明 UNLOAD_SAFE 的 v2 策略插件：provider id -> 库句柄。
    unloadable: Mutex<HashMap<ProviderId, Arc<Library>>>,
    /// 可卸载策略插件在配置里的路径，用于卸载后重新加载。
    known: Mutex<HashMap<ProviderId, PathBuf>>,
    /// 已卸载的插件快照；Root 目录据此继续展示「已卸载」。
    unloaded: Mutex<HashMap<ProviderId, PluginInfo>>,
}

impl DynamicLoader {
    pub fn new(plugins: &[PluginConfig]) -> Self {
        Self {
            plugins: plugins.to_vec(),
            unloadable: Mutex::new(HashMap::new()),
            known: Mutex::new(HashMap::new()),
            unloaded: Mutex::new(HashMap::new()),
        }
    }

    /// 把已经 dlopen 的插件登记进 Root，并返回需要保留的库句柄（仅 UNLOAD_SAFE 策略）。
    fn install_loaded(
        manager: &mut PluginManager,
        loaded: LoadedPlugin,
    ) -> Result<Option<(ProviderId, Arc<Library>)>, PluginError> {
        let LoadedPlugin {
            inner,
            provider_id,
            library,
            unload_safe,
        } = loaded;
        match inner {
            LoadedKind::Adapter(factory) => manager
                .register_adapter_with_source(Arc::new(factory), PluginSource::Dynamic)
                .map_err(|error| PluginError::Loader(error.to_string()))?,
            LoadedKind::Wake(wake) => manager
                .install_manager(Arc::new(wake))
                .map_err(|error| PluginError::Loader(error.to_string()))?,
            LoadedKind::Decision(decision) => manager
                .install_manager(Arc::new(decision))
                .map_err(|error| PluginError::Loader(error.to_string()))?,
        }
        Ok(if unload_safe {
            provider_id.map(|id| (id, library))
        } else {
            None
        })
    }
}

impl PluginLoader for DynamicLoader {
    fn manifest(&self) -> LoaderManifest {
        LoaderManifest::new(
            LoaderKind::Dynamic,
            PLUGIN_API_VERSION,
            vec![
                ExtensionPoint::PlatformAdapter,
                ExtensionPoint::Wake,
                ExtensionPoint::Access,
                ExtensionPoint::Delivery,
                ExtensionPoint::Send,
                ExtensionPoint::TurnError,
                ExtensionPoint::Receive,
                ExtensionPoint::Clock,
            ],
        )
    }

    fn install(&self, manager: &mut PluginManager) -> Result<(), PluginError> {
        for plugin in &self.plugins {
            let loaded =
                LoadedPlugin::load(&plugin.path, plugin.kind.as_deref()).map_err(|detail| {
                    PluginError::Loader(acb_i18n::t!(
                        "adapter.dynamic.load_failed",
                        path = plugin.path,
                        detail = detail
                    ))
                })?;
            if let Some((id, library)) = Self::install_loaded(manager, loaded)? {
                self.known
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id.clone(), PathBuf::from(&plugin.path));
                self.unloadable
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(id, library);
            }
        }
        Ok(())
    }

    fn unload(&self, manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        let library = {
            let unloadable = self
                .unloadable
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            unloadable.get(id).cloned().ok_or_else(|| {
                PluginError::Loader(acb_i18n::t!("adapter.dynamic.unload_unknown", id = id))
            })?
        };
        let mut info = manager.info(id).ok_or_else(|| PluginError::Provider {
            id: id.to_string(),
            detail: acb_i18n::t!("kernel.plugin.unknown_provider", id = id),
        })?;
        manager.uninstall_manager(id.namespace())?;
        self.unloadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        info.lifecycle = PluginLifecycle::Unloaded;
        self.unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), info);
        // manager 已丢弃，剩下的库引用在这里释放，触发 dlclose。
        drop(library);
        Ok(())
    }

    fn can_unload(&self, id: &ProviderId) -> bool {
        self.known
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(id)
    }

    fn inactive(&self) -> Vec<PluginInfo> {
        self.unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    /// 注意：ADR-0030 D3 的“回滚到上一份已加载实现”在当前同路径 reload 下不成立。
    /// 调用方在 reload 前已经 unload，旧库已经 `dlclose`；这里只重新加载路径，
    /// 失败时插件保持 `unloaded` 并留下 `lastError`，不会恢复旧版本。
    /// 候选预加载/回滚方案见 `docs/plan/native-reload-rollback-options.md`。
    fn reload(&self, manager: &mut PluginManager, id: &ProviderId) -> Result<(), PluginError> {
        let path = self
            .known
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| {
                PluginError::Loader(acb_i18n::t!("adapter.dynamic.reload_unknown", id = id))
            })?;
        if self
            .unloadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(id)
        {
            return Err(PluginError::Loader(acb_i18n::t!(
                "adapter.dynamic.reload_loaded",
                id = id
            )));
        }

        let path_text = path.to_string_lossy().into_owned();
        let loaded = LoadedPlugin::load(&path_text, None).map_err(|detail| {
            PluginError::Loader(acb_i18n::t!(
                "adapter.dynamic.reload_failed",
                path = path_text,
                detail = detail
            ))
        })?;
        let found = loaded
            .provider_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "-".to_owned());
        if loaded.provider_id.as_ref() != Some(id) {
            return Err(PluginError::Loader(acb_i18n::t!(
                "adapter.dynamic.reload_id_mismatch",
                expected = id,
                found = found
            )));
        }
        if !loaded.unload_safe {
            return Err(PluginError::Loader(acb_i18n::t!(
                "adapter.dynamic.reload_not_unload_safe",
                id = id
            )));
        }
        let Some((_, library)) = Self::install_loaded(manager, loaded)? else {
            return Err(PluginError::Loader(acb_i18n::t!(
                "adapter.dynamic.reload_not_unload_safe",
                id = id
            )));
        };
        self.unloadable
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), library);
        self.unloaded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
        Ok(())
    }
}

struct LoadedPlugin {
    inner: LoadedKind,
    provider_id: Option<ProviderId>,
    library: Arc<Library>,
    unload_safe: bool,
}

enum LoadedKind {
    Adapter(DylibAdapterFactory),
    Wake(DylibWakeManager),
    Decision(DylibDecisionManager),
}

impl LoadedPlugin {
    fn load(path: &str, expected_kind: Option<&str>) -> Result<Self, String> {
        let library = unsafe { Library::new(path) }
            .map_err(|error| acb_i18n::t!("adapter.dynamic.open_library_failed", error = error))?;
        let (table, unload_safe) = read_abi_table(&library)?;
        let library = Arc::new(library);
        let wire = read_manifest(&table)?;
        match wire.extension.as_str() {
            "platform_adapter" => {
                let manifest = AdapterManifest::try_from(wire).map_err(|detail| {
                    acb_i18n::t!("adapter.dynamic.invalid_manifest", detail = detail)
                })?;
                validate_manifest(&manifest).map_err(|error| error.to_string())?;
                if let Some(expected) = expected_kind {
                    if manifest.kind.as_ref() != expected {
                        return Err(acb_i18n::t!(
                            "adapter.dynamic.manifest_kind_mismatch",
                            kind = manifest.kind,
                            expected = expected
                        ));
                    }
                }
                Ok(Self {
                    inner: LoadedKind::Adapter(DylibAdapterFactory {
                        _library: library.clone(),
                        table,
                        manifest,
                    }),
                    provider_id: None,
                    library,
                    // 适配器卸载还没实现；即使声明 v2 也按常驻处理。
                    unload_safe: false,
                })
            }
            "wake" => {
                let provider = ProviderId::parse(&wire.id).map_err(|error| {
                    acb_i18n::t!(
                        "adapter.dynamic.invalid_provider_id",
                        extension = "wake",
                        error = error
                    )
                })?;
                if provider.namespace() == "acb" {
                    return Err(acb_i18n::t!("adapter.dynamic.acb_namespace"));
                }
                if wire.api_version != PLUGIN_API_VERSION {
                    return Err(acb_i18n::t!(
                        "adapter.dynamic.api_incompatible",
                        version = wire.api_version,
                        required = PLUGIN_API_VERSION
                    ));
                }
                let metadata = PluginMetadata::try_from(wire.metadata).map_err(|detail| {
                    acb_i18n::t!("adapter.dynamic.invalid_metadata", detail = detail)
                })?;
                if metadata
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err(acb_i18n::t!("adapter.dynamic.wake_summary_required"));
                }
                if metadata.effects.is_empty() {
                    return Err(acb_i18n::t!("adapter.dynamic.wake_effects_required"));
                }
                Ok(Self {
                    inner: LoadedKind::Wake(DylibWakeManager::new(
                        library.clone(),
                        table,
                        provider.clone(),
                        wire.platform,
                        wire.id,
                        metadata,
                    )),
                    provider_id: Some(provider),
                    library,
                    unload_safe,
                })
            }
            "access" | "delivery" | "send" | "turn_error" | "receive" | "clock" => {
                let provider = ProviderId::parse(&wire.id).map_err(|error| {
                    acb_i18n::t!(
                        "adapter.dynamic.invalid_provider_id",
                        extension = wire.extension,
                        error = error
                    )
                })?;
                if provider.namespace() == "acb" {
                    return Err(acb_i18n::t!("adapter.dynamic.acb_namespace"));
                }
                if wire.api_version != PLUGIN_API_VERSION {
                    return Err(acb_i18n::t!(
                        "adapter.dynamic.api_incompatible",
                        version = wire.api_version,
                        required = PLUGIN_API_VERSION
                    ));
                }
                let metadata = PluginMetadata::try_from(wire.metadata).map_err(|detail| {
                    acb_i18n::t!("adapter.dynamic.invalid_metadata", detail = detail)
                })?;
                if metadata
                    .summary
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    return Err(acb_i18n::t!(
                        "adapter.dynamic.provider_summary_required",
                        extension = wire.extension
                    ));
                }
                let point = match wire.extension.as_str() {
                    "access" => ExtensionPoint::Access,
                    "delivery" => ExtensionPoint::Delivery,
                    "send" => ExtensionPoint::Send,
                    "turn_error" => ExtensionPoint::TurnError,
                    "receive" => ExtensionPoint::Receive,
                    "clock" => ExtensionPoint::Clock,
                    _ => unreachable!("already routed by extension point"),
                };
                Ok(Self {
                    inner: LoadedKind::Decision(DylibDecisionManager::new(
                        library.clone(),
                        table,
                        provider.clone(),
                        point,
                        wire.id,
                        metadata,
                    )),
                    provider_id: Some(provider),
                    library,
                    unload_safe,
                })
            }
            other => Err(acb_i18n::t!(
                "adapter.dynamic.unknown_extension",
                extension = other
            )),
        }
    }
}

fn read_abi_table(library: &Library) -> Result<(AdapterV1, bool), String> {
    if let Ok(entry) = unsafe { library.get::<EntryFnV2>(ENTRY_SYMBOL_V2) } {
        let table_ptr = unsafe { entry() };
        if table_ptr.is_null() {
            return Err(acb_i18n::t!("adapter.dynamic.empty_table"));
        }
        let table = unsafe { *table_ptr };
        if table.abi_version != ABI_VERSION_V2 {
            return Err(acb_i18n::t!(
                "adapter.dynamic.abi_incompatible",
                version = table.abi_version,
                required = ABI_VERSION_V2
            ));
        }
        let required = std::mem::size_of::<AcbPluginV2>() as u32;
        if table.struct_size < required {
            return Err(acb_i18n::t!(
                "adapter.dynamic.v2_struct_too_small",
                size = table.struct_size,
                required = required
            ));
        }
        if table.manifest.is_none()
            || table.create.is_none()
            || table.invoke.is_none()
            || table.destroy.is_none()
            || table.free_string.is_none()
        {
            return Err(acb_i18n::t!("adapter.dynamic.incomplete_table"));
        }
        return Ok((
            AdapterV1 {
                abi_version: ABI_VERSION,
                manifest: table.manifest,
                create: table.create,
                invoke: table.invoke,
                destroy: table.destroy,
                free_string: table.free_string,
            },
            table.flags & PLUGIN_FLAG_UNLOAD_SAFE != 0,
        ));
    }

    let entry: libloading::Symbol<'_, EntryFn> =
        unsafe { library.get(ENTRY_SYMBOL) }.map_err(|error| {
            acb_i18n::t!(
                "adapter.dynamic.missing_export",
                symbol = String::from_utf8_lossy(ENTRY_SYMBOL),
                error = error
            )
        })?;
    let table_ptr = unsafe { entry() };
    if table_ptr.is_null() {
        return Err(acb_i18n::t!("adapter.dynamic.empty_table"));
    }
    let table = unsafe { *table_ptr };
    if table.abi_version != ABI_VERSION {
        return Err(acb_i18n::t!(
            "adapter.dynamic.abi_incompatible",
            version = table.abi_version,
            required = ABI_VERSION
        ));
    }
    if table.manifest.is_none()
        || table.create.is_none()
        || table.invoke.is_none()
        || table.destroy.is_none()
        || table.free_string.is_none()
    {
        return Err(acb_i18n::t!("adapter.dynamic.incomplete_table"));
    }
    Ok((table, false))
}

struct DylibAdapterFactory {
    /// 库必须活到进程退出：函数指针都指向它。
    _library: Arc<Library>,
    table: AdapterV1,
    manifest: AdapterManifest,
}

fn read_manifest(table: &AdapterV1) -> Result<WireManifest, String> {
    let manifest_fn = table
        .manifest
        .ok_or(acb_i18n::t!("adapter.dynamic.missing_manifest_fn"))?;
    let pointer = unsafe { manifest_fn() };
    let json = take_owned_string(pointer, table)?;
    serde_json::from_str(&json)
        .map_err(|error| acb_i18n::t!("adapter.dynamic.manifest_invalid_json", error = error))
}

impl AdapterFactory for DylibAdapterFactory {
    fn manifest(&self) -> AdapterManifest {
        self.manifest.clone()
    }

    fn create(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, AdapterError> {
        self.create_instance(connection, events)
            .map_err(|detail| AdapterError::Create(connection.id.clone(), detail))
    }
}

impl DylibAdapterFactory {
    fn create_instance(
        &self,
        connection: &Connection,
        events: EventQueue,
    ) -> Result<AdapterInstance, String> {
        let context = Arc::new(HostContext {
            events,
            stop: Arc::new(AtomicBool::new(false)),
            connection: connection.id.clone(),
        });
        let host = HostV1 {
            abi_version: ABI_VERSION,
            ctx: Arc::as_ptr(&context) as *mut c_void,
            submit_event: Some(submit_event),
            should_stop: Some(should_stop),
            log: Some(log),
        };

        let config = WireInstanceConfig::Adapter {
            connection: WireConnection::from(connection),
        };
        let config_json = CString::new(encode_line(&config).map_err(|error| {
            acb_i18n::t!("adapter.dynamic.serialize_connection_failed", error = error)
        })?)
        .map_err(|_| acb_i18n::t!("adapter.dynamic.connection_config_nul"))?;

        let create = self
            .table
            .create
            .ok_or(acb_i18n::t!("adapter.dynamic.missing_create_fn"))?;
        let handle_ptr = unsafe { create(config_json.as_ptr(), &host) };
        if handle_ptr.is_null() {
            return Err(acb_i18n::t!("adapter.dynamic.empty_handle"));
        }
        let handle = Arc::new(Mutex::new(AdapterHandle {
            ptr: handle_ptr,
            alive: true,
        }));

        let catalog = match describe(&self.table, &handle) {
            Ok(catalog) => catalog,
            Err(detail) => {
                destroy_handle(&self.table, &handle);
                return Err(acb_i18n::t!(
                    "adapter.dynamic.read_catalog_failed",
                    detail = detail
                ));
            }
        };

        let outbound: Box<dyn Outbound> = Box::new(DylibOutbound {
            _library: Arc::clone(&self._library),
            _context: Arc::clone(&context),
            table: self.table,
            handle: Arc::clone(&handle),
            connection: connection.id.clone(),
            catalog,
            seq: 0,
        });

        let table = self.table;
        let stop = Arc::clone(&context.stop);
        let connection_id = connection.id.clone();
        let launch: Box<dyn FnOnce(AdapterHost) + Send> = Box::new(move |host| {
            monitor(host.shutdown, stop, table, handle, context, connection_id);
        });

        Ok(AdapterInstance {
            outbound,
            launch,
            manifest: self.manifest.clone(),
        })
    }
}

fn describe(
    table: &AdapterV1,
    handle: &Arc<Mutex<AdapterHandle>>,
) -> Result<Arc<[MethodSpec]>, String> {
    let reply = invoke(table, handle, &CoreMessage::Describe { id: 1 })?;
    match reply {
        AdapterMessage::Reply {
            result: WireResult::Catalog { catalog },
            ..
        } => convert_catalog(catalog)
            .map_err(|detail| acb_i18n::t!("adapter.dynamic.catalog_invalid", detail = detail)),
        AdapterMessage::Reply {
            result: WireResult::Error { error },
            ..
        } => Err(acb_i18n::t!(
            "adapter.dynamic.describe_rejected",
            error = format!("{error:?}")
        )),
        other => Err(acb_i18n::t!(
            "adapter.dynamic.bad_describe_reply",
            reply = format!("{other:?}")
        )),
    }
}

/// 动态库 wake provider 的 manager。一个插件一个子树，namespace 来自 provider id。
struct DylibWakeManager {
    library: Arc<Library>,
    table: AdapterV1,
    provider: ProviderId,
    platform: String,
    plugin_id: String,
    metadata: PluginMetadata,
    cached: Mutex<Option<Arc<DylibWakeProvider>>>,
}

impl DylibWakeManager {
    fn new(
        library: Arc<Library>,
        table: AdapterV1,
        provider: ProviderId,
        platform: String,
        plugin_id: String,
        metadata: PluginMetadata,
    ) -> Self {
        Self {
            library,
            table,
            provider,
            platform,
            plugin_id,
            metadata,
            cached: Mutex::new(None),
        }
    }
}

impl ManagerPlugin for DylibWakeManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        let ProviderOptions::Wake(policy) = options else {
            return Err(PluginError::Provider {
                id: id.to_string(),
                detail: acb_i18n::t!("adapter.dynamic.wake_policy_required"),
            });
        };

        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(provider) = cached.as_ref() {
            let provider: Arc<dyn WakeProvider> = provider.clone();
            return Ok(ProviderInstance::Wake(provider));
        }
        let provider = DylibWakeProvider::create(
            Arc::clone(&self.library),
            self.table,
            &self.plugin_id,
            &self.platform,
            policy,
        )
        .map_err(|detail| PluginError::Provider {
            id: id.to_string(),
            detail,
        })?;
        let provider = Arc::new(provider);
        *cached = Some(Arc::clone(&provider));
        let dynamic: Arc<dyn WakeProvider> = provider;
        Ok(ProviderInstance::Wake(dynamic))
    }

    fn shutdown(&self) {
        let provider = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(provider) = provider {
            provider.destroy();
        }
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

struct DylibWakeProvider {
    _library: Arc<Library>,
    _context: Arc<HostContext>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    seq: AtomicU64,
    failures: AtomicU64,
}

impl DylibWakeProvider {
    fn create(
        library: Arc<Library>,
        table: AdapterV1,
        plugin_id: &str,
        platform: &str,
        policy: &WakePolicy,
    ) -> Result<Self, String> {
        // wake 判定不需要入站事件；给一个空队列，插件误发事件时会被记录为队列关闭。
        let (events, _receiver) = EventQueue::channel();
        let context = Arc::new(HostContext {
            events,
            stop: Arc::new(AtomicBool::new(false)),
            connection: plugin_id.to_owned(),
        });
        let host = HostV1 {
            abi_version: ABI_VERSION,
            ctx: Arc::as_ptr(&context) as *mut c_void,
            submit_event: Some(submit_event),
            should_stop: Some(should_stop),
            log: Some(log),
        };
        let config = WireInstanceConfig::Provider {
            provider: plugin_id.to_owned(),
            extension: "wake".into(),
            options: wake_options(policy),
        };
        let config_json = CString::new(encode_line(&config).map_err(|error| {
            acb_i18n::t!("adapter.dynamic.serialize_wake_failed", error = error)
        })?)
        .map_err(|_| acb_i18n::t!("adapter.dynamic.wake_config_nul"))?;

        let create = table
            .create
            .ok_or(acb_i18n::t!("adapter.dynamic.missing_create_fn"))?;
        let handle_ptr = unsafe { create(config_json.as_ptr(), &host) };
        if handle_ptr.is_null() {
            return Err(acb_i18n::t!(
                "adapter.dynamic.wake_empty_handle",
                plugin_id = plugin_id,
                platform = platform
            ));
        }

        Ok(Self {
            _library: library,
            _context: context,
            table,
            handle: Arc::new(Mutex::new(AdapterHandle {
                ptr: handle_ptr,
                alive: true,
            })),
            seq: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        })
    }

    fn note_failure(&self, detail: &str) {
        let count = self.failures.fetch_add(1, Ordering::Relaxed);
        if count < 5 || count % 100 == 0 {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.dynamic.wake_failed",
                    attempt = count + 1,
                    detail = detail
                )
            );
        }
    }

    fn destroy(&self) {
        destroy_handle(&self.table, &self.handle);
    }
}

impl WakeProvider for DylibWakeProvider {
    fn decide(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeDecision {
        self.decide_with_reason(options, context).decision
    }

    fn decide_with_reason(&self, options: &WakePolicy, context: &WakeContext<'_>) -> WakeVerdict {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        let request = WireWakeRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            connection: context.connection.to_owned(),
            platform: context.platform.to_owned(),
            sender_id: context.sender_id.to_owned(),
            text: context.text.to_owned(),
            at_self: context.at_self,
            reply_to_self: context.reply_to_self,
            has_body: context.has_body,
            event: context.event.map(str::to_owned),
            tags: context.tags.iter().map(|tag| (*tag).to_owned()).collect(),
            unread: context.unread,
            last_wake_at: context.last_wake_at,
            now: context.now,
        };
        let request = serde_json::to_value(request).unwrap_or(Value::Null);
        let message = CoreMessage::Decide {
            id,
            point: "wake".into(),
            options: wake_options(options),
            request,
        };

        match invoke(&self.table, &self.handle, &message) {
            Ok(AdapterMessage::Reply {
                result:
                    WireResult::Decision {
                        decision,
                        reason,
                        delay_ms,
                    },
                ..
            }) => match decision.as_str() {
                "wake" => WakeVerdict {
                    decision: WakeDecision::Wake,
                    reason,
                    delay_ms,
                },
                "stay" => WakeVerdict::stay(),
                other => {
                    self.note_failure(&acb_i18n::t!(
                        "adapter.dynamic.unknown_wake_decision",
                        decision = other
                    ));
                    WakeVerdict::stay()
                }
            },
            Ok(other) => {
                self.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.not_wake_reply",
                    reply = format!("{other:?}")
                ));
                WakeVerdict::stay()
            }
            Err(detail) => {
                self.note_failure(&detail);
                WakeVerdict::stay()
            }
        }
    }
}

fn wake_options(policy: &WakePolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "at_mention": policy.at_mention,
        "reply": policy.reply,
        "name_mention": policy.name_mention,
        "names": policy.names,
        "accumulate": policy.accumulate,
        "event_tags": policy.event_tags,
        "events": policy.events,
        "keywords": policy.keywords,
        "ignore_speakers": policy.ignore_speakers,
        "extra": policy.options,
    })
}

/// 动态库决策型策略 provider 的 manager（access / receive / clock）。
///
/// 和 wake 一样：一个插件一个子树，namespace 来自 provider id；判定在 core 的同步路径里，
/// 所以只开放动态库载体（外置进程会在 core 锁内做 I/O）。
struct DylibDecisionManager {
    library: Arc<Library>,
    table: AdapterV1,
    provider: ProviderId,
    point: ExtensionPoint,
    plugin_id: String,
    metadata: PluginMetadata,
    cached: Mutex<Option<CachedDecision>>,
}

enum CachedDecision {
    Access(Arc<DylibAccessProvider>),
    Delivery(Arc<DylibDeliveryComposer>),
    Send(Arc<DylibSendHook>),
    TurnError(Arc<DylibTurnErrorProvider>),
    Receive(Arc<DylibReceiveProvider>),
    Clock(Arc<DylibClockProvider>),
}

impl CachedDecision {
    fn instance(&self) -> ProviderInstance {
        match self {
            Self::Access(provider) => ProviderInstance::Access(provider.clone()),
            Self::Delivery(provider) => ProviderInstance::Delivery(provider.clone()),
            Self::Send(provider) => ProviderInstance::Send(provider.clone()),
            Self::TurnError(provider) => ProviderInstance::TurnError(provider.clone()),
            Self::Receive(provider) => ProviderInstance::Receive(provider.clone()),
            Self::Clock(provider) => ProviderInstance::Clock(provider.clone()),
        }
    }

    fn destroy(&self) {
        match self {
            Self::Access(provider) => provider.channel.destroy(),
            Self::Delivery(provider) => provider.channel.destroy(),
            Self::Send(provider) => provider.channel.destroy(),
            Self::TurnError(provider) => provider.channel.destroy(),
            Self::Receive(provider) => provider.channel.destroy(),
            Self::Clock(provider) => provider.channel.destroy(),
        }
    }
}

impl DylibDecisionManager {
    fn new(
        library: Arc<Library>,
        table: AdapterV1,
        provider: ProviderId,
        point: ExtensionPoint,
        plugin_id: String,
        metadata: PluginMetadata,
    ) -> Self {
        Self {
            library,
            table,
            provider,
            point,
            plugin_id,
            metadata,
            cached: Mutex::new(None),
        }
    }

    fn capability(&self) -> Capability {
        match self.point {
            ExtensionPoint::Access => Capability::AccessDecision,
            ExtensionPoint::Delivery => Capability::DeliverPlan,
            ExtensionPoint::Send => Capability::SendHook,
            ExtensionPoint::TurnError => Capability::TurnErrorDecision,
            ExtensionPoint::Receive => Capability::ReceiveDecision,
            ExtensionPoint::Clock => Capability::ClockDecision,
            _ => Capability::AccessDecision,
        }
    }
}

impl ManagerPlugin for DylibDecisionManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            self.provider.namespace(),
            PLUGIN_API_VERSION,
            vec![self.capability()],
        )
        .with_metadata(self.metadata.clone())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                self.provider.clone(),
                self.point,
                PLUGIN_API_VERSION,
                vec![self.capability()],
            )
            .with_metadata(self.metadata.clone())
            .with_source(PluginSource::Dynamic),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id != &self.provider {
            return Err(PluginError::UnknownProvider(id.to_string()));
        }
        let (extension, options_json) = match (self.point, options) {
            (ExtensionPoint::Access, ProviderOptions::Access(policy)) => {
                ("access", access_options(policy))
            }
            (ExtensionPoint::Delivery, ProviderOptions::Delivery(policy)) => {
                ("delivery", delivery_options(policy))
            }
            (ExtensionPoint::Send, ProviderOptions::None) => ("send", json!({})),
            (ExtensionPoint::TurnError, ProviderOptions::TurnError(policy)) => {
                ("turn_error", turn_error_options(policy))
            }
            (ExtensionPoint::Receive, ProviderOptions::Receive(policy)) => {
                ("receive", receive_options(policy))
            }
            (ExtensionPoint::Clock, ProviderOptions::Clock(policy)) => {
                ("clock", clock_options(policy))
            }
            (point, _) => {
                return Err(PluginError::Provider {
                    id: id.to_string(),
                    detail: acb_i18n::t!(
                        "adapter.dynamic.provider_options_wrong_type",
                        point = format!("{point:?}")
                    ),
                });
            }
        };

        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(provider) = cached.as_ref() {
            return Ok(provider.instance());
        }

        let channel = Arc::new(
            DecisionChannel::create(
                Arc::clone(&self.library),
                self.table,
                &self.plugin_id,
                extension,
                options_json.clone(),
            )
            .map_err(|detail| PluginError::Provider {
                id: id.to_string(),
                detail,
            })?,
        );
        let provider = match extension {
            "access" => CachedDecision::Access(Arc::new(DylibAccessProvider {
                channel: Arc::clone(&channel),
            })),
            "delivery" => CachedDecision::Delivery(Arc::new(DylibDeliveryComposer {
                channel: Arc::clone(&channel),
            })),
            "send" => CachedDecision::Send(Arc::new(DylibSendHook {
                channel: Arc::clone(&channel),
                provider: id.to_string(),
            })),
            "turn_error" => CachedDecision::TurnError(Arc::new(DylibTurnErrorProvider {
                channel: Arc::clone(&channel),
            })),
            "receive" => CachedDecision::Receive(Arc::new(DylibReceiveProvider {
                channel: Arc::clone(&channel),
            })),
            "clock" => CachedDecision::Clock(Arc::new(DylibClockProvider {
                channel: Arc::clone(&channel),
            })),
            other => {
                return Err(PluginError::Provider {
                    id: id.to_string(),
                    detail: acb_i18n::t!("adapter.dynamic.unknown_decision_point", point = other),
                });
            }
        };
        let instance = provider.instance();
        *cached = Some(provider);
        Ok(instance)
    }

    fn shutdown(&self) {
        let provider = self
            .cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(provider) = provider {
            provider.destroy();
        }
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

/// 决策型 provider 的共用通道：一个 handle + 一串请求号。
struct DecisionChannel {
    _library: Arc<Library>,
    _context: Arc<HostContext>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    seq: AtomicU64,
    failures: AtomicU64,
    label: String,
}

impl DecisionChannel {
    fn create(
        library: Arc<Library>,
        table: AdapterV1,
        plugin_id: &str,
        extension: &str,
        options: Value,
    ) -> Result<Self, String> {
        let (events, _receiver) = EventQueue::channel();
        let context = Arc::new(HostContext {
            events,
            stop: Arc::new(AtomicBool::new(false)),
            connection: plugin_id.to_owned(),
        });
        let host = HostV1 {
            abi_version: ABI_VERSION,
            ctx: Arc::as_ptr(&context) as *mut c_void,
            submit_event: Some(submit_event),
            should_stop: Some(should_stop),
            log: Some(log),
        };
        let config = WireInstanceConfig::Provider {
            provider: plugin_id.to_owned(),
            extension: extension.to_owned(),
            options,
        };
        let config_json = CString::new(encode_line(&config).map_err(|error| {
            acb_i18n::t!(
                "adapter.dynamic.serialize_config_failed",
                extension = extension,
                error = error
            )
        })?)
        .map_err(|_| acb_i18n::t!("adapter.dynamic.config_nul", extension = extension))?;

        let create = table
            .create
            .ok_or(acb_i18n::t!("adapter.dynamic.missing_create_fn"))?;
        let handle_ptr = unsafe { create(config_json.as_ptr(), &host) };
        if handle_ptr.is_null() {
            return Err(acb_i18n::t!(
                "adapter.dynamic.empty_handle_extension",
                extension = extension,
                plugin_id = plugin_id
            ));
        }

        Ok(Self {
            _library: library,
            _context: context,
            table,
            handle: Arc::new(Mutex::new(AdapterHandle {
                ptr: handle_ptr,
                alive: true,
            })),
            seq: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            label: extension.to_owned(),
        })
    }

    fn next(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn call(&self, message: &CoreMessage) -> Result<AdapterMessage, String> {
        invoke(&self.table, &self.handle, message)
    }

    fn note_failure(&self, detail: &str) {
        let count = self.failures.fetch_add(1, Ordering::Relaxed);
        if count < 5 || count % 100 == 0 {
            eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.dynamic.plugin_failed",
                    label = self.label,
                    attempt = count + 1,
                    detail = detail
                )
            );
        }
    }

    fn destroy(&self) {
        destroy_handle(&self.table, &self.handle);
    }
}

/// 发一次 Decide，取回 `decision` 字符串。回执形状不对或插件报错返回 `None`。
fn plugin_decision(channel: &DecisionChannel, message: CoreMessage) -> Option<String> {
    match channel.call(&message) {
        Ok(AdapterMessage::Reply {
            result: WireResult::Decision { decision, .. },
            ..
        }) => Some(decision),
        Ok(other) => {
            channel.note_failure(&acb_i18n::t!(
                "adapter.dynamic.not_decision_reply",
                reply = format!("{other:?}")
            ));
            None
        }
        Err(detail) => {
            channel.note_failure(&detail);
            None
        }
    }
}

struct DylibAccessProvider {
    channel: Arc<DecisionChannel>,
}

impl AccessProvider for DylibAccessProvider {
    fn check(&self, options: &AccessPolicy, request: &AccessRequest<'_>) -> AccessDecision {
        let wire = WireAccessRequest {
            session: request.session.to_owned(),
            connection: request.connection.to_owned(),
            room: request.room.map(|room| room.to_string()),
            origin_room: request.origin_room.map(|room| room.to_string()),
            action: request.action.as_str().to_owned(),
            subscriptions: request
                .subscriptions
                .iter()
                .map(|room| room.to_string())
                .collect(),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "access".into(),
            options: access_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match plugin_decision(&self.channel, message).as_deref() {
            Some("allow") => AccessDecision::Allow,
            Some("deny") => AccessDecision::Deny,
            Some(other) => {
                self.channel.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.unknown_access_decision",
                    decision = other
                ));
                AccessDecision::Deny
            }
            None => AccessDecision::Deny,
        }
    }
}

struct DylibDeliveryComposer {
    channel: Arc<DecisionChannel>,
}

impl DeliveryComposer for DylibDeliveryComposer {
    fn compose(&self, options: &DeliveryPolicy, request: &DeliveryRequest<'_>) -> DeliveryPlan {
        let wire = WireDeliveryRequest {
            session: request.session.to_owned(),
            room: request.room.to_string(),
            limit: request.limit,
            records: request.records.to_vec(),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "delivery".into(),
            options: delivery_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match self.channel.call(&message) {
            Ok(AdapterMessage::Reply {
                result: WireResult::Delivery { plan: Some(plan) },
                ..
            }) => DeliveryPlan {
                keep: plan.keep,
                omitted: plan.omitted,
            },
            // plan = null 与 Lua 的 nil 一样：明确要求用内置选批。
            Ok(AdapterMessage::Reply {
                result: WireResult::Delivery { plan: None },
                ..
            }) => default_delivery_plan(request),
            Ok(other) => {
                self.channel.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.not_delivery_plan",
                    reply = format!("{other:?}")
                ));
                default_delivery_plan(request)
            }
            Err(detail) => {
                self.channel.note_failure(&detail);
                default_delivery_plan(request)
            }
        }
    }
}

struct DylibTurnErrorProvider {
    channel: Arc<DecisionChannel>,
}

impl TurnErrorProvider for DylibTurnErrorProvider {
    fn decide(
        &self,
        policy: &TurnErrorPolicy,
        request: &TurnErrorRequest<'_>,
    ) -> Vec<TurnErrorAction> {
        let wire = WireTurnErrorRequest {
            session: request.session.to_owned(),
            room: request.room.to_string(),
            error: request.error.to_owned(),
            actions: request.actions.iter().map(wire_turn_error_action).collect(),
            default_actions: request
                .default_actions
                .iter()
                .map(wire_turn_error_action)
                .collect(),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "turn_error".into(),
            options: turn_error_options(policy),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match self.channel.call(&message) {
            Ok(AdapterMessage::Reply {
                result: WireResult::Actions { actions },
                ..
            }) => actions
                .into_iter()
                .filter_map(|action| parse_turn_error_action(&self.channel, action))
                .collect(),
            Ok(other) => {
                self.channel.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.not_turn_error_action",
                    reply = format!("{other:?}")
                ));
                DefaultTurnErrorProvider.decide(policy, request)
            }
            Err(detail) => {
                self.channel.note_failure(&detail);
                DefaultTurnErrorProvider.decide(policy, request)
            }
        }
    }
}

fn wire_turn_error_action(action: &TurnErrorAction) -> WireTurnErrorAction {
    match action {
        TurnErrorAction::Log => WireTurnErrorAction::Log,
        TurnErrorAction::Send { room } => WireTurnErrorAction::Send {
            room: room.to_string(),
        },
    }
}

fn parse_turn_error_action(
    channel: &DecisionChannel,
    action: WireTurnErrorAction,
) -> Option<TurnErrorAction> {
    match action {
        WireTurnErrorAction::Log => Some(TurnErrorAction::Log),
        WireTurnErrorAction::Send { room } => match RoomId::parse(&room) {
            Ok(room) => Some(TurnErrorAction::Send { room }),
            Err(error) => {
                channel.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.invalid_turn_error_room",
                    room = room,
                    error = error
                ));
                None
            }
        },
    }
}

struct DylibSendHook {
    channel: Arc<DecisionChannel>,
    provider: String,
}

impl SendHook for DylibSendHook {
    fn before_send(
        &self,
        context: &SendContext<'_>,
        elements: &mut Vec<Element>,
    ) -> Result<(), PluginError> {
        let wire = WireSendRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            phase: "before".into(),
            origin: context.origin.as_str().to_owned(),
            elements: elements.clone(),
            platform_id: None,
            at: None,
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "send".into(),
            options: json!({}),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match self.channel.call(&message) {
            Ok(AdapterMessage::Reply {
                result:
                    WireResult::Elements {
                        elements: replacement,
                    },
                ..
            }) => {
                *elements = replacement;
                Ok(())
            }
            // 插件只实现了 after 时，before 返回 keep 即可明确表示「不改」。
            Ok(AdapterMessage::Reply {
                result: WireResult::Decision { decision, .. },
                ..
            }) if decision == "keep" || decision == "ok" => Ok(()),
            Ok(AdapterMessage::Reply {
                result:
                    WireResult::Error {
                        error: WireError::Unsupported { .. },
                    },
                ..
            }) => Ok(()),
            Ok(AdapterMessage::Reply {
                result: WireResult::Error { error },
                ..
            }) => {
                let detail = acb_i18n::t!(
                    "adapter.dynamic.before_send_hook_error",
                    error = format!("{error:?}")
                );
                self.channel.note_failure(&detail);
                Err(PluginError::Provider {
                    id: self.provider.clone(),
                    detail,
                })
            }
            Ok(other) => {
                let detail = acb_i18n::t!(
                    "adapter.dynamic.bad_before_send_reply",
                    reply = format!("{other:?}")
                );
                self.channel.note_failure(&detail);
                Err(PluginError::Provider {
                    id: self.provider.clone(),
                    detail,
                })
            }
            Err(detail) => {
                self.channel.note_failure(&detail);
                Err(PluginError::Provider {
                    id: self.provider.clone(),
                    detail,
                })
            }
        }
    }

    fn after_send(&self, context: &SendContext<'_>, sent: &Sent) {
        let wire = WireSendRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            phase: "after".into(),
            origin: context.origin.as_str().to_owned(),
            elements: Vec::new(),
            platform_id: Some(sent.platform_id.clone()),
            at: Some(sent.at),
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "send".into(),
            options: json!({}),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        if let Err(detail) = self.channel.call(&message) {
            self.channel.note_failure(&detail);
        }
    }
}

struct DylibReceiveProvider {
    channel: Arc<DecisionChannel>,
}

impl ReceiveProvider for DylibReceiveProvider {
    fn decide(&self, options: &ReceivePolicy, request: &ReceiveRequest<'_>) -> ReceiveDecision {
        let wire = WireReceiveRequest {
            connection: request.connection.to_owned(),
            platform: request.platform.to_owned(),
            room: request.room.to_string(),
            sender: request.sender.clone(),
            kind: request.kind.clone(),
            elements: request.elements.to_vec(),
            text: request.text.to_owned(),
            at_self: request.at_self,
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "receive".into(),
            options: receive_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match plugin_decision(&self.channel, message).as_deref() {
            Some("record") => ReceiveDecision::Record,
            Some("drop") => ReceiveDecision::Drop,
            Some(other) => {
                self.channel.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.unknown_receive_decision",
                    decision = other
                ));
                ReceiveDecision::Record
            }
            // 出错按「接收」兜底：宁可多记一条，也不误丢消息。
            None => ReceiveDecision::Record,
        }
    }
}

struct DylibClockProvider {
    channel: Arc<DecisionChannel>,
}

impl ClockProvider for DylibClockProvider {
    fn is_due(&self, options: &ClockPolicy, context: &ClockContext<'_>) -> bool {
        let wire = WireClockRequest {
            session: context.session.to_owned(),
            room: context.room.to_string(),
            now: context.now,
            last_clock_at: context.last_clock_at,
            last_wake_at: context.last_wake_at,
            last_record_at: context.last_record_at,
        };
        let message = CoreMessage::Decide {
            id: self.channel.next(),
            point: "clock".into(),
            options: clock_options(options),
            request: serde_json::to_value(wire).unwrap_or(Value::Null),
        };
        match plugin_decision(&self.channel, message).as_deref() {
            Some("due") => true,
            Some("skip") => false,
            Some(other) => {
                self.channel.note_failure(&acb_i18n::t!(
                    "adapter.dynamic.unknown_clock_decision",
                    decision = other
                ));
                false
            }
            // 出错按「不触发」兜底：时钟只是例行检查，不该因为插件坏了刷屏。
            None => false,
        }
    }
}

fn access_options(policy: &AccessPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "rooms": policy.rooms.iter().map(|room| room.to_string()).collect::<Vec<_>>(),
        "extra": policy.options,
    })
}

fn delivery_options(policy: &DeliveryPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "extra": policy.options,
    })
}

fn turn_error_options(policy: &TurnErrorPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "extra": policy.options,
    })
}

fn receive_options(policy: &ReceivePolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "extra": policy.options,
    })
}

fn clock_options(policy: &ClockPolicy) -> Value {
    json!({
        "provider": policy.provider.to_string(),
        "interval_secs": policy.interval_secs,
        "idle_secs": policy.idle_secs,
        "extra": policy.options,
    })
}

struct HostContext {
    events: EventQueue,
    stop: Arc<AtomicBool>,
    connection: String,
}

struct AdapterHandle {
    ptr: *mut c_void,
    alive: bool,
}

unsafe impl Send for AdapterHandle {}

struct DylibOutbound {
    _library: Arc<Library>,
    /// 插件可能随时回调 HostV1；这份 Arc 保证 ctx 指针一直有效。
    _context: Arc<HostContext>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    connection: String,
    catalog: Arc<[MethodSpec]>,
    seq: u64,
}

impl Outbound for DylibOutbound {
    fn send(&mut self, room: &RoomId, elements: &[Element]) -> Result<Sent, OutboundError> {
        if room.connection != self.connection {
            return Err(OutboundError::Unavailable(acb_i18n::t!(
                "adapter.dynamic.connection_mismatch",
                connection = self.connection,
                room = room.connection
            )));
        }

        self.seq += 1;
        let reply = invoke_with_timeout(
            &self.table,
            &self.handle,
            &CoreMessage::Send {
                id: self.seq,
                room: room.clone(),
                elements: elements.to_vec(),
            },
            CALL_TIMEOUT,
        )
        .map_err(OutboundError::Unavailable)?;

        match reply {
            AdapterMessage::Reply {
                result: WireResult::Sent { platform_id, at },
                ..
            } => Ok(Sent { platform_id, at }),
            AdapterMessage::Reply {
                result: WireResult::Error { error },
                ..
            } => Err(send_error(error)),
            other => Err(OutboundError::Unavailable(acb_i18n::t!(
                "adapter.dynamic.not_send_result",
                reply = format!("{other:?}")
            ))),
        }
    }

    fn catalog(&self, _connection: &str) -> Arc<[MethodSpec]> {
        Arc::clone(&self.catalog)
    }

    fn call(
        &mut self,
        room: &RoomId,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<CallOutcome, CallError> {
        if room.connection != self.connection {
            return Err(CallError::Forbidden(acb_i18n::t!(
                "adapter.dynamic.call_connection_mismatch",
                connection = self.connection,
                room = room.connection
            )));
        }

        self.seq += 1;
        let reply = invoke_with_timeout(
            &self.table,
            &self.handle,
            &CoreMessage::Call {
                id: self.seq,
                room: room.clone(),
                method: method.to_owned(),
                params: params.clone(),
            },
            CALL_TIMEOUT,
        )
        .map_err(CallError::Unavailable)?;

        match reply {
            AdapterMessage::Reply {
                result: WireResult::Call { value, record },
                ..
            } => {
                let record = record
                    .map(OutgoingRecord::try_from)
                    .transpose()
                    .map_err(CallError::Unavailable)?;
                Ok(CallOutcome { value, record })
            }
            AdapterMessage::Reply {
                result: WireResult::Error { error },
                ..
            } => Err(call_error(error)),
            other => Err(CallError::Unavailable(acb_i18n::t!(
                "adapter.dynamic.not_call_result",
                reply = format!("{other:?}")
            ))),
        }
    }
}

fn invoke(
    table: &AdapterV1,
    handle: &Arc<Mutex<AdapterHandle>>,
    message: &CoreMessage,
) -> Result<AdapterMessage, String> {
    let request = CString::new(encode_line(message).map_err(|error| {
        acb_i18n::t!("adapter.dynamic.serialize_request_failed", error = error)
    })?)
    .map_err(|_| acb_i18n::t!("adapter.dynamic.request_nul"))?;

    let guard = handle
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !guard.alive {
        return Err(acb_i18n::t!("adapter.dynamic.destroyed"));
    }
    let invoke_fn = table
        .invoke
        .ok_or(acb_i18n::t!("adapter.dynamic.missing_invoke_fn"))?;
    let pointer = unsafe { invoke_fn(guard.ptr, request.as_ptr()) };
    // 锁在调用期间不放：插件的 handle 不是线程安全的，destroy 必须排队。
    drop(guard);

    if pointer.is_null() {
        return Err(acb_i18n::t!("adapter.dynamic.no_result"));
    }
    let json = take_owned_string(pointer, table)?;
    decode_line::<AdapterMessage>(&json)
        .map_err(|error| acb_i18n::t!("adapter.dynamic.bad_reply_json", error = error))
}

fn invoke_with_timeout(
    table: &AdapterV1,
    handle: &Arc<Mutex<AdapterHandle>>,
    message: &CoreMessage,
    timeout: Duration,
) -> Result<AdapterMessage, String> {
    let table = *table;
    let handle = Arc::clone(handle);
    let message = message.clone();
    let (tx, rx) = mpsc::channel();
    // 插件的 handle 不是线程安全的：整个 invoke 都在工作线程里，调用方只等结果。
    thread::spawn(move || {
        let _ = tx.send(invoke(&table, &handle, &message));
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(acb_i18n::t!(
            "adapter.dynamic.call_timeout",
            millis = timeout.as_millis()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(acb_i18n::t!("adapter.dynamic.call_thread_gone"))
        }
    }
}

fn take_owned_string(pointer: *mut c_char, table: &AdapterV1) -> Result<String, String> {
    if pointer.is_null() {
        return Err(acb_i18n::t!("adapter.dynamic.empty_string"));
    }
    let text = unsafe { CStr::from_ptr(pointer).to_string_lossy().into_owned() };
    if let Some(free) = table.free_string {
        unsafe { free(pointer) };
    }
    Ok(text)
}

fn destroy_handle(table: &AdapterV1, handle: &Arc<Mutex<AdapterHandle>>) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match handle.try_lock() {
            Ok(mut guard) => {
                destroy_locked(table, &mut guard);
                return;
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                let mut guard = poisoned.into_inner();
                destroy_locked(table, &mut guard);
                return;
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    eprintln!("{}", acb_i18n::t!("adapter.dynamic.destroy_stuck"));
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn destroy_locked(table: &AdapterV1, guard: &mut AdapterHandle) {
    if !guard.alive {
        return;
    }
    if let Some(destroy) = table.destroy {
        unsafe { destroy(guard.ptr) };
    }
    guard.alive = false;
    guard.ptr = std::ptr::null_mut();
}

fn monitor(
    host_shutdown: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    table: AdapterV1,
    handle: Arc<Mutex<AdapterHandle>>,
    _context: Arc<HostContext>,
    connection: String,
) {
    while !host_shutdown.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(100));
    }
    stop.store(true, Ordering::Relaxed);

    // 先让插件自己收尾（Shutdown 可能返回空，忽略），再销毁 handle。
    let _ = invoke(&table, &handle, &CoreMessage::Shutdown);
    destroy_handle(&table, &handle);
    eprintln!(
        "{}",
        acb_i18n::t!("adapter.dynamic.shutdown", connection = connection)
    );
}

unsafe extern "C" fn submit_event(ctx: *mut c_void, message_json: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if ctx.is_null() || message_json.is_null() {
            return;
        }
        let context = unsafe { &*(ctx as *const HostContext) };
        let text = unsafe { CStr::from_ptr(message_json) }
            .to_string_lossy()
            .into_owned();
        match decode_line::<AdapterMessage>(&text) {
            Ok(AdapterMessage::Event { record }) => {
                if !context.events.submit(record.into()) {
                    eprintln!(
                        "{}",
                        acb_i18n::t!(
                            "adapter.dynamic.inbound_queue_closed",
                            connection = context.connection
                        )
                    );
                }
            }
            Ok(other) => eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.dynamic.submit_event_wrong_message",
                    connection = context.connection,
                    message = format!("{other:?}")
                )
            ),
            Err(error) => eprintln!(
                "{}",
                acb_i18n::t!(
                    "adapter.dynamic.submit_event_bad_json",
                    connection = context.connection,
                    error = error
                )
            ),
        }
    }));
}

unsafe extern "C" fn should_stop(ctx: *mut c_void) -> i32 {
    if ctx.is_null() {
        return 1;
    }
    let context = unsafe { &*(ctx as *const HostContext) };
    i32::from(context.stop.load(Ordering::Relaxed))
}

unsafe extern "C" fn log(ctx: *mut c_void, level: *const c_char, message: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let connection = if ctx.is_null() {
            "?".to_owned()
        } else {
            unsafe { &*(ctx as *const HostContext) }.connection.clone()
        };
        let level = if level.is_null() {
            "log".into()
        } else {
            unsafe { CStr::from_ptr(level) }
                .to_string_lossy()
                .into_owned()
        };
        let message = if message.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        };
        eprintln!(
            "{}",
            acb_i18n::t!(
                "adapter.dynamic.log_line",
                connection = connection,
                level = level,
                message = message
            )
        );
    }));
}
