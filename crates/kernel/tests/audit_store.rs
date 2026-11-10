//! 审计落盘：写入顺序、最近 N 条、字段完整。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::storage::{AuditEntry, AuditStore, MemoryStore, SqliteStore};

const AT: i64 = 1_793_000_000_000;

fn entry(capability: &str, decision: &str) -> AuditEntry {
    AuditEntry {
        at: AT,
        session: Some("dsh-dev".to_owned()),
        subject: "system_admin".to_owned(),
        place: "private:qq:323622442".to_owned(),
        capability: capability.to_owned(),
        target_kind: "connection".to_owned(),
        target_id: "qq".to_owned(),
        decision: decision.to_owned(),
        basis: "rule:admin-config".to_owned(),
        policy_version: 3,
        approver: None,
        request_id: None,
    }
}

#[test]
fn memory_audit_is_ordered_and_limited() {
    let mut store = MemoryStore::new();
    store
        .audit_put(&entry("config_read", "allow"))
        .expect("能写");
    store
        .audit_put(&entry("config_write", "deny"))
        .expect("能写");
    let recent = store.audit_recent(1).expect("能读");
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].capability, "config_write");
}

#[test]
fn sqlite_audit_roundtrip() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时间正常")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("acb-audit-{}-{nanos}", std::process::id()));
    let path: PathBuf = dir.join("acb.db");

    let mut store = SqliteStore::open(&path).expect("能打开");
    store
        .audit_put(&entry("config_read", "allow"))
        .expect("能写");
    store
        .audit_put(&entry("plugin_admin", "deny"))
        .expect("能写");
    drop(store);

    let store = SqliteStore::open(&path).expect("能再开");
    let recent = store.audit_recent(10).expect("能读");
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].capability, "plugin_admin");
    assert_eq!(recent[0].decision, "deny");
    assert_eq!(recent[1].basis, "rule:admin-config");
    let _ = std::fs::remove_dir_all(dir);
}
