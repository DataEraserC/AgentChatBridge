//! 双载体参考实现测试：WASM 宿主（进程载体）与 ABI v2 `.so`。
//!
//! 默认只跑 WASM 部分；`.so` 与一致性测试要显式开 feature：
//!   cargo test -p acb-wasm-host --features native-so

#![cfg(feature = "wasm-host")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[cfg(feature = "native-so")]
use acb_kernel::PluginConfig;
use acb_kernel::{
    AccessAction, AccessDecision, AccessPolicy, AccessRequest, PluginManager, ProviderId, RoomId,
};
use acb_plugin_process_provider::{ProcessProviderConfig, ProcessProviderLoader};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn host_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acb-wasm-host"))
}

fn provider_config(module: &Path, provider: &str, timeout_ms: u64) -> ProcessProviderConfig {
    ProcessProviderConfig {
        id: provider.to_owned(),
        extension: "access".to_owned(),
        command: vec![
            host_bin().to_string_lossy().into_owned(),
            "--module".to_owned(),
            module.to_string_lossy().into_owned(),
            "--provider".to_owned(),
            provider.to_owned(),
        ],
        env: BTreeMap::new(),
        cwd: None,
        startup_timeout_secs: 5,
        timeout_ms,
    }
}

fn install_process(config: ProcessProviderConfig) -> (PluginManager, ProviderId) {
    let id = ProviderId::parse(&config.id).expect("provider id");
    let loader = Arc::new(ProcessProviderLoader::new(&[config]));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(loader)
        .expect("process provider should install");
    (plugins, id)
}

fn policy(id: &ProviderId) -> AccessPolicy {
    AccessPolicy {
        provider: id.clone(),
        options: serde_json::json!({}),
    }
}

fn subscriptions() -> Vec<RoomId> {
    vec![
        RoomId::parse("qq:group/1").expect("room 1"),
        RoomId::parse("qq:group/2").expect("room 2"),
    ]
}

fn access<'a>(room: Option<&'a RoomId>, subscriptions: &'a [RoomId]) -> AccessRequest<'a> {
    AccessRequest {
        session: "s1",
        connection: "qq",
        room,
        origin_room: room,
        subject: acb_kernel::GrantSubject::User,
        place: acb_kernel::GrantPlace::Group,
        action: AccessAction::Read,
        subscriptions,
    }
}

#[test]
fn wasm_access_provider_returns_expected_decisions() {
    let (plugins, id) = install_process(provider_config(
        &fixture("access.wasm"),
        "demo.wasm_access",
        3000,
    ));
    let policy = policy(&id);
    let provider = plugins
        .resolve_access(&id, &policy)
        .expect("resolve access");
    let subs = subscriptions();
    let room1 = &subs[0];
    let room2 = &subs[1];

    assert_eq!(
        provider.check(&policy, &access(Some(room1), &subs)),
        AccessDecision::Allow
    );
    assert_eq!(
        provider.check(&policy, &access(Some(room2), &subs)),
        AccessDecision::Deny
    );
    assert_eq!(
        provider.check(&policy, &access(None, &subs)),
        AccessDecision::Deny
    );
}

#[test]
fn wasm_trap_is_a_hard_failure() {
    let (plugins, id) = install_process(provider_config(
        &fixture("trap.wat"),
        "demo.wasm_trap",
        1000,
    ));
    let policy = policy(&id);
    let provider = plugins
        .resolve_access(&id, &policy)
        .expect("resolve access");
    let subs = subscriptions();
    assert_eq!(
        provider.check(&policy, &access(Some(&subs[0]), &subs)),
        AccessDecision::Deny,
        "trap 后必须 fail-closed"
    );

    let health = plugins
        .manager_health()
        .into_iter()
        .find(|entry| entry.namespace == "demo")
        .expect("manager health");
    assert!(
        matches!(&health.health, acb_kernel::Health::Unhealthy(detail) if detail.contains("trap") || detail.contains("exited")),
        "trap 必须把 provider 标成 Unhealthy：{:?}",
        health.health
    );
}

#[test]
fn missing_export_fails_before_manifest() {
    let output = Command::new(host_bin())
        .arg("--module")
        .arg(fixture("missing_export.wat"))
        .output()
        .expect("run host");
    assert!(!output.status.success(), "缺导出必须硬失败");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("missing export: acb_decide"),
        "错误要指出缺哪个导出：{stderr}"
    );

    let loader = Arc::new(ProcessProviderLoader::new(&[provider_config(
        &fixture("missing_export.wat"),
        "demo.missing_export",
        3000,
    )]));
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins
        .install_loader_owned(loader)
        .expect_err("缺导出的宿主不能装进 Root");
    assert!(
        error.to_string().contains("process exited before manifest"),
        "provider 加载错误要可读：{error}"
    );
}

