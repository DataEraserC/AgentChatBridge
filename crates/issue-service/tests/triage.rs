//! 工作项视图相关的验收：类型 / 里程碑三态、评论、关联、筛选，以及自带的 Web UI。
//!
//! 这批字段是照着 GitHub issue + Azure work item 的日常用法加的，所以测的是
//! 「用起来对不对」，不是「字段存下来了没有」。

use acb_issue_service::{
    CommentInput, CreateIssue, CreateLink, IssueError, SearchQuery, UI_HTML, UpdateIssue,
    add_comment, comments_for_issue, create_issue, create_link, delete_link, events_for_issue,
    get_issue, list_links, migrate, search_issues,
};

const ADMIN: &str = "admin";
const NOW: i64 = 1_700_000_000_000;

fn memory_db() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    migrate(&conn).unwrap();
    conn
}

fn issue(conn: &rusqlite::Connection, title: &str) -> acb_issue_service::Issue {
    create_issue(
        conn,
        CreateIssue {
            title: title.to_owned(),
            body: String::new(),
            priority: None,
            kind: None,
            milestone: None,
            tags: Vec::new(),
            assignee: None,
            reporter: None,
            source: None,
            related: None,
            verification_owner: None,
            on_behalf_of: None,
        },
        ADMIN,
        NOW,
    )
    .unwrap()
}

fn update(conn: &rusqlite::Connection, id: &str, patch: UpdateIssue) -> Result<(), IssueError> {
    acb_issue_service::update_issue(conn, id, patch, ADMIN, true, ADMIN, NOW).map(|_| ())
}

fn blank_update(version: i64) -> UpdateIssue {
    UpdateIssue {
        expected_version: version,
        title: None,
        body: None,
        priority: None,
        kind: None,
        milestone: None,
        tags: None,
        assignee: None,
        status: None,
        resolution: None,
        verification_owner: None,
        comment: None,
        on_behalf_of: None,
    }
}

#[test]
fn kind_defaults_to_task_and_rejects_unknown_values() {
    let conn = memory_db();
    let created = issue(&conn, "没有类型");
    assert_eq!(created.kind, "task", "不写 kind 时是 task");

    let bad = create_issue(
        &conn,
        CreateIssue {
            title: "类型写错".to_owned(),
            body: String::new(),
            priority: None,
            kind: Some("epic".to_owned()),
            milestone: None,
            tags: Vec::new(),
            assignee: None,
            reporter: None,
            source: None,
            related: None,
            verification_owner: None,
            on_behalf_of: None,
        },
        ADMIN,
        NOW,
    );
    assert!(
        matches!(bad, Err(IssueError::BadRequest(_))),
        "未知类型要报错"
    );

    // 大小写/空白归一化后再校验。
    let mut patch = blank_update(created.version);
    patch.kind = Some("  BUG ".to_owned());
    update(&conn, &created.id, patch).unwrap();
    assert_eq!(get_issue(&conn, &created.id).unwrap().kind, "bug");
}

#[test]
fn milestone_is_tri_state_on_update() {
    let conn = memory_db();
    let created = issue(&conn, "里程碑");
    let mut patch = blank_update(created.version);
    patch.milestone = Some(Some("v0.2".to_owned()));
    update(&conn, &created.id, patch).unwrap();
    assert_eq!(
        get_issue(&conn, &created.id).unwrap().milestone.as_deref(),
        Some("v0.2")
    );

    // 字段缺失 = 不动。
    let current = get_issue(&conn, &created.id).unwrap();
    update(&conn, &created.id, blank_update(current.version)).unwrap();
    assert_eq!(
        get_issue(&conn, &created.id).unwrap().milestone.as_deref(),
        Some("v0.2"),
        "没提 milestone 就不该动它"
    );

    // 显式 null = 清空（JSON 侧由 deserialize_some 保证；这里直接构造 Some(None)）。
    let current = get_issue(&conn, &created.id).unwrap();
    let mut patch = blank_update(current.version);
    patch.milestone = Some(None);
    update(&conn, &created.id, patch).unwrap();
    assert!(get_issue(&conn, &created.id).unwrap().milestone.is_none());
}

#[test]
fn meta_changes_land_in_the_timeline() {
    let conn = memory_db();
    let created = issue(&conn, "改了要留痕");
    let mut patch = blank_update(created.version);
    patch.kind = Some("feature".to_owned());
    patch.milestone = Some(Some("v0.3".to_owned()));
    patch.priority = Some("P1".to_owned());
    update(&conn, &created.id, patch).unwrap();

    let events = events_for_issue(&conn, &created.id).unwrap();
    let updated = events
        .iter()
        .find(|event| event.kind == "issue.updated")
        .expect("应有 issue.updated 事件");
    assert!(
        updated.summary.contains("kind feature"),
        "{}",
        updated.summary
    );
    assert!(
        updated.summary.contains("milestone v0.3"),
        "{}",
        updated.summary
    );
    assert!(
        updated.summary.contains("priority P1"),
        "{}",
        updated.summary
    );
}

