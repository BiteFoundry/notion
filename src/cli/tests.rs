use super::*;
use crate::models::{NotionBlock, NotionComment, NotionDatabase, NotionPage, NotionPageDetail};
use serde_json::Value;
use clap::error::ErrorKind;

#[test]
fn offset_page_boundaries() {
    let items = vec![10, 20, 30, 40, 50];

    let p1 = offset_page(items.clone(), 2, Some("2")).unwrap();
    assert_eq!(p1.items, vec![30, 40]);
    assert!(p1.has_more);
    assert_eq!(p1.next_cursor.as_deref(), Some("4"));

    let p2 = offset_page(items.clone(), 2, Some("4")).unwrap();
    assert_eq!(p2.items, vec![50]);
    assert!(!p2.has_more);
    assert_eq!(p2.next_cursor, None);

    let p3 = offset_page(items.clone(), 2, Some("5")).unwrap();
    assert!(p3.items.is_empty());
    assert!(!p3.has_more);
    assert_eq!(p3.next_cursor, None);

    let p4 = offset_page(items.clone(), 2, Some("9")).unwrap();
    assert!(p4.items.is_empty());
    assert!(!p4.has_more);
    assert_eq!(p4.next_cursor, None);

    let bad_cursor = offset_page(items.clone(), 2, Some("x"))
        .err()
        .unwrap()
        .to_string();
    assert!(bad_cursor.contains("Invalid cursor 'x'"));

    let bad_limit = offset_page(items.clone(), 0, None)
        .err()
        .unwrap()
        .to_string();
    assert!(bad_limit.contains("Invalid limit 0"));

    let all = offset_page(items.clone(), usize::MAX, None).unwrap();
    assert_eq!(all.items, items);
    assert!(!all.has_more);
    assert_eq!(all.next_cursor, None);
}

const KO_ITEM: &str = "- 목록 항목";
const TWO_LINES: &str = "- first\n- second";

