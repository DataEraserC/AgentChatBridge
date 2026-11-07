//! notifier 宿主测试：用假子进程确认事件按行 JSON 送出去，且不阻塞调用方。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use acb_kernel::{InBandNoticeConfig, NoticeEvent, NoticeSink, NotificationsConfig};
use acb_plugin_process_tools::ProcessNotifier;

fn script_path() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "acb-notifier-host-{}-{}.sh",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos()
    ));
    fs::write(&path, "#!/bin/sh\ncat > \"$ACB_NOTIFIER_HOST_OUT\"\n").expect("能写脚本");
    let mut permissions = fs::metadata(&path).expect("有元数据").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("能加执行权限");
    path
}

#[test]
fn inactive_configuration_never_spawns_a_child() {
    let config = NotificationsConfig {
        enabled: true,
        url: Some("http://127.0.0.1:8080".to_owned()),
        topic: None,
        topic_source: None,
        token: None,
        min_interval_secs: 600,
        in_band: InBandNoticeConfig::default(),
    };
    assert!(
        ProcessNotifier::spawn(&config).is_err(),
        "缺 topic 时不该启动子进程"
    );
}

#[test]
fn host_forwards_events_as_json_lines() {
    let script = script_path();
    let out = script.with_extension("jsonl");
    // SAFETY: 测试进程串行跑这一个宿主用例；设完即取，子进程启动后就不再依赖它。
    unsafe {
        std::env::set_var("ACB_NOTIFIER_BIN", &script);
        std::env::set_var("ACB_NOTIFIER_HOST_OUT", &out);
    }
    let config = NotificationsConfig {
        enabled: true,
        url: Some("http://127.0.0.1:8080".to_owned()),
        topic: Some("acb".to_owned()),
        topic_source: None,
        token: Some("secret".to_owned()),
        min_interval_secs: 600,
        in_band: InBandNoticeConfig::default(),
    };
    let notifier = Arc::new(ProcessNotifier::spawn(&config).expect("能启动假子进程"));
    unsafe {
        std::env::remove_var("ACB_NOTIFIER_BIN");
        std::env::remove_var("ACB_NOTIFIER_HOST_OUT");
    }

    notifier.notify(NoticeEvent::DependencyEdge {
        kind: "platform".to_owned(),
        id: "qq".to_owned(),
        from: "up".to_owned(),
        to: "down".to_owned(),
        since: 1_793_772_538_000,
        last_ok_at: None,
        class: Some("unreachable".to_owned()),
        detail: Some("connect refused".to_owned()),
        attempts: 3,
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0) == 0 {
        assert!(Instant::now() < deadline, "子进程没有收到事件");
        thread::sleep(Duration::from_millis(20));
    }
    let text = fs::read_to_string(&out).expect("能读输出");
    let line: serde_json::Value = serde_json::from_str(text.trim()).expect("是 JSON");
    assert_eq!(line["jsonrpc"], "2.0");
    assert_eq!(line["method"], "notify");
    assert_eq!(line["params"]["event"], "dependency_edge");
    assert_eq!(line["params"]["kind"], "platform");
    assert_eq!(line["params"]["id"], "qq");
    assert_eq!(notifier.dropped(), 0);

    drop(notifier);
    let _ = fs::remove_file(script);
    let _ = fs::remove_file(out);
}
