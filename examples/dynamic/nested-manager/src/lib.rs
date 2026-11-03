//! PluginManager 嵌套 manager 示例（ADR-0026 Phase 3）。
//!
//! 父 manager 只负责组织子树，自己不提供 provider；它通过
//! [`ManagerPlugin::children`] 返回子 manager。子 manager 注册
//! `child.always`，配置里直接引用这个 provider 即可。
//!
//! 注意：manager 树是安装/生命周期结构，不编码进 provider id。
//! `ProviderId` 的 namespace 只有第一段（`child.always` 的 namespace 是
//! `child`），所以配置写 `child.always`，不是 `parent.child.always`。

use std::sync::Arc;

use acb_kernel::{
    Capability, ExtensionPoint, Health, ManagerHealth, ManagerManifest, ManagerPlugin,
    PLUGIN_API_VERSION, PluginError, PluginManager, PluginMetadata, PluginSource,
    ProviderDescriptor, ProviderId, ProviderInstance, ProviderOptions, WakeContext, WakeDecision,
    WakePolicy, WakeProvider, WakeVerdict,
};

/// 父 manager 的 namespace：只做容器，不直接提供 provider。
pub const PARENT_NAMESPACE: &str = "parent";

/// 子 manager 的 namespace。
pub const CHILD_NAMESPACE: &str = "child";

/// 子 manager 提供的 provider id。配置里引用这个。
pub const CHILD_PROVIDER: &str = "child.always";

fn child_provider_id() -> ProviderId {
    ProviderId::parse(CHILD_PROVIDER).expect("示例 provider id 必须合法")
}

fn metadata() -> PluginMetadata {
    PluginMetadata {
        summary: Some("嵌套 manager 示例：子 manager 提供的 always wake".into()),
        ..PluginMetadata::EMPTY
    }
}

/// 总是唤醒。示例只演示注册与解析，不读 options。
#[derive(Debug, Default)]
pub struct AlwaysWake;

impl WakeProvider for AlwaysWake {
    fn decide(&self, _options: &WakePolicy, _context: &WakeContext<'_>) -> WakeDecision {
        WakeDecision::Wake
    }

    fn decide_with_reason(&self, _options: &WakePolicy, _context: &WakeContext<'_>) -> WakeVerdict {
        WakeVerdict::wake("nested manager example")
    }
}

/// 子 manager：真正提供 `child.always`。
#[derive(Debug, Default)]
pub struct ChildManager;

impl ManagerPlugin for ChildManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(
            CHILD_NAMESPACE,
            PLUGIN_API_VERSION,
            vec![Capability::WakeDecision],
        )
        .with_metadata(metadata())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        vec![
            ProviderDescriptor::new(
                child_provider_id(),
                ExtensionPoint::Wake,
                PLUGIN_API_VERSION,
                vec![Capability::WakeDecision],
            )
            .with_metadata(metadata())
            .with_source(PluginSource::Trusted),
        ]
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        if id == &child_provider_id() {
            Ok(ProviderInstance::Wake(Arc::new(AlwaysWake)))
        } else {
            Err(PluginError::UnknownProvider(id.to_string()))
        }
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

/// 父 manager：没有自己的 provider，只通过 `children()` 挂子 manager。
#[derive(Debug, Default)]
pub struct ParentManager;

impl ManagerPlugin for ParentManager {
    fn manifest(&self) -> ManagerManifest {
        ManagerManifest::new(PARENT_NAMESPACE, PLUGIN_API_VERSION, Vec::new())
    }

    fn descriptors(&self) -> Vec<ProviderDescriptor> {
        Vec::new()
    }

    fn instantiate(
        &self,
        id: &ProviderId,
        _options: ProviderOptions<'_>,
    ) -> Result<ProviderInstance, PluginError> {
        Err(PluginError::UnknownProvider(id.to_string()))
    }

    fn children(&self) -> Vec<Arc<dyn ManagerPlugin>> {
        vec![Arc::new(ChildManager)]
    }

    fn health(&self) -> Health {
        Health::Healthy
    }
}

/// 把整棵示例子树装进 Root。生产代码在启动时调用它，再 `Bridge::with_plugins`。
pub fn install(manager: &mut PluginManager) -> Result<(), PluginError> {
    manager.install_manager(Arc::new(ParentManager))
}

/// `PluginManager::manager_health()` 的预期顺序：先父后子。
pub fn expected_health() -> Vec<ManagerHealth> {
    vec![
        ManagerHealth {
            namespace: PARENT_NAMESPACE.to_owned(),
            health: Health::Healthy,
        },
        ManagerHealth {
            namespace: CHILD_NAMESPACE.to_owned(),
            health: Health::Healthy,
        },
    ]
}

/// 只读访问示例的 provider id，避免测试重复拼字符串。
pub fn provider_id() -> ProviderId {
    child_provider_id()
}

#[cfg(test)]
mod tests {
    use super::*;
    use acb_kernel::{Config, RoomId};

    fn config_with_provider(provider: &str) -> String {
        format!(
            r#"
            [[connections]]
            id = "qq"
            platform = "qq"
            kind = "onebot11"
            url = "ws://127.0.0.1:3001"
            account = "10001"

            [[personas]]
            id = "p"
            name = "P"
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

            [sessions.subscriptions.wake]
            provider = "{provider}"
            "#
        )
    }

    fn context(room: &RoomId) -> WakeContext<'_> {
        WakeContext {
            session: "main",
            room,
            connection: "qq",
            platform: "qq",
            sender_id: "10001",
            text: "在吗",
            at_self: false,
            reply_to_self: false,
            has_body: true,
            event: None,
            tags: &[],
            unread: 1,
            last_wake_at: 0,
            now: 0,
        }
    }

    #[test]
    fn child_provider_is_installed_and_resolves() {
        let mut plugins = PluginManager::with_native_builtins();
        install(&mut plugins).expect("嵌套子树应当能安装");

        let provider = plugins
            .resolve_wake(&provider_id(), &WakePolicy::default())
            .expect("子 manager 的 provider 应当可解析");
        let room = RoomId::new("qq", "group/1").expect("合法");
        assert_eq!(
            provider.decide(&WakePolicy::default(), &context(&room)),
            WakeDecision::Wake
        );
        assert!(
            plugins
                .catalog()
                .iter()
                .any(|info| info.id == provider_id() && info.source == PluginSource::Trusted),
            "子 manager 的 provider 应当进 Root 目录"
        );
    }

    #[test]
    fn config_can_reference_the_child_provider() {
        let config = Config::parse(&config_with_provider(CHILD_PROVIDER)).expect("配置合法");
        let mut plugins = PluginManager::with_native_builtins();
        install(&mut plugins).expect("嵌套子树应当能安装");
        plugins
            .validate_config(&config)
            .expect("配置引用 child.always 应当能解析");
    }

    #[test]
    fn health_order_is_parent_then_child() {
        let mut plugins = PluginManager::with_native_builtins();
        install(&mut plugins).expect("嵌套子树应当能安装");
        assert_eq!(plugins.manager_health(), expected_health());
    }

    #[test]
    fn parent_itself_does_not_provide_the_child_id() {
        let parent = ParentManager;
        assert!(matches!(
            parent.instantiate(
                &provider_id(),
                ProviderOptions::Wake(&WakePolicy::default())
            ),
            Err(PluginError::UnknownProvider(_))
        ));
    }
}
