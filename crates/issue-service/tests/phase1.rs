//! 阶段 1 验收：鉴权、状态机、检索、outbox、超时与备份。
//!
//! 这些测试直接用库函数和临时 SQLite 文件，不依赖 ACB 进程。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use acb_issue_service::{
    ALL_SCOPES, CLAIM_STALE_MS, CreateIssue, CreateSubscription, CreateWorkItem, IssueError,
    MyWorkQuery, ReportResult, SCOPE_ADMIN, SCOPE_READ, SCOPE_WRITE, SearchQuery, UpdateIssue,
    VerifyWorkItem, ack_events, authenticate, create_issue, create_subscription, create_token,
    create_work_item, get_issue, migrate, my_work, now_ms, open_db, report_result, require_scope,
    revoke_token, rotate_token, run_due, search_issues, update_issue, verify_work_item,
};

const ADMIN: &str = "admin";

fn temp_path(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("acb-issue-{tag}-{stamp}.db"))
}

fn cleanup(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

fn memory_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    conn
}

fn admin_token(conn: &rusqlite::Connection, now: i64) -> String {
    create_token(
        conn,
        ADMIN,
        "admin",
        &ALL_SCOPES
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>(),
        now + 365 * 24 * 3600 * 1000,
        now,
        None,
    )
    .unwrap()
    .plaintext
}

fn principal(conn: &rusqlite::Connection, id: &str, scopes: &[&str], now: i64) -> String {
    create_token(
        conn,
        id,
        id,
        &scopes
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect::<Vec<_>>(),
        now + 90 * 24 * 3600 * 1000,
        now,
        None,
    )
    .unwrap()
    .plaintext
}

fn issue_input(title: &str, reporter: &str) -> CreateIssue {
    CreateIssue {
        title: title.to_owned(),
        body: format!("body of {title}"),
        priority: None,
        tags: vec!["bug".to_owned()],
        assignee: None,
        reporter: Some(reporter.to_owned()),
        source: None,
        related: None,
        verification_owner: None,
        on_behalf_of: None,
    }
}

#[test]
fn empty_database_migrates_and_survives_restart() {
    let path = temp_path("restart");
    let now = 1_000_000_i64;
    let issue_id;
    {
        let conn = open_db(&path).unwrap();
        let token = admin_token(&conn, now);
        let actor = authenticate(&conn, &token, now).unwrap();
        let issue =
            create_issue(&conn, issue_input("重启后还在", &actor.id), &actor.id, now).unwrap();
        issue_id = issue.id;
        assert_eq!(issue.status, "new");
        assert_eq!(issue.version, 1);
    }
    {
        let conn = open_db(&path).unwrap();
        let issue = get_issue(&conn, &issue_id).unwrap();
        assert_eq!(issue.title, "重启后还在");
        assert_eq!(issue.status, "new");
    }
    cleanup(&path);
}

#[test]
fn token_401_403_expiry_rotation_and_service_scope() {
    let conn = memory_db();
    let now = 1_000_000_i64;

    assert!(matches!(
        authenticate(&conn, "not-a-token", now),
        Err(IssueError::Unauthorized)
    ));
    assert!(matches!(
        authenticate(&conn, "", now),
        Err(IssueError::Unauthorized)
    ));

    let reader = principal(&conn, "reader", &[SCOPE_READ], now);
    let read_principal = authenticate(&conn, &reader, now).unwrap();
    assert!(read_principal.has(SCOPE_READ));
    assert!(matches!(
        require_scope(&read_principal, SCOPE_WRITE),
        Err(IssueError::Forbidden(_))
    ));

    let expired = create_token(
        &conn,
        "reader",
        "old",
        &[SCOPE_READ.to_owned()],
        now - 1,
        now,
        None,
    )
    .unwrap();
    assert!(matches!(
        authenticate(&conn, &expired.plaintext, now),
        Err(IssueError::Unauthorized)
    ));

    // 撤销一张有效 token：认证立刻 401。
    let revocable = principal(&conn, "reader", &[SCOPE_READ], now);
    let principal_before = authenticate(&conn, &revocable, now).unwrap();
    revoke_token(&conn, &principal_before.token_id, now).unwrap();
    assert!(matches!(
        authenticate(&conn, &revocable, now),
        Err(IssueError::Unauthorized)
    ));

    // 轮换：旧 token 保留 7 天重叠，新 token 可用。
    let old = principal(&conn, "agent", &[SCOPE_READ, SCOPE_WRITE], now);
    let old_principal = authenticate(&conn, &old, now).unwrap();
    let rotated = rotate_token(&conn, &old_principal.token_id, now).unwrap();
    assert!(
        authenticate(&conn, &old, now).is_ok(),
        "重叠期内旧 token 仍有效"
    );
    assert!(
        authenticate(&conn, &old, now + 7 * 24 * 3600 * 1000).is_err(),
        "重叠期结束后旧 token 失效"
    );
    assert!(
        authenticate(&conn, &rotated.plaintext, now + 7 * 24 * 3600 * 1000).is_ok(),
        "新 token 继续有效"
    );

    // service token 不得含 admin。
    let denied = create_token(
        &conn,
        "acb-bridge",
        "service",
        &[SCOPE_READ.to_owned(), SCOPE_ADMIN.to_owned()],
        now + 90 * 24 * 3600 * 1000,
        now,
        None,
    );
    assert!(matches!(denied, Err(IssueError::BadRequest(_))));
}

