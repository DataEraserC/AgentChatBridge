//! 策略落库（schema 13）：权限规则、房间政策、审批路由、管理员。
//!
//! 盯三件事：
//! 1. 写下去读回来还是同一份（含 `deny`、含空列表、含中文房间键）；
//! 2. 关掉再开还在，且「导入过一份空的」不会被当成「还没导入」；
//! 3. 单条增删改不会动到别的记录。
//!
//! 第 2 条是这套设计里最容易翻车的地方：如果没有 `policy_initialized` 标记，
//! 管理员把策略清空之后重启，桥会以为没导入过，拿 `config.toml` 的旧权限复活。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_kernel::{
    AccountGrant, GrantCapability, GrantRule, GrantScope, GrantSubject, GrantsConfig, Place,
    Policies, PolicyEffect, RoomGrant, SqliteStore, SystemAdminGrant, Trust,
};

const AT: i64 = 1_793_000_000_000;

struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("时间正常")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("acb-policy-{}-{name}-{nanos}", std::process::id()));
        Self {
            path: dir.join("acb.db"),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn rule(id: &str) -> GrantRule {
    GrantRule {
        id: id.to_owned(),
        subject: GrantSubject::User,
        place: Place::Room(acb_kernel::RoomScope::MultiParty),
        scope: GrantScope::Subscribed,
        target: None,
        capability: vec![GrantCapability::RoomRead, GrantCapability::RoomWrite],
        permissions: Vec::new(),
    }
}

fn sample() -> GrantsConfig {
    let mut grants = GrantsConfig::default();
    grants.rules.push(rule("allow-read"));
    grants.deny.push(GrantRule {
        id: "deny-secret".to_owned(),
        subject: GrantSubject::User,
        place: Place::Room(acb_kernel::RoomScope::MultiParty),
        scope: GrantScope::Subscribed,
        target: None,
        capability: vec![GrantCapability::AccountSecret],
        permissions: Vec::new(),
    });
    grants.rooms.insert(
        "qq:group/769843907".to_owned(),
        RoomGrant {
            trust: Trust::Trusted,
            members: vec!["323622442".to_owned()],
            approval_route: vec!["qq:private/323622442".to_owned()],
        },
    );
    // 空列表也要能存：它是「明确没有审批路由」的表达，不是缺失。
    grants
        .rooms
        .insert("qq:group/973928169".to_owned(), RoomGrant::default());
    grants.accounts.insert(
        "qq:10001".to_owned(),
        AccountGrant {
            approval_route: vec!["qq:group/498587179".to_owned()],
        },
    );
    grants.system_admin = Some(SystemAdminGrant {
        connection: "qq".to_owned(),
        account: "323622442".to_owned(),
    });
    grants
}

#[test]
fn empty_database_reports_not_imported() {
    let db = TempDb::new("empty");
    let store = SqliteStore::open(db.path()).expect("能打开");
    assert!(
        store.policy_load().expect("可读").is_none(),
        "一条记录都没有时要返回 None——调用方靠这个决定「首次导入」，空策略会把所有人降权"
    );
}

#[test]
fn replace_then_load_round_trips_everything() {
    let db = TempDb::new("roundtrip");
    let mut store = SqliteStore::open(db.path()).expect("能打开");
    let grants = sample();
    store.policy_replace(&grants).expect("能写");

    let loaded = store.policy_load().expect("可读").expect("导入过就有");
    assert_eq!(loaded.rules, grants.rules, "allow 规则要一字不差");
    assert_eq!(loaded.deny, grants.deny, "deny 也要存，不能只存 allow");
    assert_eq!(
        loaded.rooms, grants.rooms,
        "房间键（含中文/斜杠）与空列表都要保真"
    );
    assert_eq!(loaded.accounts, grants.accounts);
    assert_eq!(loaded.system_admin, grants.system_admin);
    assert!(loaded.bot_admin.is_none());
}

#[test]
fn policy_survives_restart() {
    let db = TempDb::new("restart");
    let grants = sample();
    {
        let mut store = SqliteStore::open(db.path()).expect("能打开");
        store.policy_replace(&grants).expect("能写");
    }
    let store = SqliteStore::open(db.path()).expect("能再打开");
    assert_eq!(
        store.policy_load().expect("可读"),
        Some(grants),
        "重启之后策略必须是同一份：桥按库判权限，读不到就等于全员失权"
    );
}

#[test]
fn clearing_policy_still_counts_as_imported() {
    let db = TempDb::new("cleared");
    {
        let mut store = SqliteStore::open(db.path()).expect("能打开");
        store.policy_replace(&sample()).expect("能写");
        // 管理员把规则、房间、账号全删干净。
        store
            .policy_replace(&GrantsConfig::default())
            .expect("能写");
    }

    let store = SqliteStore::open(db.path()).expect("能再打开");
    let loaded = store.policy_load().expect("可读");
    assert!(
        loaded.is_some(),
        "清空之后必须仍然是 Some(空)：否则重启会把 config.toml 的旧权限复活"
    );
    assert_eq!(loaded.unwrap(), GrantsConfig::default());
}

#[test]
fn single_rule_upsert_and_delete_touch_only_one_row() {
    let db = TempDb::new("rule-ops");
    let mut store = SqliteStore::open(db.path()).expect("能打开");
    store.policy_replace(&sample()).expect("能写");

    // 同 id 覆盖：能力换了，条数不变。
    let mut changed = rule("allow-read");
    changed.capability = vec![GrantCapability::RoomRead];
    store
        .policy_rule_put(PolicyEffect::Allow, &changed, AT)
        .expect("能写");
    let loaded = store.policy_load().expect("可读").expect("有");
    assert_eq!(loaded.rules.len(), 1, "同 id 是覆盖，不是新增");
    assert_eq!(loaded.rules[0].capability, vec![GrantCapability::RoomRead]);

    assert!(
        store
            .policy_rule_delete(PolicyEffect::Allow, "allow-read")
            .expect("能删")
    );
    assert!(
        !store
            .policy_rule_delete(PolicyEffect::Allow, "allow-read")
            .expect("能删"),
        "已经删掉了，再删要返回 false"
    );
    let loaded = store.policy_load().expect("可读").expect("有");
    assert!(loaded.rules.is_empty());
    assert_eq!(loaded.deny.len(), 1, "删 allow 不该碰 deny");
    assert_eq!(loaded.rooms.len(), 2, "也不该碰房间");
}

#[test]
fn approval_route_edit_is_visible_immediately() {
    let db = TempDb::new("route");
    let mut store = SqliteStore::open(db.path()).expect("能打开");
    store.policy_replace(&sample()).expect("能写");

    // 2026-11-09 现场：769843907 没配审批路由，票据落到没人看的适配器私聊。
    // 现在补一条，读回来必须立刻是新的——不再有「创建时快照救了不旧票」那套。
    store
        .policy_room_put(
            "qq:group/769843907",
            &RoomGrant {
                trust: Trust::Untrusted,
                members: vec![],
                approval_route: vec![
                    "qq:private/323622442".to_owned(),
                    "qq:group/498587179".to_owned(),
                ],
            },
            AT,
        )
        .expect("能写");

    let loaded = store.policy_load().expect("可读").expect("有");
    assert_eq!(
        loaded.rooms["qq:group/769843907"].approval_route,
        vec![
            "qq:private/323622442".to_owned(),
            "qq:group/498587179".to_owned()
        ]
    );
    assert_eq!(loaded.rooms.len(), 2, "同房间是覆盖，不该多出一条");
}

#[test]
fn room_and_account_delete_report_whether_anything_went() {
    let db = TempDb::new("delete");
    let mut store = SqliteStore::open(db.path()).expect("能打开");
    store.policy_replace(&sample()).expect("能写");

    assert!(
        store
            .policy_room_delete("qq:group/973928169")
            .expect("能删")
    );
    assert!(
        !store
            .policy_room_delete("qq:group/973928169")
            .expect("能删")
    );
    assert!(store.policy_account_delete("qq:10001").expect("能删"));
    assert!(!store.policy_account_delete("qq:10001").expect("能删"));

    let loaded = store.policy_load().expect("可读").expect("有");
    assert_eq!(loaded.rooms.len(), 1);
    assert!(loaded.accounts.is_empty());
    assert_eq!(loaded.rules.len(), 1, "删房间不该碰规则");
}

#[test]
fn system_admin_can_be_cleared() {
    let db = TempDb::new("admin");
    let mut store = SqliteStore::open(db.path()).expect("能打开");
    store.policy_replace(&sample()).expect("能写");
    store.policy_system_admin_put(None, AT).expect("能清");

    let loaded = store.policy_load().expect("可读").expect("有");
    assert!(loaded.system_admin.is_none());
}

#[test]
fn schema_version_covers_policy_tables() {
    // 这条不钉死具体数字（每加一张表都要改一次测试，改的人还会顺手把断言改成新值，
    // 等于没看守）。它守的是「policy_* 引入之后版本号至少是 13」：有人把版本号
    // 往回写会让老库被当成新库打开。
    assert!(
        acb_kernel::SCHEMA_VERSION >= 13,
        "policy_* 四张表随 schema 13 引入；版本号不能低于它（当前 {}）",
        acb_kernel::SCHEMA_VERSION
    );
}
