//! 外置进程工具载体的集成测试。夹具是一个同包的 bin，按参数模拟正常/挂起/崩溃。

use std::time::{Duration, Instant};

use acb_kernel::{
    ErrorCode, PluginError, PluginManager, ProviderId, RoomId, ToolCallContext, ToolPluginConfig,
    ToolProvider,
};
use acb_plugin_process_tools::{Backoff, ProcessToolLoader, ProcessToolProvider};

fn fixture() -> String {
    env!("CARGO_BIN_EXE_process-tool-fixture").to_owned()
}

fn config(id: &str, args: &[&str], timeout_ms: u64) -> ToolPluginConfig {
    ToolPluginConfig {
        id: id.to_owned(),
        command: fixture(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        timeout_ms: Some(timeout_ms),
    }
}

fn unique_marker(name: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir()
        .join(format!("acb-process-tool-{name}-{nanos}"))
        .to_string_lossy()
        .into_owned()
}

fn context() -> (RoomId, String) {
    (
        RoomId::parse("qq:group/1").expect("房间合法"),
        "main".to_owned(),
    )
}

#[test]
fn install_lists_tools_and_calls() {
    let provider =
        ProcessToolProvider::spawn(config("process.echo", &["echo"], 500)).expect("夹具应能启动");
    let tools = provider.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let (room, session) = context();
    let context = ToolCallContext {
        session: &session,
        room: &room,
        origin_sender: None,
        live: true,
    };
    let value = provider
        .call(&context, "echo", serde_json::json!({"hello": "world"}))
        .expect("调用应成功");
    assert_eq!(value, serde_json::json!({"hello": "world"}));
    provider.shutdown();
    assert!(!provider.is_alive());
}

#[test]
fn call_timeout_marks_unhealthy_and_returns_fast() {
    let provider = ProcessToolProvider::spawn_with_backoff(
        config("process.hang", &["hang"], 200),
        Backoff::new(vec![1000]),
    )
    .expect("夹具应能启动");
    let (room, session) = context();
    let context = ToolCallContext {
        session: &session,
        room: &room,
        origin_sender: None,
        live: true,
    };
    let started = Instant::now();
    let error = provider
        .call(&context, "echo", serde_json::json!({"hello": "world"}))
        .expect_err("挂起应超时");
    assert_eq!(error.code, ErrorCode::Timeout);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(
        matches!(provider.health(), acb_kernel::Health::Unhealthy(_)),
        "超时后应标记 Unhealthy"
    );

    let started = Instant::now();
    let second = provider
        .call(&context, "echo", serde_json::json!({}))
        .expect_err("Unhealthy 期间应立即失败");
    assert!(started.elapsed() < Duration::from_millis(100));
    assert!(second.message.contains("unavailable"));
    provider.shutdown();
}

#[test]
fn crash_restarts_with_backoff_and_recovers() {
    let marker = unique_marker("crash-once");
    let provider = ProcessToolProvider::spawn_with_backoff(
        config("process.crash", &["crash-once", &marker], 300),
        Backoff::new(vec![50, 100]),
    )
    .expect("夹具应能启动");
    let (room, session) = context();
    let context = ToolCallContext {
        session: &session,
        room: &room,
        origin_sender: None,
        live: true,
    };
    let first = provider
        .call(&context, "echo", serde_json::json!({"hello": "world"}))
        .expect_err("第一次应崩溃");
    assert_eq!(first.code, ErrorCode::Internal);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !provider.is_alive() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(provider.is_alive(), "退避后应重启成功");
    let recovered = provider
        .call(&context, "echo", serde_json::json!({"after": "restart"}))
        .expect("重启后应恢复");
    assert_eq!(recovered, serde_json::json!({"after": "restart"}));
    provider.shutdown();
    let _ = std::fs::remove_file(marker);
}

#[test]
fn duplicate_tool_name_rejected() {
    let loader = ProcessToolLoader::new(&[
        config("process.one", &["echo"], 300),
        config("process.two", &["echo"], 300),
    ]);
    let mut plugins = PluginManager::with_native_builtins();
    let error = plugins
        .install_loader(&loader)
        .expect_err("两个 provider 声明同一个工具名应被拒绝");
    assert!(
        matches!(error, PluginError::ToolNameConflict { .. }),
        "错误应是工具名冲突：{error}"
    );
}

#[test]
fn loader_registers_provider_and_shutdown_works() {
    let loader = ProcessToolLoader::new(&[config("process.echo", &["echo"], 300)]);
    let mut plugins = PluginManager::with_native_builtins();
    plugins
        .install_loader(&loader)
        .expect("合法 loader 应能安装");
    let info = plugins
        .info(&ProviderId::parse("process.echo").expect("合法 id"))
        .expect("目录里应有工具 provider");
    assert_eq!(info.extension.as_str(), "tool");
    assert_eq!(
        loader
            .provider(&ProviderId::parse("process.echo").expect("合法 id"))
            .expect("能取到 provider")
            .tools()
            .len(),
        1
    );
    let provider = loader
        .provider(&ProviderId::parse("process.echo").expect("合法 id"))
        .expect("能取到 provider");
    assert!(provider.is_alive());
    provider.shutdown();
    assert!(!provider.is_alive());
}