#[test]
fn parses_hyphen_text_for_all_audited_notion_commands() {
    // 1. pages create --title / --body
    let cli = parse_cli_from([
        "notion", "pages", "create", "--parent", "page", "--title", KO_ITEM, "--body",
        TWO_LINES, "--json",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Pages {
            action:
                PagesCommand::Create {
                    parent,
                    title,
                    body,
                    json,
                    ..
                },
        }) => {
            assert_eq!(parent, "page");
            assert_eq!(title, KO_ITEM);
            assert_eq!(body.as_deref(), Some(TWO_LINES));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 2. pages append
    let cli = parse_cli_from(["notion", "pages", "append", "page", TWO_LINES, "--json"])
        .unwrap();
    match cli.command {
        Some(Command::Pages {
            action: PagesCommand::Append { page, body, json, .. },
        }) => {
            assert_eq!(page, "page");
            assert_eq!(body.as_deref(), Some(TWO_LINES));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 3. blocks append
    let cli = parse_cli_from(["notion", "blocks", "append", "block", KO_ITEM, "--json"])
        .unwrap();
    match cli.command {
        Some(Command::Blocks {
            action: BlocksCommand::Append { block_id, body, json, .. },
        }) => {
            assert_eq!(block_id, "block");
            assert_eq!(body.as_deref(), Some(KO_ITEM));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 4. blocks edit --text
    let cli = parse_cli_from(["notion", "blocks", "edit", "block", "--text", KO_ITEM, "--json"])
        .unwrap();
    match cli.command {
        Some(Command::Blocks {
            action: BlocksCommand::Edit { block_id, text, json },
        }) => {
            assert_eq!(block_id, "block");
            assert_eq!(text, KO_ITEM);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 5. comments add
    let cli = parse_cli_from([
        "notion", "comments", "add", "page", KO_ITEM, "--discussion", "disc-1", "--json",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Comments {
            action: CommentsCommand::Add { page, text, discussion, json },
        }) => {
            assert_eq!(page, "page");
            assert_eq!(text, KO_ITEM);
            assert_eq!(discussion.as_deref(), Some("disc-1"));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 6. db create-item --title / --body
    let cli = parse_cli_from([
        "notion", "db", "create-item", "db", "--title", KO_ITEM, "--body", TWO_LINES, "--json",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Db {
            action: DbCommand::CreateItem { database, title, body, json, .. },
        }) => {
            assert_eq!(database, "db");
            assert_eq!(title, KO_ITEM);
            assert_eq!(body.as_deref(), Some(TWO_LINES));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 7. pages edit --title
    let cli = parse_cli_from(["notion", "pages", "edit", "page", "--title", KO_ITEM, "--json"])
        .unwrap();
    match cli.command {
        Some(Command::Pages {
            action: PagesCommand::Edit { page, title, json, .. },
        }) => {
            assert_eq!(page, "page");
            assert_eq!(title.as_deref(), Some(KO_ITEM));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 8. db update-item --title
    let cli = parse_cli_from([
        "notion", "db", "update-item", "page", "--title", KO_ITEM, "--json",
    ])
    .unwrap();
    match cli.command {
        Some(Command::Db {
            action: DbCommand::UpdateItem { page_id, title, json, .. },
        }) => {
            assert_eq!(page_id, "page");
            assert_eq!(title.as_deref(), Some(KO_ITEM));
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // 9. search
    let cli = parse_cli_from(["notion", "search", KO_ITEM, "--remote", "--limit", "5", "--json"])
        .unwrap();
    match cli.command {
        Some(Command::Search {
            query,
            remote,
            limit,
            json,
            ..
        }) => {
            assert_eq!(query, KO_ITEM);
            assert!(remote);
            assert_eq!(limit, 5);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }
}

#[test]
fn preserves_flags_missing_values_equals_separator_and_help() {
    // Explicit `--flag=value` and positional `--` still allow flag-looking text.
    let cli = parse_cli_from(["notion", "blocks", "edit", "blk", "--text=--dry-run", "--json"])
        .unwrap();
    match cli.command {
        Some(Command::Blocks {
            action: BlocksCommand::Edit { text, json, .. },
        }) => {
            assert_eq!(text, "--dry-run");
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = parse_cli_from(["notion", "pages", "append", "page", "--", "--dry-run"])
        .unwrap();
    match cli.command {
        Some(Command::Pages {
            action: PagesCommand::Append { body, .. },
        }) => assert_eq!(body.as_deref(), Some("--dry-run")),
        other => panic!("unexpected command: {other:?}"),
    }

    let cli = parse_cli_from(["notion", "search", "--", "--help"]).unwrap();
    match cli.command {
        Some(Command::Search { query, .. }) => assert_eq!(query, "--help"),
        other => panic!("unexpected command: {other:?}"),
    }

    // Real flags after a missing option value must NOT be swallowed as the option value.
    for bad in [
        vec!["notion", "blocks", "edit", "blk", "--text", "--json"],
        vec!["notion", "blocks", "edit", "blk", "--text", "--dry-run"],
        vec!["notion", "blocks", "edit", "blk", "--text", "--help"],
        vec!["notion", "pages", "create", "--parent", "p", "--title", "T", "--body", "--json"],
        vec!["notion", "pages", "edit", "p", "--title", "--editor"],
        vec!["notion", "db", "create-item", "db", "--title", "--json"],
        vec!["notion", "pages", "append", "page", "--dry-run"],
        vec!["notion", "search", "--dry-run"],
        vec!["notion", "search", "-x"],
    ] {
        let err = parse_cli_from(bad.clone()).unwrap_err();
        assert_ne!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected parse error for {bad:?}, got help"
        );
    }

    // `pages append page --json` keeps body=None (does not swallow `--json`).
    let cli = parse_cli_from(["notion", "pages", "append", "page", "--json"]).unwrap();
    match cli.command {
        Some(Command::Pages {
            action: PagesCommand::Append { body, json, .. },
        }) => {
            assert_eq!(body, None);
            assert!(json);
        }
        other => panic!("unexpected command: {other:?}"),
    }

    // Help works both alone and after hyphen text values.
    for help_args in [
        vec!["notion", "pages", "create", "--parent", "p", "--title", "T", "--body", KO_ITEM, "--help"],
        vec!["notion", "pages", "append", "page", KO_ITEM, "--help"],
        vec!["notion", "blocks", "append", "blk", KO_ITEM, "--help"],
        vec!["notion", "blocks", "edit", "blk", "--text", KO_ITEM, "--help"],
        vec!["notion", "comments", "add", "page", KO_ITEM, "--help"],
        vec!["notion", "db", "create-item", "db", "--title", "T", "--body", KO_ITEM, "--help"],
        vec!["notion", "pages", "edit", "page", "--title", KO_ITEM, "--help"],
        vec!["notion", "db", "update-item", "page", "--title", KO_ITEM, "--help"],
        vec!["notion", "search", KO_ITEM, "--help"],
        vec!["notion", "search", "--help"],
        vec!["notion", "blocks", "edit", "--help"],
        vec!["notion", "pages", "append", "page", "-h"],
    ] {
        let err = parse_cli_from(help_args.clone()).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected DisplayHelp for {help_args:?}, got {err}"
        );
    }

    // Non-text ID, number, and enum parsers stay unchanged.
    for rejected in [
        vec!["notion", "pages", "create", "--parent", "-bad-id", "--title", "T"],
        vec!["notion", "pages", "append", "-bad-page", KO_ITEM],
        vec!["notion", "pages", "list", "--limit", "-5"],
        vec!["notion", "pages", "get", "page", "--format", "-md"],
    ] {
        assert!(
            parse_cli_from(rejected.clone()).is_err(),
            "expected parser rejection for {rejected:?}"
        );
    }
}

#[tokio::test]
async fn write_ops_use_cached_schema_and_retry_once_on_validation_error() {
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    use axum::{
        extract::Path as AxumPath,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use serde_json::json;
    use tokio::net::TcpListener;

    let db_gets = Arc::new(AtomicUsize::new(0));
    let page_gets = Arc::new(AtomicUsize::new(0));
    let renamed_title_col = Arc::new(AtomicBool::new(false));

    let db_gets_h = Arc::clone(&db_gets);
    let page_gets_h = Arc::clone(&page_gets);
    let renamed_db_h = Arc::clone(&renamed_title_col);
    let renamed_post_h = Arc::clone(&renamed_title_col);

    let app = Router::new()
        .route(
            "/v1/databases/{id}",
            get(move |AxumPath(id): AxumPath<String>| {
                let c = Arc::clone(&db_gets_h);
                let renamed = Arc::clone(&renamed_db_h);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    let col_name = if renamed.load(Ordering::SeqCst) {
                        "Task Title"
                    } else {
                        "Name"
                    };
                    Json(json!({
                        "object": "database",
                        "id": id,
                        "url": "https://www.notion.so/db-1",
                        "parent": { "type": "workspace", "workspace": true },
                        "title": [{ "plain_text": "Tasks" }],
                        "description": [],
                        "last_edited_time": "2026-11-20T02:00:00.000Z",
                        "properties": {
                            col_name: { "id": "title", "type": "title", "title": {} }
                        }
                    }))
                }
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(move |AxumPath(id): AxumPath<String>| {
                let c = Arc::clone(&page_gets_h);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "object": "page",
                        "id": id,
                        "url": format!("https://www.notion.so/{id}"),
                        "parent": { "type": "database_id", "database_id": "db-1" },
                        "archived": false,
                        "last_edited_time": "2026-11-20T02:00:00.000Z",
                        "properties": {
                            "Name": { "type": "title", "title": [{ "plain_text": "Row 1" }] }
                        }
                    }))
                }
            })
            .patch(|AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                let props = body
                    .get("properties")
                    .cloned()
                    .unwrap_or_else(|| json!({ "Name": { "type": "title", "title": [{ "plain_text": "Updated" }] } }));
                Json(json!({
                    "object": "page",
                    "id": id,
                    "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "database_id", "database_id": "db-1" },
                    "archived": false,
                    "last_edited_time": "2026-11-20T03:00:00.000Z",
                    "properties": props
                }))
            }),
        )
        .route(
            "/v1/pages",
            post(move |Json(body): Json<Value>| {
                let renamed = Arc::clone(&renamed_post_h);
                async move {
                    let expected = if renamed.load(Ordering::SeqCst) {
                        "Task Title"
                    } else {
                        "Name"
                    };
                    let has_expected = body
                        .get("properties")
                        .and_then(|p| p.get(expected))
                        .is_some();
                    if !has_expected {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "object": "error",
                                "status": 400,
                                "code": "validation_error",
                                "message": format!("Name is not a property that exists (expected {expected}).")
                            })),
                        )
                            .into_response();
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "object": "page",
                            "id": "row-created",
                            "url": "https://www.notion.so/row-created",
                            "parent": { "type": "database_id", "database_id": "db-1" },
                            "archived": false,
                            "last_edited_time": "2026-11-20T03:00:00.000Z",
                            "properties": {
                                expected: { "type": "title", "title": [{ "plain_text": "Created" }] }
                            }
                        })),
                    )
                        .into_response()
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("cli_cache_write.db")).unwrap();
    let cfg = AppConfig {
        api_base_url: format!("http://{addr}/v1"),
        ..Default::default()
    };
    let auth = AuthManager::from_token(cfg.clone(), "ntn_cli_write");
    let ws_id = auth.active_workspace_id().await;
    let client = NotionClient::new(&cfg, auth).unwrap();

    // Seed cached database schema + cached page properties in SQLite.
    let mut db_schema = std::collections::BTreeMap::new();
    db_schema.insert(
        "Name".to_string(),
        crate::models::NotionPropertySchema {
            id: "title".into(),
            name: "Name".into(),
            prop_type: "title".into(),
            options: Vec::new(),
            relation_database_id: None,
        },
    );
    storage
        .upsert_database(&NotionDatabase {
            id: "db-1".into(),
            workspace_id: ws_id.clone(),
            title: "Tasks".into(),
            schema: db_schema,
            ..Default::default()
        })
        .unwrap();
    let mut page_props = std::collections::BTreeMap::new();
    page_props.insert(
        "Name".to_string(),
        crate::models::NotionPropertyValue::Title("Row 1".into()),
    );
    storage
        .upsert_page(&NotionPage {
            id: "row-1".into(),
            workspace_id: ws_id.clone(),
            parent_type: "database_id".into(),
            parent_id: Some("db-1".into()),
            title: "Row 1".into(),
            properties: page_props,
            ..Default::default()
        })
        .unwrap();

    // Warm cache: all 4 write ops should skip GET /v1/databases and GET /v1/pages.
    op_create_database_item(&client, &storage, "db-1", "Item 1", None, None, None)
        .await
        .unwrap();
    op_update_database_item(&client, &storage, "row-1", Some("Row 1b"), None, None)
        .await
        .unwrap();
    op_update_page_metadata(&client, &storage, "row-1", Some(Some("Row 1c")), None)
        .await
        .unwrap();
    op_patch_page(
        &client,
        &storage,
        "row-1",
        PagePatch {
            title: Some("Row 1d"),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(db_gets.load(Ordering::SeqCst), 0, "cached DB schema must skip GET /v1/databases");
    assert_eq!(page_gets.load(Ordering::SeqCst), 0, "cached page properties must skip GET /v1/pages");

    // Stale cache recovery: server renamed title column -> HTTP 400 validation_error ->
    // refreshes DB schema once and retries POST /v1/pages successfully.
    renamed_title_col.store(true, Ordering::SeqCst);
    let created =
        op_create_database_item(&client, &storage, "db-1", "Item After Rename", None, None, None)
            .await
            .unwrap();
    assert_eq!(created.id, "row-created");
    assert_eq!(
        db_gets.load(Ordering::SeqCst),
        1,
        "stale schema validation_error must refresh DB schema exactly once and retry"
    );
    server.abort();
}

#[tokio::test]
async fn append_blocks_and_add_comment_update_warm_cache_without_refetch() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use axum::{
        extract::Path as AxumPath,
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use tokio::net::TcpListener;

    let block_gets = Arc::new(AtomicUsize::new(0));
    let comment_gets = Arc::new(AtomicUsize::new(0));
    let bg_h = Arc::clone(&block_gets);
    let cg_h = Arc::clone(&comment_gets);

    let app = Router::new()
        .route(
            "/v1/blocks/{id}/children",
            get(move |AxumPath(_id): AxumPath<String>| {
                let c = Arc::clone(&bg_h);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "results": [], "has_more": false, "next_cursor": null }))
                }
            })
            .patch(|AxumPath(_id): AxumPath<String>| async {
                Json(json!({
                    "results": [{
                        "object": "block",
                        "id": "blk-appended",
                        "type": "paragraph",
                        "has_children": false,
                        "paragraph": { "rich_text": [{ "plain_text": "Appended line" }] }
                    }]
                }))
            }),
        )
        .route(
            "/v1/comments",
            get(move || {
                let c = Arc::clone(&cg_h);
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    Json(json!({ "results": [], "has_more": false, "next_cursor": null }))
                }
            })
            .post(|| async {
                Json(json!({
                    "object": "comment",
                    "id": "cmt-new",
                    "discussion_id": "disc-2",
                    "created_time": "2026-11-20T04:00:00.000Z",
                    "created_by": { "id": "u-1", "name": "Alice" },
                    "rich_text": [{ "plain_text": "Looks good!" }]
                }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("cli_incremental.db")).unwrap();
    let cfg = AppConfig {
        api_base_url: format!("http://{addr}/v1"),
        ..Default::default()
    };
    let auth = AuthManager::from_token(cfg.clone(), "ntn_cli_inc");
    let ws_id = auth.active_workspace_id().await;
    let client = NotionClient::new(&cfg, auth).unwrap();

    // Seed warm page detail (existing block + complete comments).
    let page = NotionPage {
        id: "page-1".into(),
        workspace_id: ws_id.clone(),
        title: "Doc".into(),
        last_edited_time: Some("2026-11-20T02:00:00Z".into()),
        ..Default::default()
    };
    let existing_block = NotionBlock {
        id: "blk-0".into(),
        page_id: "page-1".into(),
        parent_block_id: None,
        block_type: "paragraph".into(),
        has_children: false,
        archived: false,
        kind: crate::models::NotionBlockKind::Paragraph {
            rich_text: vec![crate::models::RichTextSpan::plain("Existing line")],
            color: crate::models::NotionColor::Default,
        },
        children: Vec::new(),
    };
    let existing_comment = NotionComment {
        id: "cmt-0".into(),
        page_id: "page-1".into(),
        discussion_id: "disc-1".into(),
        author: "Bob".into(),
        body: "First".into(),
        rich_text: vec![crate::models::RichTextSpan::plain("First")],
        created_time: "2026-11-20T01:00:00Z".into(),
    };
    storage
        .save_page_detail(&NotionPageDetail {
            page,
            blocks: vec![existing_block],
            comments: vec![existing_comment],
            comments_error: None,
            markdown: "Existing line".into(),
            media: Vec::new(),
        })
        .unwrap();

    let appended = op_append_blocks(&client, &storage, "page-1", "Appended line")
        .await
        .unwrap();
    assert_eq!(appended.len(), 1);
    assert_eq!(
        block_gets.load(Ordering::SeqCst),
        0,
        "warm-cache op_append_blocks must not refetch the full block tree"
    );
    let detail = storage.load_page_detail(&ws_id, "page-1").unwrap().unwrap();
    let ids: Vec<&str> = detail.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["blk-0", "blk-appended"]);

    let comment = op_add_comment(&client, &storage, "page-1", None, "Looks good!")
        .await
        .unwrap();
    assert_eq!(comment.id, "cmt-new");
    assert_eq!(
        comment_gets.load(Ordering::SeqCst),
        0,
        "warm-cache op_add_comment must not refetch GET /v1/comments"
    );
    let comments = storage.load_page_comments(&ws_id, "page-1").unwrap();
    let cids: Vec<&str> = comments.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(cids, vec!["cmt-0", "cmt-new"]);

    server.abort();
}
