//! 动态扩展目录（ADR-0029 P1 的 route 最小闭环）。
//!
//! 目录结构：
//!
//! ```text
//! $ACB_DYNAMIC_DIR/
//!   echo/
//!     manifest.toml
//! ```
//!
//! 每个子目录一个声明式模块。模块只能声明 `extension = "route"`，由 core 解释
//! `match` / `action`，不执行任意代码。Root 只暴露一个稳定 provider `dyn.routes`；
//! 增删模块不改 `config.toml`，热重载在轮次边界整体替换快照。
//!
//! 安全边界：
//! - 只从 `ACB_DYNAMIC_DIR` 指定的受信目录发现；
//! - 模块目录与 manifest 必须是普通目录/文件，符号链接拒绝；
//! - 规范化后必须在根目录内；
//! - manifest 有大小上限，字段未知直接拒绝；
//! - 任一模块校验失败时整次重载不替换线上快照。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    Capability, Element, ExtensionPoint, Health, ManagerManifest, ManagerPlugin,
    PLUGIN_API_VERSION, PluginEffect, PluginError, PluginMetadata, PluginSource,
    ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, RoomId, RouteAction,
    RouteContext, RouteDecision, RoutePolicy, RouteProvider,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DYNAMIC_DIR_ENV: &str = "ACB_DYNAMIC_DIR";
pub const NAMESPACE: &str = "dyn";
pub const ROUTES_PROVIDER: &str = "dyn.routes";
pub const MANIFEST_FILE: &str = "manifest.toml";
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

/// 一个发现结果。`errors` 非空时调用方不能替换线上快照。
struct Scan {
    modules: Vec<Module>,
    errors: Vec<ModuleError>,
}

#[derive(Debug, Clone)]
struct ModuleError {
    module: String,
    detail: String,
}

struct Module {
    spec: ModuleSpec,
    path: PathBuf,
    mtime: i64,
    hash: String,
    runtime: Arc<Runtime>,
}

#[derive(Default)]
struct Runtime {
    calls: AtomicU64,
}

struct ModuleSpec {
    id: ProviderId,
    summary: String,
    effects: Vec<String>,
    capabilities: Vec<String>,
    scope: Scope,
    matcher: Matcher,
    action: Action,
}

enum Scope {
    Global,
    Connections(Vec<String>),
    Sessions(Vec<String>),
}

impl Scope {
    fn label(&self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Connections(_) => "connection",
            Self::Sessions(_) => "session",
        }
    }

    fn matches(&self, context: &RouteContext<'_>) -> bool {
        match self {
            Self::Global => true,
            Self::Connections(connections) => connections.iter().any(|id| id == context.connection),
            Self::Sessions(sessions) => sessions
                .iter()
                .any(|session| context.sessions.iter().any(|active| active == session)),
        }
    }
}

struct Matcher {
    text_prefix: Option<String>,
    text_exact: Option<String>,
    text_contains: Option<String>,
    rooms: Vec<RoomId>,
    is_admin: Option<bool>,
}

impl Matcher {
    fn matches(&self, context: &RouteContext<'_>) -> bool {
        if let Some(prefix) = &self.text_prefix {
            if !context.text.starts_with(prefix) {
                return false;
            }
        }
        if let Some(exact) = &self.text_exact {
            if context.text != exact {
                return false;
            }
        }
        if let Some(needle) = &self.text_contains {
            if !context.text.contains(needle) {
                return false;
            }
        }
        if !self.rooms.is_empty() && !self.rooms.iter().any(|room| room == context.room) {
            return false;
        }
        if let Some(admin) = self.is_admin {
            if admin != context.is_admin {
                return false;
            }
        }
        true
    }
}

enum Action {
    Pass,
    Drop {
        reason: String,
    },
    Send {
        room: Option<RoomId>,
        elements: Vec<Element>,
        store: bool,
        wake: bool,
    },
}