#[test]
fn wasm_memory_limit_is_enforced() {
    let output = Command::new(host_bin())
        .arg("--module")
        .arg(fixture("memory_hog.wat"))
        .arg("--max-memory-bytes")
        .arg("65536")
        .output()
        .expect("run host");
    assert!(!output.status.success(), "内存超限必须硬失败");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("memory limit exceeded"),
        "错误要说明内存上限：{stderr}"
    );
}

#[cfg(feature = "native-so")]
fn lib_name(crate_name: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{crate_name}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{crate_name}.dll")
    } else {
        format!("lib{crate_name}.so")
    }
}

#[cfg(feature = "native-so")]
fn fixture_lib(crate_name: &str) -> PathBuf {
    let name = lib_name(crate_name);
    let exe = std::env::current_exe().expect("current exe");
    let deps = exe.parent().expect("deps dir");
    let target = deps.parent().expect("target dir");
    for candidate in [target.join(&name), deps.join(&name)] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "fixture {name} not found under {} or {}; run cargo test -p acb-wasm-host --features native-so",
        target.display(),
        deps.display()
    );
}

#[cfg(feature = "native-so")]
fn install_dynamic(path: &Path) -> (PluginManager, ProviderId) {
    let config = PluginConfig {
        path: path.to_string_lossy().into_owned(),
        kind: None,
        timeout_ms: None,
        version: None,
    };
    let loader = Arc::new(acb_adapter_dynamic::DynamicLoader::new(&[config]));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(loader)
        .expect("UNLOAD_SAFE v2 plugin should install");
    let id = ProviderId::parse("demo.access_allowlist").expect("provider id");
    (plugins, id)
}

#[cfg(feature = "native-so")]
#[test]
fn so_v2_provider_loads_registers_calls_and_unloads() {
    let (mut plugins, id) = install_dynamic(&fixture_lib("acb_access_so_fixture"));
    let policy = policy(&id);
    let provider = plugins
        .resolve_access(&id, &policy)
        .expect("resolve access");
    let subs = subscriptions();
    assert_eq!(
        provider.check(&policy, &access(Some(&subs[0]), &subs)),
        AccessDecision::Allow
    );
    assert_eq!(
        provider.check(&policy, &access(Some(&subs[1]), &subs)),
        AccessDecision::Deny
    );
    drop(provider);

    plugins
        .unload_provider(&id)
        .expect("UNLOAD_SAFE 必须能卸载");
    assert!(
        plugins.resolve_access(&id, &policy).is_err(),
        "卸载后不能再解析到 provider"
    );
}

#[cfg(feature = "native-so")]
#[test]
fn so_abi_mismatch_is_a_clear_error() {
    let path = fixture_lib("acb_access_so_bad_abi");
    let config = PluginConfig {
        path: path.to_string_lossy().into_owned(),
        kind: None,
        timeout_ms: None,
        version: None,
    };
    let loader = Arc::new(acb_adapter_dynamic::DynamicLoader::new(&[config]));
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins
        .install_loader_owned(loader)
        .expect_err("ABI 版本不匹配必须拒绝");
    let text = error.to_string().to_lowercase();
    assert!(
        text.contains("abi") || text.contains("version"),
        "错误要说明 ABI 不匹配：{text}"
    );
}

#[cfg(feature = "native-so")]
#[test]
fn wasm_and_so_agree_on_the_same_requests() {
    let (wasm_plugins, wasm_id) = install_process(provider_config(
        &fixture("access.wasm"),
        "demo.wasm_access",
        3000,
    ));
    let (so_plugins, so_id) = install_dynamic(&fixture_lib("acb_access_so_fixture"));
    let wasm_policy = policy(&wasm_id);
    let so_policy = policy(&so_id);
    let wasm = wasm_plugins
        .resolve_access(&wasm_id, &wasm_policy)
        .expect("wasm provider");
    let native = so_plugins
        .resolve_access(&so_id, &so_policy)
        .expect("so provider");
    let subs = subscriptions();

    for room in [Some(&subs[0]), Some(&subs[1]), None] {
        let wasm_decision = wasm.check(&wasm_policy, &access(room, &subs));
        let so_decision = native.check(&so_policy, &access(room, &subs));
        assert_eq!(
            wasm_decision, so_decision,
            "两种载体必须给出相同决策：room={room:?}"
        );
    }
}