#[test]
fn comments_are_events_and_bump_the_version() {
    let conn = memory_db();
    let created = issue(&conn, "评论");
    let before = created.version;
    let event = add_comment(
        &conn,
        &created.id,
        CommentInput {
            body: "  第二条线索  ".to_owned(),
            idempotency_key: None,
            on_behalf_of: None,
        },
        ADMIN,
        NOW + 1,
    )
    .unwrap();
    assert_eq!(event.kind, "issue.comment_added");
    assert_eq!(event.summary, "第二条线索", "首尾空白去掉");

    let comments = comments_for_issue(&conn, &created.id).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(
        get_issue(&conn, &created.id).unwrap().version,
        before + 1,
        "评论要推进版本，列表排序才看得到"
    );

    let empty = add_comment(
        &conn,
        &created.id,
        CommentInput {
            body: "   ".to_owned(),
            idempotency_key: None,
            on_behalf_of: None,
        },
        ADMIN,
        NOW + 2,
    );
    assert!(
        matches!(empty, Err(IssueError::BadRequest(_))),
        "空评论要拒"
    );
}

#[test]
fn links_are_idempotent_and_refuse_self_reference() {
    let conn = memory_db();
    let a = issue(&conn, "A");
    let b = issue(&conn, "B");

    let first = create_link(
        &conn,
        &a.id,
        CreateLink {
            to_issue: Some(b.id.clone()),
            kind: Some("blocks".to_owned()),
            on_behalf_of: None,
        },
        ADMIN,
        NOW + 1,
    )
    .unwrap();
    let again = create_link(
        &conn,
        &a.id,
        CreateLink {
            to_issue: Some(b.id.clone()),
            kind: Some("blocks".to_owned()),
            on_behalf_of: None,
        },
        ADMIN,
        NOW + 2,
    )
    .unwrap();
    assert_eq!(first.id, again.id, "同一对 (from,to,kind) 只留一条");

    let self_link = create_link(
        &conn,
        &a.id,
        CreateLink {
            to_issue: Some(a.id.clone()),
            kind: None,
            on_behalf_of: None,
        },
        ADMIN,
        NOW + 3,
    );
    assert!(matches!(self_link, Err(IssueError::BadRequest(_))));

    // 两边都能看见这条关联。
    assert_eq!(list_links(&conn, &a.id).unwrap().len(), 1);
    assert_eq!(list_links(&conn, &b.id).unwrap().len(), 1);

    assert!(delete_link(&conn, &first.id).unwrap());
    assert!(list_links(&conn, &a.id).unwrap().is_empty());
}

#[test]
fn search_filters_cover_the_new_columns() {
    let conn = memory_db();
    let bug = create_issue(
        &conn,
        CreateIssue {
            title: "白屏".to_owned(),
            body: String::new(),
            priority: Some("P1".to_owned()),
            kind: Some("bug".to_owned()),
            milestone: Some("v0.2".to_owned()),
            tags: Vec::new(),
            assignee: None,
            reporter: None,
            source: None,
            related: None,
            verification_owner: None,
            on_behalf_of: None,
        },
        "alice",
        NOW,
    )
    .unwrap();
    issue(&conn, "没有里程碑的任务");

    let by_kind = search_issues(
        &conn,
        &SearchQuery {
            kind: Some("bug".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_kind.issues.len(), 1);
    assert_eq!(by_kind.issues[0].id, bug.id);

    let by_milestone = search_issues(
        &conn,
        &SearchQuery {
            milestone: Some("v0.2".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_milestone.issues.len(), 1);

    let no_milestone = search_issues(
        &conn,
        &SearchQuery {
            milestone: Some("none".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(no_milestone.issues.len(), 1, "none = 没有里程碑");
    assert_ne!(no_milestone.issues[0].id, bug.id);

    let by_reporter = search_issues(
        &conn,
        &SearchQuery {
            reporter: Some("alice".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_reporter.issues.len(), 1);

    let by_priority = search_issues(
        &conn,
        &SearchQuery {
            priority: Some("p1".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(by_priority.issues.len(), 1, "优先级大小写不敏感");
}

#[test]
fn ui_is_embedded_and_self_contained() {
    assert!(UI_HTML.contains("Issue 工作台"));
    assert!(
        UI_HTML.contains("/v1/meta"),
        "UI 从 meta 取枚举，不另抄一份"
    );
    assert!(!UI_HTML.contains("http://"), "不允许引外部资源");
    assert!(!UI_HTML.contains("<script src"), "单文件，无构建步骤");
}