impl Action {
    fn decision(&self) -> RouteDecision {
        match self {
            Self::Pass => RouteDecision::Pass,
            Self::Drop { reason } => RouteDecision::Drop {
                reason: Some(reason.clone()),
            },
            Self::Send {
                room,
                elements,
                store,
                wake,
            } => RouteDecision::Handle {
                store: *store || *wake,
                wake: *wake,
                actions: vec![RouteAction::Send {
                    room: room.clone(),
                    elements: elements.clone(),
                }],
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    id: String,
    api_version: u32,
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default = "default_carrier")]
    carrier: String,
    extension: String,
    summary: String,
    #[serde(default)]
    effects: Vec<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    connections: Vec<String>,
    #[serde(default)]
    sessions: Vec<String>,
    #[serde(default, rename = "match")]
    match_rule: RawMatch,
    action: RawAction,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMatch {
    #[serde(default)]
    text_prefix: Option<String>,
    #[serde(default)]
    text_exact: Option<String>,
    #[serde(default)]
    text_contains: Option<String>,
    #[serde(default)]
    rooms: Vec<String>,
    #[serde(default)]
    is_admin: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAction {
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    room: Option<String>,
    #[serde(default)]
    store: bool,
    #[serde(default)]
    wake: bool,
    #[serde(default)]
    reason: Option<String>,
}

fn default_version() -> u32 {
    1
}

fn default_carrier() -> String {
    "declarative".to_owned()
}

fn default_scope() -> String {
    "global".to_owned()
}

/// 动态扩展 manager。Root 里的 provider 是 `dyn.routes`，模块级信息在 [`Self::details`]。
pub struct DynamicRouteManager {
    root: PathBuf,
    shared: Arc<Shared>,
}

struct Shared {
    /// 线上快照。轮次边界重载时整体替换；旧轮次已经拿到的 Arc 继续用旧快照。
    live: RwLock<Arc<Snapshot>>,
    status: Mutex<Status>,
}

struct Snapshot {
    modules: Vec<Module>,
}

struct Status {
    errors: Vec<ModuleError>,
    health: Health,
    scanned_at: i64,
}

impl DynamicRouteManager {
    /// 从 `ACB_DYNAMIC_DIR` 读目录；未设置或空串返回 `None`。
    pub fn from_env() -> Result<Option<Self>, PluginError> {
        let Some(root) = std::env::var_os(DYNAMIC_DIR_ENV) else {
            return Ok(None);
        };
        if root.is_empty() {
            return Ok(None);
        }
        Self::load(PathBuf::from(root)).map(Some)
    }

    /// 首次加载。目录本身不可读是启动错误；单个模块坏只进入 health/errors。
    pub fn load(root: impl Into<PathBuf>) -> Result<Self, PluginError> {
        let root = root.into();
        let scan = scan_dir(&root)?;
        let health = health_for(&scan.errors);
        let shared = Arc::new(Shared {
            live: RwLock::new(Arc::new(Snapshot {
                modules: scan.modules,
            })),
            status: Mutex::new(Status {
                errors: scan.errors,
                health,
                scanned_at: now_millis(),
            }),
        });
        Ok(Self { root, shared })
    }

    /// 重新扫描。任一模块失败时不替换 live 快照。
    pub fn reload(&self) -> Result<(), PluginError> {
        let scan = scan_dir(&self.root)?;
        let mut status = self
            .shared
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        status.scanned_at = now_millis();
        if !scan.errors.is_empty() {
            let count = scan.errors.len();
            status.health = health_for(&scan.errors);
            status.errors = scan.errors;
            return Err(PluginError::Manager {
                namespace: NAMESPACE.to_owned(),
                detail: acb_i18n::t!("extensions.reload_rejected", count = count),
            });
        }
        *self
            .shared
            .live
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::new(Snapshot {
            modules: scan.modules,
        });
        status.health = Health::Healthy;
        status.errors.clear();
        Ok(())
    }

    fn router(&self) -> DynamicRouter {
        DynamicRouter {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl ManagerPlugin for DynamicRouteManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            NAMESPACE,
            PLUGIN_API_VERSION,
            vec![Capability::RouteDecision],
        )
        .with_metadata(manager_metadata())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![routes_descriptor()]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id == &routes_id() {
            Ok(ProviderInstance::Route(Arc::new(self.router())))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }

    fn reload(&self, _scope: &acb_kernel::ReloadScope) -> Result<(), PluginError> {
        self.reload()
    }

    fn health(&self) -> Health {
        self.shared
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .health
            .clone()
    }

    fn root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }

    fn details(&self) -> Value {
        let snapshot = self
            .shared
            .live
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let status = self
            .shared
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        json!({
            "namespace": NAMESPACE,
            "provider": ROUTES_PROVIDER,
            "modules": snapshot.modules.iter().map(module_json).collect::<Vec<_>>(),
            "errors": status.errors.iter().map(|error| json!({
                "module": error.module,
                "detail": error.detail,
            })).collect::<Vec<_>>(),
            "health": health_json(&status.health),
            "scannedAt": status.scanned_at,
            "root": self.root.display().to_string(),
        })
    }
}

struct DynamicRouter {
    shared: Arc<Shared>,
}

impl RouteProvider for DynamicRouter {
    fn decide(&self, _options: &RoutePolicy, context: &RouteContext<'_>) -> RouteDecision {
        let snapshot = self
            .shared
            .live
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for module in &snapshot.modules {
            if !module.spec.scope.matches(context) {
                continue;
            }
            if !module.spec.matcher.matches(context) {
                continue;
            }
            module.runtime.calls.fetch_add(1, Ordering::Relaxed);
            match &module.spec.action {
                Action::Pass => continue,
                other => return other.decision(),
            }
        }
        RouteDecision::Pass
    }
}

fn routes_id() -> ProviderId {
    ProviderId::parse(ROUTES_PROVIDER).expect("built-in provider id must be valid")
}

fn manager_metadata() -> PluginMetadata {
    PluginMetadata {
        title: Some("动态扩展目录".into()),
        summary: Some("扫描 ACB_DYNAMIC_DIR，按顺序执行声明式 route 模块".into()),
        effects: Vec::new().into(),
        ..PluginMetadata::EMPTY
    }
}

fn routes_descriptor() -> ProviderDescriptor {
    ProviderDescriptor::new(
        routes_id(),
        ExtensionPoint::Route,
        PLUGIN_API_VERSION,
        vec![Capability::RouteDecision],
    )
    .with_metadata(PluginMetadata {
        title: Some("动态 route 分派器".into()),
        summary: Some("按模块顺序执行 ACB_DYNAMIC_DIR 里的 route 模块".into()),
        ..PluginMetadata::EMPTY
    })
    .with_source(PluginSource::Declarative)
}

fn scan_dir(root: &Path) -> Result<Scan, PluginError> {
    let root = fs::canonicalize(root).map_err(|error| {
        PluginError::Loader(acb_i18n::t!(
            "extensions.root.read_failed",
            path = root.display(),
            error = error
        ))
    })?;
    if !root.is_dir() {
        return Err(PluginError::Loader(acb_i18n::t!(
            "extensions.root.not_dir",
            path = root.display()
        )));
    }

    let mut entries: Vec<_> = fs::read_dir(&root)
        .map_err(|error| {
            PluginError::Loader(acb_i18n::t!(
                "extensions.root.read_failed",
                path = root.display(),
                error = error
            ))
        })?
        .collect::<Result<_, _>>()
        .map_err(|error| {
            PluginError::Loader(acb_i18n::t!(
                "extensions.root.read_failed",
                path = root.display(),
                error = error
            ))
        })?;
    entries.sort_by_key(|entry| entry.file_name());

    let mut modules = Vec::new();
    let mut errors = Vec::new();
    let mut seen_ids: HashMap<String, PathBuf> = HashMap::new();

    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                errors.push(module_error(
                    &name,
                    acb_i18n::t!(
                        "extensions.module.read_failed",
                        path = path.display(),
                        error = error
                    ),
                ));
                continue;
            }
        };
        if file_type.is_symlink() {
            errors.push(module_error(
                &name,
                acb_i18n::t!("extensions.module.symlink", path = path.display()),
            ));
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        let module_dir = match fs::canonicalize(&path) {
            Ok(module_dir) => module_dir,
            Err(error) => {
                errors.push(module_error(
                    &name,
                    acb_i18n::t!(
                        "extensions.module.read_failed",
                        path = path.display(),
                        error = error
                    ),
                ));
                continue;
            }
        };
        if !module_dir.starts_with(&root) {
            errors.push(module_error(
                &name,
                acb_i18n::t!("extensions.module.path_escape", path = module_dir.display()),
            ));
            continue;
        }
        let manifest_path = module_dir.join(MANIFEST_FILE);
        let metadata = match fs::symlink_metadata(&manifest_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                errors.push(module_error(
                    &name,
                    acb_i18n::t!(
                        "extensions.manifest.missing",
                        module = name,
                        path = manifest_path.display(),
                        error = error
                    ),
                ));
                continue;
            }
        };
        if metadata.file_type().is_symlink() {
            errors.push(module_error(
                &name,
                acb_i18n::t!(
                    "extensions.manifest.symlink",
                    path = manifest_path.display()
                ),
            ));
            continue;
        }
        if !metadata.is_file() {
            errors.push(module_error(
                &name,
                acb_i18n::t!(
                    "extensions.manifest.not_file",
                    path = manifest_path.display()
                ),
            ));
            continue;
        }
        if metadata.len() > MAX_MANIFEST_BYTES {
            errors.push(module_error(
                &name,
                acb_i18n::t!(
                    "extensions.manifest.too_large",
                    path = manifest_path.display(),
                    size = metadata.len(),
                    max = MAX_MANIFEST_BYTES
                ),
            ));
            continue;
        }
        let bytes = match fs::read(&manifest_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                errors.push(module_error(
                    &name,
                    acb_i18n::t!(
                        "extensions.manifest.read_failed",
                        path = manifest_path.display(),
                        error = error
                    ),
                ));
                continue;
            }
        };
        let raw: RawManifest = match toml::from_str(&String::from_utf8_lossy(&bytes)) {
            Ok(raw) => raw,
            Err(error) => {
                errors.push(module_error(
                    &name,
                    acb_i18n::t!(
                        "extensions.manifest.parse_failed",
                        path = manifest_path.display(),
                        error = error
                    ),
                ));
                continue;
            }
        };
        let spec = match validate(raw, &name) {
            Ok(spec) => spec,
            Err(detail) => {
                errors.push(module_error(&name, detail));
                continue;
            }
        };
        if let Some(first) = seen_ids.get(&spec.id.to_string()) {
            errors.push(module_error(
                &name,
                acb_i18n::t!(
                    "extensions.manifest.duplicate_id",
                    id = spec.id,
                    first = first.display(),
                    second = module_dir.display()
                ),
            ));
            continue;
        }
        seen_ids.insert(spec.id.to_string(), module_dir.clone());
        modules.push(Module {
            spec,
            path: manifest_path,
            mtime: mtime_millis(&metadata),
            hash: content_hash(&bytes),
            runtime: Arc::new(Runtime::default()),
        });
    }

    Ok(Scan { modules, errors })
}

