//! ABI v2 conformance：装载、调用、destroy、dlclose、重新装载。
//!
//! 需要 PATH 里有 C 编译器（cc / gcc）。没有就跳过，不让本地开发环境因为缺编译器失败。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_adapter_api::abi::{ABI_VERSION, HostV1};
use acb_adapter_dynamic::{DynamicLoader, UnloadSafeLibrary};
use acb_kernel::{PluginConfig, PluginLifecycle, PluginManager, ProviderId, WakePolicy};

fn temp_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时钟")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("acb-v2-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn compile(source: &Path, output: &Path, flags: Option<&str>) -> Result<(), String> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let mut command = Command::new(&cc);
    command
        .arg("-shared")
        .arg("-fPIC")
        .arg("-o")
        .arg(output)
        .arg(source);
    if let Some(flags) = flags {
        command.arg(flags);
    }
    let status = command.status().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("cc exited with {status}"));
    }
    Ok(())
}

fn host() -> HostV1 {
    HostV1 {
        abi_version: ABI_VERSION,
        ctx: ptr::null_mut(),
        submit_event: None,
        should_stop: None,
        log: None,
    }
}

#[test]
fn unload_safe_v2_plugin_can_be_loaded_invoked_and_reloaded() {
    let dir = temp_dir();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unload_v2.c");
    let library = dir.join("libunload_v2.so");
    if let Err(error) = compile(&source, &library, None) {
        eprintln!("skip ABI v2 conformance: compiler unavailable ({error})");
        return;
    }

    let path = library.to_str().expect("UTF-8 路径");
    let plugin = UnloadSafeLibrary::load(path).expect("UNLOAD_SAFE fixture 应当加载");
    assert!(
        plugin.manifest().expect("manifest").contains("test.unload"),
        "manifest 要原样返回"
    );
    let handle = plugin.create("{}", &host()).expect("create");
    // SAFETY: handle 来自同一 loader 的 create，且在下一次 destroy 前串行使用。
    let output = unsafe { plugin.invoke(handle, "{}") }.expect("invoke");
    assert!(output.contains("\"ok\":true"), "invoke 结果：{output}");
    // SAFETY: handle 只 destroy 一次，之后不再使用。
    unsafe { plugin.destroy(handle) };
    plugin.unload().expect("unload");

    // 卸载后重新 dlopen：至少证明 loader 没有把 Library 永久钉在静态表里。
    let again = UnloadSafeLibrary::load(path).expect("卸载后应当能重新加载");
    assert!(again.manifest().expect("manifest").contains("test.unload"));
    again.unload().expect("再次卸载");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unload_safe_v2_strategy_is_removed_from_root() {
    let dir = temp_dir();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unload_v2.c");
    let library = dir.join("libstrategy_v2.so");
    if let Err(error) = compile(&source, &library, None) {
        eprintln!("skip ABI v2 strategy test: compiler unavailable ({error})");
        return;
    }

    let config = PluginConfig {
        path: library.to_str().expect("UTF-8 路径").to_owned(),
        kind: None,
        timeout_ms: None,
        version: None,
    };
    let loader = Arc::new(DynamicLoader::new(&[config]));
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader_owned(loader.clone())
        .expect("v2 wake fixture 应当装进 Root");
    let id = ProviderId::parse("test.unload").expect("合法 provider id");
    assert!(
        plugins.resolve_wake(&id, &WakePolicy::default()).is_ok(),
        "安装后应当能解析 wake provider"
    );

    plugins
        .unload_provider(&id)
        .expect("Root 通过 loader 注册表卸载");
    assert!(
        plugins.resolve_wake(&id, &WakePolicy::default()).is_err(),
        "卸载后 manager 与 descriptor 都必须消失"
    );
    let info = plugins.info(&id).expect("卸载后仍应保留目录快照");
    assert_eq!(info.lifecycle, PluginLifecycle::Unloaded);
    assert!(
        plugins
            .catalog()
            .iter()
            .any(|entry| entry.id == id && entry.lifecycle == PluginLifecycle::Unloaded),
        "控制台目录要能看到已卸载状态"
    );

    plugins.reload_provider(&id).expect("应当能重新加载");
    assert!(
        plugins.resolve_wake(&id, &WakePolicy::default()).is_ok(),
        "重新加载后 provider 必须恢复"
    );
    assert_eq!(
        plugins.info(&id).expect("目录").lifecycle,
        PluginLifecycle::Unloadable
    );
    assert!(
        plugins.reload_provider(&id).is_err(),
        "已加载的 provider 不能重复重载"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugin_without_unload_safe_flag_is_refused() {
    let dir = temp_dir();
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/unload_v2.c");
    let library = dir.join("libnot_safe.so");
    if let Err(error) = compile(&source, &library, Some("-DPLUGIN_FLAGS=0")) {
        eprintln!("skip ABI v2 refusal test: compiler unavailable ({error})");
        return;
    }

    let result = UnloadSafeLibrary::load(library.to_str().expect("UTF-8 路径"));
    assert!(result.is_err(), "未声明 UNLOAD_SAFE 必须拒绝");
    let error = result.err().expect("刚刚确认是 Err");
    assert!(
        error.contains("UNLOAD_SAFE") || error.contains("unload"),
        "错误要说明原因：{error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