#[test]
fn state_machine_conflicts_and_verifier_rules() {
    let conn = memory_db();
    let now = 1_000_000_i64;

    let _creator = principal(&conn, "creator", &[SCOPE_READ, SCOPE_WRITE], now);
    let _executor = principal(&conn, "executor", &[SCOPE_READ, SCOPE_WRITE], now);
    let _outsider = principal(&conn, "outsider", &[SCOPE_READ, SCOPE_WRITE], now);
    let admin = admin_token(&conn, now);

    let issue = create_issue(&conn, issue_input("状态机", "creator"), "creator", now).unwrap();
    assert!(matches!(
        update_issue(
            &conn,
            &issue.id,
            UpdateIssue {
                expected_version: 1,
                title: None,
                body: None,
                priority: None,
                tags: None,
                assignee: None,
                status: Some("done".to_owned()),
                resolution: None,
                verification_owner: None,
                comment: None,
                on_behalf_of: None,
            },
            "creator",
            false,
            ADMIN,
            now,
        ),
        Err(IssueError::InvalidTransition { .. })
    ));

    let started = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 1,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: Some(Some("executor".to_owned())),
            status: Some("in_progress".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(started.status, "in_progress");
    assert_eq!(started.version, 2);

    // 旧版本写入必须 409，不静默覆盖。
    let stale = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 1,
            title: Some("偷偷覆盖".to_owned()),
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: None,
            resolution: None,
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    );
    assert!(matches!(stale, Err(IssueError::StaleVersion)));
    assert_eq!(get_issue(&conn, &issue.id).unwrap().title, "状态机");

    let item = create_work_item(
        &conn,
        &issue.id,
        CreateWorkItem {
            title: "切片".to_owned(),
            body: None,
            priority: None,
            tags: Vec::new(),
            assignee: Some("executor".to_owned()),
            acceptance: "结果可复现".to_owned(),
            related: None,
            on_behalf_of: None,
        },
        "creator",
        now,
    )
    .unwrap();
    let reported = report_result(
        &conn,
        &item.id,
        ReportResult {
            status: Some("pending_verification".to_owned()),
            result: "已经修好".to_owned(),
            idempotency_key: "run-1".to_owned(),
            on_behalf_of: None,
        },
        "executor",
        now,
    )
    .unwrap();
    assert_eq!(reported["ok"], true);

    let pending = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 2,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("pending_verification".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "executor",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(pending.status, "pending_verification");
    assert_eq!(pending.version, 3);
    assert_eq!(
        pending.verification_due_at,
        Some(now + 72 * 3600 * 1000),
        "进入待验收要写 72h 截止"
    );

    // 执行者不能自验收；创建者可以。
    let work_item_id = item.id.clone();
    let denied_verification = verify_work_item(
        &conn,
        &work_item_id,
        VerifyWorkItem {
            passed: true,
            note: "复现通过".to_owned(),
            on_behalf_of: None,
        },
        "executor",
        false,
        ADMIN,
        now,
    );
    assert!(matches!(denied_verification, Err(IssueError::Forbidden(_))));

    let item = verify_work_item(
        &conn,
        &work_item_id,
        VerifyWorkItem {
            passed: true,
            note: "复现通过".to_owned(),
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(item.status, "done");

    let done = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 3,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("done".to_owned()),
            resolution: Some("验收通过".to_owned()),
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(done.status, "done");
    assert_eq!(done.closed_at, Some(now));

    // done 不能直接 closed；先 reopen 到 in_progress。
    let reopened_done = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 4,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("in_progress".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: Some("重新打开".to_owned()),
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(reopened_done.status, "in_progress");
    assert_eq!(reopened_done.closed_at, None);
    assert_eq!(reopened_done.version, 5);

    // closed 必须写 resolution；closed 之后还能 reopen 回 in_progress。
    let closed = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 5,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("closed".to_owned()),
            resolution: Some("重复问题".to_owned()),
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(closed.status, "closed");

    let reopened = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 6,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("in_progress".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: Some("再次打开".to_owned()),
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    assert_eq!(reopened.status, "in_progress");
    assert_eq!(reopened.closed_at, None);
    assert_eq!(reopened.version, 7);

    // 非验收者（outsider）不能把别人的 issue 推到 done。
    let other = create_issue(&conn, issue_input("另一条", "creator"), "creator", now).unwrap();
    let mut version = other.version;
    for status in ["in_progress", "pending_verification"] {
        if status == "pending_verification" {
            let wi = create_work_item(
                &conn,
                &other.id,
                CreateWorkItem {
                    title: "切片".to_owned(),
                    body: None,
                    priority: None,
                    tags: Vec::new(),
                    assignee: Some("executor".to_owned()),
                    acceptance: "".to_owned(),
                    related: None,
                    on_behalf_of: None,
                },
                "creator",
                now,
            )
            .unwrap();
            report_result(
                &conn,
                &wi.id,
                ReportResult {
                    status: None,
                    result: "完成".to_owned(),
                    idempotency_key: "run-2".to_owned(),
                    on_behalf_of: None,
                },
                "executor",
                now,
            )
            .unwrap();
        }
        version = update_issue(
            &conn,
            &other.id,
            UpdateIssue {
                expected_version: version,
                title: None,
                body: None,
                priority: None,
                tags: None,
                assignee: Some(Some("executor".to_owned())),
                status: Some(status.to_owned()),
                resolution: None,
                verification_owner: None,
                comment: None,
                on_behalf_of: None,
            },
            "creator",
            false,
            ADMIN,
            now,
        )
        .unwrap()
        .version;
    }
    let denied = update_issue(
        &conn,
        &other.id,
        UpdateIssue {
            expected_version: version,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("done".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "outsider",
        false,
        ADMIN,
        now,
    );
    assert!(matches!(denied, Err(IssueError::Forbidden(_))));

    let _ = admin;
}

#[test]
fn search_filters_and_cursor_pagination() {
    let conn = memory_db();
    let now = 1_000_000_i64;
    for index in 0..5 {
        let mut input = issue_input(&format!("登录失败 {index}"), "creator");
        input.tags = if index % 2 == 0 {
            vec!["login".to_owned()]
        } else {
            vec!["billing".to_owned()]
        };
        input.body = format!("中文正文 {index}");
        let issue = create_issue(&conn, input, "creator", now + index).unwrap();
        update_issue(
            &conn,
            &issue.id,
            UpdateIssue {
                expected_version: issue.version,
                title: None,
                body: None,
                priority: None,
                tags: None,
                assignee: Some(Some("agent".to_owned())),
                status: Some("in_progress".to_owned()),
                resolution: None,
                verification_owner: None,
                comment: None,
                on_behalf_of: None,
            },
            "creator",
            false,
            ADMIN,
            now + index,
        )
        .unwrap();
    }

    let by_tag = search_issues(
        &conn,
        &SearchQuery {
            tags: vec!["login".to_owned()],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_tag.issues.len(), 3);

    let by_assignee = search_issues(
        &conn,
        &SearchQuery {
            assignee: Some("agent".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_assignee.issues.len(), 5);

    let by_text = search_issues(
        &conn,
        &SearchQuery {
            q: Some("登录失败".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_text.issues.len(), 5, "中文关键词必须命中");

    let by_time = search_issues(
        &conn,
        &SearchQuery {
            since: Some(now + 2),
            until: Some(now + 4),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_time.issues.len(), 3);

    // 写入过程中翻页：用游标拿剩下的，不跳条不重条。
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = search_issues(
            &conn,
            &SearchQuery {
                limit: Some(2),
                cursor: cursor.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        for issue in page.issues {
            seen.push(issue.id);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen.len(), 5);
    let unique: std::collections::BTreeSet<_> = seen.iter().collect();
    assert_eq!(unique.len(), 5, "游标分页不能重复");
}

#[test]
fn outbox_is_idempotent_claimable_and_rcs_after_restart() {
    let conn = memory_db();
    let now = 1_000_000_i64;
    create_subscription(
        &conn,
        CreateSubscription {
            subscriber_id: "session:s1".to_owned(),
            channel: "acb_session".to_owned(),
            filter: serde_json::json!({}),
        },
        now,
    )
    .unwrap();

    let issue = create_issue(&conn, issue_input("outbox", "creator"), "creator", now).unwrap();
    let item = create_work_item(
        &conn,
        &issue.id,
        CreateWorkItem {
            title: "执行".to_owned(),
            body: None,
            priority: None,
            tags: Vec::new(),
            assignee: Some("executor".to_owned()),
            acceptance: "".to_owned(),
            related: None,
            on_behalf_of: None,
        },
        "creator",
        now,
    )
    .unwrap();
    report_result(
        &conn,
        &item.id,
        ReportResult {
            status: None,
            result: "完成".to_owned(),
            idempotency_key: "same-key".to_owned(),
            on_behalf_of: None,
        },
        "executor",
        now,
    )
    .unwrap();
    let duplicate = report_result(
        &conn,
        &item.id,
        ReportResult {
            status: None,
            result: "完成".to_owned(),
            idempotency_key: "same-key".to_owned(),
            on_behalf_of: None,
        },
        "executor",
        now,
    )
    .unwrap();
    assert_eq!(duplicate["duplicate"], true);

    let events = acb_issue_service::events_for_issue(&conn, &issue.id).unwrap();
    assert_eq!(events.len(), 3, "created + workitem.created + result");
    let pending: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event_deliveries WHERE state='pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, 3, "同一幂等键不能产生第二条 outbox");

    let mut query = MyWorkQuery {
        subscriber: Some("session:s1".to_owned()),
        channel: Some("acb_session".to_owned()),
        ..Default::default()
    };
    let work = my_work(&conn, "acb-bridge", &query, now).unwrap();
    assert_eq!(work.unread_events.count, 3);
    assert_eq!(work.unread_events.event_ids.len(), 3);

    // ack 幂等：第二次返回 already。
    query.ack_event_ids = work.unread_events.event_ids.clone();
    let acked = ack_events(
        &conn,
        "session:s1",
        "acb_session",
        &work.unread_events.event_ids,
        now,
    )
    .unwrap();
    assert_eq!(acked["acked"], 3);
    let again = ack_events(
        &conn,
        "session:s1",
        "acb_session",
        &work.unread_events.event_ids,
        now,
    )
    .unwrap();
    assert_eq!(again["already"], 3);

    let after = my_work(&conn, "acb-bridge", &query, now).unwrap();
    assert_eq!(after.unread_events.count, 0, "ack 后不再重复投递");

    // claim 超过 5 分钟回 pending：模拟一个未 ack 的 claim。
    let issue2 = create_issue(&conn, issue_input("claim", "creator"), "creator", now).unwrap();
    let claimed = my_work(
        &conn,
        "acb-bridge",
        &MyWorkQuery {
            subscriber: Some("session:s1".to_owned()),
            channel: Some("acb_session".to_owned()),
            ..Default::default()
        },
        now,
    )
    .unwrap();
    assert_eq!(claimed.unread_events.count, 1, "新事件 1 条");
    assert_eq!(claimed.unread_events.events[0]["issue_id"], issue2.id);
    let redelivered = my_work(
        &conn,
        "acb-bridge",
        &MyWorkQuery {
            subscriber: Some("session:s1".to_owned()),
            channel: Some("acb_session".to_owned()),
            ..Default::default()
        },
        now + 1,
    )
    .unwrap();
    assert_eq!(
        redelivered.unread_events.count, 1,
        "未 ack 的 claim 仍算未读"
    );
    let requeued = my_work(
        &conn,
        "acb-bridge",
        &MyWorkQuery {
            subscriber: Some("session:s1".to_owned()),
            channel: Some("acb_session".to_owned()),
            ..Default::default()
        },
        now + CLAIM_STALE_MS + 1,
    )
    .unwrap();
    assert_eq!(requeued.unread_events.count, 1, "5 分钟后回到 pending");
}

#[test]
fn verification_escalates_at_72h_and_closes_at_168h() {
    let conn = memory_db();
    let now = 1_000_000_i64;

    let issue = create_issue(&conn, issue_input("超时", "creator"), "creator", now).unwrap();
    update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 1,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: Some(Some("executor".to_owned())),
            status: Some("in_progress".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now,
    )
    .unwrap();
    let item = create_work_item(
        &conn,
        &issue.id,
        CreateWorkItem {
            title: "执行".to_owned(),
            body: None,
            priority: None,
            tags: Vec::new(),
            assignee: Some("executor".to_owned()),
            acceptance: "".to_owned(),
            related: None,
            on_behalf_of: None,
        },
        "creator",
        now,
    )
    .unwrap();
    report_result(
        &conn,
        &item.id,
        ReportResult {
            status: None,
            result: "完成".to_owned(),
            idempotency_key: "timeout-run".to_owned(),
            on_behalf_of: None,
        },
        "executor",
        now,
    )
    .unwrap();
    update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: 2,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("pending_verification".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: None,
            on_behalf_of: None,
        },
        "executor",
        false,
        ADMIN,
        now,
    )
    .unwrap();

    let before = run_due(&conn, now + 72 * 3600 * 1000 - 1, ADMIN).unwrap();
    assert_eq!(before.escalated, 0);
    assert_eq!(before.auto_closed, 0);

    let escalated = run_due(&conn, now + 72 * 3600 * 1000 + 1, ADMIN).unwrap();
    assert_eq!(escalated.escalated, 2, "issue + work item 都升级");
    let issue_now = get_issue(&conn, &issue.id).unwrap();
    assert!(issue_now.verification_escalated_at.is_some());
    assert_eq!(issue_now.status, "pending_verification");

    let auto = run_due(
        &conn,
        now + 72 * 3600 * 1000 + 1 + 96 * 3600 * 1000 + 1,
        ADMIN,
    )
    .unwrap();
    assert_eq!(auto.auto_closed, 2, "issue + work item 都自动关闭");
    let closed = get_issue(&conn, &issue.id).unwrap();
    assert_eq!(closed.status, "closed");
    assert_eq!(closed.resolution.as_deref(), Some("verification_timeout"));

    let reopened = update_issue(
        &conn,
        &issue.id,
        UpdateIssue {
            expected_version: closed.version,
            title: None,
            body: None,
            priority: None,
            tags: None,
            assignee: None,
            status: Some("in_progress".to_owned()),
            resolution: None,
            verification_owner: None,
            comment: Some("reopen".to_owned()),
            on_behalf_of: None,
        },
        "creator",
        false,
        ADMIN,
        now + 200 * 3600 * 1000,
    );
    assert!(reopened.is_ok(), "closed 之后允许 reopen");
}

#[test]
fn backup_snapshot_is_readable() {
    let path = temp_path("backup-source");
    let target = temp_path("backup-target");
    let now = 1_000_000_i64;
    let issue_id;
    {
        let conn = open_db(&path).unwrap();
        let issue = create_issue(&conn, issue_input("备份", "creator"), "creator", now).unwrap();
        issue_id = issue.id;
        acb_issue_service::backup_database(&conn, &target.to_string_lossy()).unwrap();
    }
    {
        let conn = open_db(&target).unwrap();
        assert_eq!(get_issue(&conn, &issue_id).unwrap().title, "备份");
    }
    let duplicate = {
        let conn = open_db(&path).unwrap();
        acb_issue_service::backup_database(&conn, &target.to_string_lossy())
    };
    assert!(duplicate.is_err(), "备份目标已存在时不得覆盖");
    cleanup(&path);
    cleanup(&target);
}

#[test]
fn ids_and_now_are_stable() {
    assert!(now_ms() > 0);
    let conn = memory_db();
    let issue = create_issue(&conn, issue_input("id", "creator"), "creator", 42).unwrap();
    assert!(issue.id.starts_with("is_"));
}