fn validate(raw: RawManifest, module: &str) -> Result<ModuleSpec, String> {
    let id = ProviderId::parse(&raw.id).map_err(|error| {
        acb_i18n::t!(
            "extensions.manifest.id_invalid",
            module = module,
            id = raw.id,
            error = error
        )
    })?;
    if raw.api_version != PLUGIN_API_VERSION {
        return Err(acb_i18n::t!(
            "extensions.manifest.api_version",
            id = id,
            found = raw.api_version,
            required = PLUGIN_API_VERSION
        ));
    }
    if raw.version == 0 {
        return Err(acb_i18n::t!("extensions.manifest.version", id = id));
    }
    let carrier = raw.carrier.trim();
    if carrier != "declarative" {
        return Err(acb_i18n::t!(
            "extensions.manifest.carrier",
            id = id,
            carrier = carrier
        ));
    }
    let extension = raw.extension.trim();
    if extension != "route" {
        return Err(acb_i18n::t!(
            "extensions.manifest.extension",
            id = id,
            extension = extension
        ));
    }
    let summary = raw.summary.trim();
    if summary.is_empty() {
        return Err(acb_i18n::t!("extensions.manifest.summary", id = id));
    }
    if !raw
        .capabilities
        .iter()
        .any(|capability| capability == "route_decision")
    {
        return Err(acb_i18n::t!(
            "extensions.manifest.capability",
            id = id,
            found = raw.capabilities.join(", ")
        ));
    }
    if let Some(unknown) = raw
        .capabilities
        .iter()
        .find(|capability| capability.as_str() != "route_decision")
    {
        return Err(acb_i18n::t!(
            "extensions.manifest.capability",
            id = id,
            found = unknown
        ));
    }
    let mut effects = Vec::new();
    for effect in &raw.effects {
        let parsed = PluginEffect::parse(effect)
            .ok_or_else(|| acb_i18n::t!("extensions.manifest.effect", id = id, effect = effect))?;
        effects.push(parsed.as_str().to_owned());
    }
    let scope = match raw.scope.trim() {
        "global" => Scope::Global,
        "connection" if !raw.connections.is_empty() => Scope::Connections(raw.connections.clone()),
        "session" if !raw.sessions.is_empty() => Scope::Sessions(raw.sessions.clone()),
        "connection" => {
            return Err(acb_i18n::t!(
                "extensions.manifest.scope_list",
                id = id,
                scope = "connection",
                field = "connections"
            ));
        }
        "session" => {
            return Err(acb_i18n::t!(
                "extensions.manifest.scope_list",
                id = id,
                scope = "session",
                field = "sessions"
            ));
        }
        other => {
            return Err(acb_i18n::t!(
                "extensions.manifest.scope",
                id = id,
                scope = other
            ));
        }
    };
    let matcher = validate_match(raw.match_rule, &id)?;
    let action = validate_action(raw.action, &id, summary)?;
    Ok(ModuleSpec {
        id,
        summary: summary.to_owned(),
        effects,
        capabilities: raw.capabilities,
        scope,
        matcher,
        action,
    })
}

fn validate_match(raw: RawMatch, id: &ProviderId) -> Result<Matcher, String> {
    let text_prefix = trim_option(raw.text_prefix);
    let text_exact = trim_option(raw.text_exact);
    let text_contains = trim_option(raw.text_contains);
    let mut rooms = Vec::new();
    for room in &raw.rooms {
        rooms.push(RoomId::parse(room).map_err(|error| {
            acb_i18n::t!(
                "extensions.manifest.room",
                id = id,
                room = room,
                error = error
            )
        })?);
    }
    if text_prefix.is_none()
        && text_exact.is_none()
        && text_contains.is_none()
        && rooms.is_empty()
        && raw.is_admin.is_none()
    {
        return Err(acb_i18n::t!("extensions.manifest.match_empty", id = id));
    }
    Ok(Matcher {
        text_prefix,
        text_exact,
        text_contains,
        rooms,
        is_admin: raw.is_admin,
    })
}

fn validate_action(raw: RawAction, id: &ProviderId, summary: &str) -> Result<Action, String> {
    match raw.kind.trim() {
        "pass" => Ok(Action::Pass),
        "drop" => Ok(Action::Drop {
            reason: raw.reason.unwrap_or_else(|| summary.to_owned()),
        }),
        "send" => {
            let text = raw.text.unwrap_or_default().trim().to_owned();
            if text.is_empty() {
                return Err(acb_i18n::t!("extensions.manifest.send_text", id = id));
            }
            let room = match raw.room {
                Some(room) => Some(RoomId::parse(&room).map_err(|error| {
                    acb_i18n::t!(
                        "extensions.manifest.room",
                        id = id,
                        room = room,
                        error = error
                    )
                })?),
                None => None,
            };
            Ok(Action::Send {
                room,
                elements: vec![Element::text(text)],
                store: raw.store,
                wake: raw.wake,
            })
        }
        other => Err(acb_i18n::t!(
            "extensions.manifest.action",
            id = id,
            action = other
        )),
    }
}

fn trim_option(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn module_error(module: &str, detail: String) -> ModuleError {
    ModuleError {
        module: module.to_owned(),
        detail,
    }
}

fn health_for(errors: &[ModuleError]) -> Health {
    if errors.is_empty() {
        Health::Healthy
    } else {
        Health::Degraded(acb_i18n::t!(
            "extensions.health_degraded",
            count = errors.len()
        ))
    }
}

fn health_json(health: &Health) -> Value {
    match health {
        Health::Healthy => json!({ "state": "healthy" }),
        Health::Degraded(detail) => json!({ "state": "degraded", "detail": detail }),
        Health::Unknown(detail) => json!({ "state": "unknown", "detail": detail }),
        Health::Unhealthy(detail) => json!({ "state": "unhealthy", "detail": detail }),
    }
}

fn module_json(module: &Module) -> Value {
    json!({
        "id": module.spec.id.to_string(),
        "extension": "route",
        "summary": module.spec.summary,
        "effects": module.spec.effects,
        "capabilities": module.spec.capabilities,
        "scope": module.spec.scope.label(),
        "source": "declarative",
        "path": module.path.display().to_string(),
        "mtime": module.mtime,
        "hash": module.hash,
        "calls": module.runtime.calls.load(Ordering::Relaxed),
        "failures": 0,
    })
}

fn content_hash(bytes: &[u8]) -> String {
    // FNV-1a 64：只做内容变更检测，不是密码学摘要。
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn mtime_millis(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::{RecordKind, Sender};
    use std::os::unix::fs::symlink;

    fn temp_root() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时钟")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("acb-ext-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write_module(root: &Path, dir: &str, manifest: &str) {
        let module = root.join(dir);
        fs::create_dir_all(&module).expect("建模块目录");
        fs::write(module.join(MANIFEST_FILE), manifest).expect("写 manifest");
    }

    fn valid_manifest(id: &str, prefix: &str) -> String {
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

    fn context<'a>(
        room: &'a RoomId,
        sender: &'a Sender,
        kind: &'a RecordKind,
        text: &'a str,
        sessions: &'a [String],
    ) -> RouteContext<'a> {
        RouteContext {
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
            provider: routes_id(),
            options: json!({}),
        }
    }

    fn route_provider(manager: &DynamicRouteManager) -> Arc<dyn RouteProvider> {
        match manager
            .instantiate(&routes_id(), ProviderOptions::Route(&route_policy()))
            .expect("应当能实例化")
        {
            ProviderInstance::Route(provider) => provider,
            other => panic!("expected route, got {:?}", other.extension_point()),
        }
    }

    #[test]
    fn manifest_is_discovered_and_route_matches() {
        let root = temp_root();
        write_module(&root, "echo", &valid_manifest("dyn.echo", "/echo"));
        let manager = DynamicRouteManager::load(&root).expect("加载");
        let router = route_provider(&manager);
        let room = RoomId::parse("qq:group/1").expect("房间");
        let sender = Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        };
        let kind = RecordKind::Message;
        let sessions = vec!["main".to_owned()];
        let decision = router.decide(
            &route_policy(),
            &context(&room, &sender, &kind, "/echo hi", &sessions),
        );
        match decision {
            RouteDecision::Handle { actions, .. } => match &actions[0] {
                RouteAction::Send { elements, .. } => {
                    assert_eq!(elements, &vec![Element::text("ok")])
                }
                other => panic!("unexpected action {other:?}"),
            },
            other => panic!("unexpected decision {other:?}"),
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let root = temp_root();
        write_module(&root, "a", &valid_manifest("dyn.same", "/a"));
        write_module(&root, "b", &valid_manifest("dyn.same", "/b"));
        let scan = scan_dir(&root).expect("扫描");
        assert_eq!(scan.modules.len(), 1);
        assert_eq!(scan.errors.len(), 1);
        assert!(scan.errors[0].detail.contains("duplicate"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_manifest_is_reported() {
        let root = temp_root();
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
        let scan = scan_dir(&root).expect("扫描");
        assert_eq!(scan.modules.len(), 0);
        assert_eq!(scan.errors.len(), 1);
        assert!(scan.errors[0].detail.contains("summary"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_manifest_and_unsupported_fields_are_rejected() {
        let root = temp_root();
        fs::create_dir_all(root.join("empty")).expect("建空模块");
        write_module(
            &root,
            "native",
            &valid_manifest("dyn.native", "/native")
                .replace("carrier = \"declarative\"", "carrier = \"native\""),
        );
        write_module(
            &root,
            "wake",
            &valid_manifest("dyn.wake", "/wake")
                .replace("extension = \"route\"", "extension = \"wake\"")
                .replace(
                    "capabilities = [\"route_decision\"]",
                    "capabilities = [\"route_decision\", \"wake_decision\"]",
                ),
        );
        let scan = scan_dir(&root).expect("扫描");
        assert_eq!(scan.modules.len(), 0);
        assert_eq!(scan.errors.len(), 3);
        let text = scan
            .errors
            .iter()
            .map(|error| error.detail.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("manifest.toml"));
        assert!(text.contains("carrier"));
        assert!(text.contains("extension") || text.contains("capabilities"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn scope_filters_dispatch_by_connection_and_session() {
        let root = temp_root();
        write_module(&root, "global", &valid_manifest("dyn.global", "/global"));
        write_module(
            &root,
            "conn",
            &valid_manifest("dyn.conn", "/conn").replace(
                "capabilities = [\"route_decision\"]",
                "capabilities = [\"route_decision\"]\nscope = \"connection\"\nconnections = [\"qq\"]",
            ),
        );
        write_module(
            &root,
            "sess",
            &valid_manifest("dyn.sess", "/sess").replace(
                "capabilities = [\"route_decision\"]",
                "capabilities = [\"route_decision\"]\nscope = \"session\"\nsessions = [\"main\"]",
            ),
        );
        let manager = DynamicRouteManager::load(&root).expect("加载");
        let router = route_provider(&manager);
        let sender = Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        };
        let kind = RecordKind::Message;
        let qq = RoomId::parse("qq:group/1").expect("房间");
        let dc = RoomId::parse("dc:group/1").expect("房间");
        let main = vec!["main".to_owned()];
        let other = vec!["other".to_owned()];
        assert!(matches!(
            router.decide(
                &route_policy(),
                &context(&qq, &sender, &kind, "/global hi", &main)
            ),
            RouteDecision::Handle { .. }
        ));
        assert!(matches!(
            router.decide(
                &route_policy(),
                &context(&qq, &sender, &kind, "/conn hi", &main)
            ),
            RouteDecision::Handle { .. }
        ));
        assert_eq!(
            router.decide(
                &route_policy(),
                &context(&dc, &sender, &kind, "/conn hi", &main)
            ),
            RouteDecision::Pass
        );
        assert!(matches!(
            router.decide(
                &route_policy(),
                &context(&qq, &sender, &kind, "/sess hi", &main)
            ),
            RouteDecision::Handle { .. }
        ));
        assert_eq!(
            router.decide(
                &route_policy(),
                &context(&qq, &sender, &kind, "/sess hi", &other)
            ),
            RouteDecision::Pass
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn symlink_module_is_rejected() {
        let root = temp_root();
        let external = temp_root();
        write_module(&external, "real", &valid_manifest("dyn.real", "/real"));
        symlink(external.join("real"), root.join("linked")).expect("建符号链接");
        let scan = scan_dir(&root).expect("扫描");
        assert_eq!(scan.modules.len(), 0);
        assert_eq!(scan.errors.len(), 1);
        assert!(scan.errors[0].detail.contains("symlink"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&external);
    }

    #[test]
    fn symlink_manifest_is_rejected() {
        let root = temp_root();
        let external = temp_root();
        let manifest = external.join("manifest.toml");
        fs::write(&manifest, valid_manifest("dyn.link", "/link")).expect("写外部 manifest");
        let module = root.join("linked");
        fs::create_dir_all(&module).expect("建模块目录");
        symlink(&manifest, module.join(MANIFEST_FILE)).expect("建 manifest 符号链接");
        let scan = scan_dir(&root).expect("扫描");
        assert_eq!(scan.modules.len(), 0);
        assert_eq!(scan.errors.len(), 1);
        assert!(scan.errors[0].detail.contains("symlink"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&external);
    }

    #[test]
    fn bad_candidate_does_not_replace_live_snapshot() {
        let root = temp_root();
        write_module(&root, "echo", &valid_manifest("dyn.echo", "/echo"));
        let manager = DynamicRouteManager::load(&root).expect("加载");
        let router = route_provider(&manager);
        let room = RoomId::parse("qq:group/1").expect("房间");
        let sender = Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        };
        let kind = RecordKind::Message;
        let sessions = vec!["main".to_owned()];

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
        assert!(manager.reload().is_err());
        let decision = router.decide(
            &route_policy(),
            &context(&room, &sender, &kind, "/echo hi", &sessions),
        );
        assert!(matches!(decision, RouteDecision::Handle { .. }));
        let details = manager.details();
        assert_eq!(details["errors"].as_array().map(Vec::len), Some(1));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn added_and_removed_modules_swap_on_reload() {
        let root = temp_root();
        write_module(&root, "one", &valid_manifest("dyn.one", "/one"));
        let manager = DynamicRouteManager::load(&root).expect("加载");
        let router = route_provider(&manager);
        let room = RoomId::parse("qq:group/1").expect("房间");
        let sender = Sender {
            id: "10001".into(),
            name: "u".into(),
            role: None,
        };
        let kind = RecordKind::Message;
        let sessions = vec!["main".to_owned()];

        write_module(&root, "two", &valid_manifest("dyn.two", "/two"));
        manager.reload().expect("增加模块后重载");
        let decision = router.decide(
            &route_policy(),
            &context(&room, &sender, &kind, "/two hi", &sessions),
        );
        assert!(matches!(decision, RouteDecision::Handle { .. }));

        fs::remove_dir_all(root.join("two")).expect("删模块");
        manager.reload().expect("删除模块后重载");
        let decision = router.decide(
            &route_policy(),
            &context(&room, &sender, &kind, "/two hi", &sessions),
        );
        assert_eq!(decision, RouteDecision::Pass);
        let _ = fs::remove_dir_all(&root);
    }
}
