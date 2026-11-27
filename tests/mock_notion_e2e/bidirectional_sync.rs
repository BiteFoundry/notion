use notion_cli::{
    auth::AuthManager,
    notion::NotionClient,
    storage::Storage,
    sync,
};

use crate::server::test_config;

#[tokio::test]
async fn e2e_bidirectional_sync_versions_forced_refresh_block_writes_and_section4_followups() {
    use std::sync::{Arc, Mutex};
    use axum::{
        extract::{Path as AxumPath, Query as AxumQuery, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use notion_cli::cli;
    use notion_cli::models::{CoreEvent, NotionBlockKind};
    use notion_cli::sync::{DetailSyncMode, FocusTarget, SyncCommand, SyncController};
    use notion_cli::ui::action::{self, Action};
    use notion_cli::ui::app::{AppState, UiMsg, UiState};
    use serde_json::{json, Value};

    #[derive(Default)]
    struct SyncMockState {
        page_ts: String,
        para_text: String,
        toggle_child_text: String,
        todo_checked: bool,
        del_archived: bool,
        comments: Vec<Value>,
        beta_archived: bool,
        search_omit_valid_page: bool,
        deleted_404_missing_from_search: bool,
        ambiguous_500_missing_from_search: bool,
        row2_archived: bool,
        fail_toggle_children: bool,
        search_two_pages_same_ts: bool,
        hits: Vec<String>,
    }

    let state = Arc::new(Mutex::new(SyncMockState {
        page_ts: "2026-11-20T02:00:00.000Z".into(),
        para_text: "Body V2".into(),
        toggle_child_text: "Nested V2".into(),
        todo_checked: false,
        del_archived: false,
        comments: vec![json!({
            "id": "cmt-1",
            "discussion_id": "disc-1",
            "created_time": "2026-11-20T02:00:00.000Z",
            "created_by": { "id": "u-1", "name": "Alice" },
            "rich_text": [{ "plain_text": "Comment V2" }]
        })],
        beta_archived: false,
        search_omit_valid_page: false,
        deleted_404_missing_from_search: false,
        ambiguous_500_missing_from_search: false,
        row2_archived: false,
        fail_toggle_children: false,
        search_two_pages_same_ts: false,
        hits: Vec::new(),
    }));

    let mk_page = |id: &str, title: &str, ts: &str, parent: Value| {
        json!({
            "object": "page",
            "id": id,
            "url": format!("https://notion.so/{id}"),
            "parent": parent,
            "archived": false,
            "last_edited_time": ts,
            "properties": {
                "title": { "type": "title", "title": [{ "plain_text": title }] }
            }
        })
    };

    let router = Router::new()
        .route(
            "/v1/users/me",
            get(|State(st): State<Arc<Mutex<SyncMockState>>>| async move {
                st.lock().unwrap().hits.push("GET /users/me".into());
                Json(json!({
                    "object": "user",
                    "id": "bot-1",
                    "name": "Sync Bot",
                    "type": "bot",
                    "bot": { "workspace_name": "Sync WS" }
                }))
            }),
        )
        .route(
            "/v1/search",
            post(
                move |State(st): State<Arc<Mutex<SyncMockState>>>, Json(body): Json<Value>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push("POST /search".into());
                    let cursor = body.get("start_cursor").and_then(Value::as_str);
                    if g.search_two_pages_same_ts {
                        let ts = g.page_ts.clone();
                        if cursor.is_none() {
                            return Json(json!({
                                "object": "list",
                                "results": [
                                    mk_page("page-a", "Page A", &ts, json!({"type":"workspace","workspace":true}))
                                ],
                                "has_more": true,
                                "next_cursor": "cur-2"
                            }));
                        }
                        return Json(json!({
                            "object": "list",
                            "results": [
                                mk_page("page-same-min-2", "Same Minute Second Page", &ts, json!({"type":"workspace","workspace":true}))
                            ],
                            "has_more": false,
                            "next_cursor": null
                        }));
                    }
                    let mut results = vec![mk_page(
                        "page-a",
                        "Page A",
                        &g.page_ts,
                        json!({"type":"workspace","workspace":true}),
                    )];
                    if !g.beta_archived {
                        results.push(mk_page(
                            "page-beta",
                            "Page Beta",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    if !g.search_omit_valid_page {
                        results.push(mk_page(
                            "page-valid-omitted",
                            "Valid Omitted Spec",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    if !g.deleted_404_missing_from_search {
                        results.push(mk_page(
                            "page-gone-404",
                            "Gone 404 Spec",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    if !g.ambiguous_500_missing_from_search {
                        results.push(mk_page(
                            "page-flaky-500",
                            "Flaky 500 Spec",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    results.push(mk_page(
                        "row-1",
                        "Row One",
                        "2026-11-20T01:00:00.000Z",
                        json!({"type":"database_id","database_id":"db-1"}),
                    ));
                    if !g.row2_archived {
                        results.push(mk_page(
                            "row-2",
                            "Row Two",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"database_id","database_id":"db-1"}),
                        ));
                    }
                    results.push(json!({
                        "object": "database",
                        "id": "db-1",
                        "url": "https://notion.so/db-1",
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false,
                        "title": [{ "plain_text": "Tasks DB" }],
                        "description": [],
                        "last_edited_time": "2026-11-20T01:00:00.000Z",
                        "properties": { "Name": { "id": "title", "type": "title", "title": {} } }
                    }));
                    Json(json!({
                        "object": "list",
                        "results": results,
                        "has_more": false,
                        "next_cursor": null
                    }))
                },
            ),
        )
        .route(
            "/v1/pages/{id}",
            get(
                move |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /pages/{id}"));
                    if id == "page-beta" {
                        let mut p = mk_page(
                            "page-beta",
                            "Page Beta",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        );
                        if g.beta_archived {
                            // Verify `in_trash: true` with `archived: false` is treated as archived
                            p["archived"] = json!(false);
                            p["in_trash"] = json!(true);
                        }
                        return (StatusCode::OK, Json(p)).into_response();
                    }
                    if id == "page-valid-omitted" {
                        return (
                            StatusCode::OK,
                            Json(mk_page(
                                "page-valid-omitted",
                                "Valid Omitted Spec",
                                "2026-11-20T01:00:00.000Z",
                                json!({"type":"workspace","workspace":true}),
                            )),
                        )
                            .into_response();
                    }
                    if id == "page-gone-404" && g.deleted_404_missing_from_search {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "code": "object_not_found",
                                "message": "Could not find page with ID"
                            })),
                        )
                            .into_response();
                    }
                    if id == "page-flaky-500" && g.ambiguous_500_missing_from_search {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "code": "internal_server_error",
                                "message": "transient upstream failure"
                            })),
                        )
                            .into_response();
                    }
                    if id == "row-2" {
                        let mut p = mk_page(
                            "row-2",
                            "Row Two",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"database_id","database_id":"db-1"}),
                        );
                        if g.row2_archived {
                            p["archived"] = json!(true);
                        }
                        return (StatusCode::OK, Json(p)).into_response();
                    }
                    (
                        StatusCode::OK,
                        Json(mk_page(
                            &id,
                            "Page A",
                            &g.page_ts,
                            json!({"type":"workspace","workspace":true}),
                        )),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(
                |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /blocks/{id}/children"));
                    if id == "blk-toggle" {
                        if g.fail_toggle_children {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({ "code": "internal_server_error", "message": "injected failure" })),
                            )
                                .into_response();
                        }
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "object": "list",
                                "results": [{
                                    "id": "blk-toggle-child",
                                    "type": "paragraph",
                                    "has_children": false,
                                    "archived": false,
                                    "paragraph": { "rich_text": [{ "plain_text": g.toggle_child_text }] }
                                }],
                                "has_more": false,
                                "next_cursor": null
                            })),
                        )
                            .into_response();
                    }
                    let mut results = vec![
                        json!({
                            "id": "blk-para",
                            "type": "paragraph",
                            "has_children": false,
                            "archived": false,
                            "paragraph": { "rich_text": [{ "plain_text": g.para_text }] }
                        }),
                        json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "has_children": false,
                            "archived": false,
                            "to_do": { "checked": g.todo_checked, "rich_text": [{ "plain_text": "Check item" }] }
                        }),
                        json!({
                            "id": "blk-toggle",
                            "type": "toggle",
                            "has_children": true,
                            "archived": false,
                            "toggle": { "rich_text": [{ "plain_text": "Details" }] }
                        }),
                    ];
                    if !g.del_archived {
                        results.push(json!({
                            "id": "blk-del",
                            "type": "paragraph",
                            "has_children": false,
                            "archived": false,
                            "paragraph": { "rich_text": [{ "plain_text": "Removable" }] }
                        }));
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "object": "list",
                            "results": results,
                            "has_more": false,
                            "next_cursor": null
                        })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}",
            get(
                |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let g = st.lock().unwrap();
                    if id == "blk-todo" {
                        return Json(json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "parent": { "type": "page_id", "page_id": "page-a" },
                            "has_children": false,
                            "archived": false,
                            "to_do": { "checked": g.todo_checked, "rich_text": [{ "plain_text": "Check item" }] }
                        }));
                    }
                    Json(json!({
                        "id": id,
                        "type": "paragraph",
                        "parent": { "type": "page_id", "page_id": "page-a" },
                        "has_children": false,
                        "archived": false,
                        "paragraph": { "rich_text": [{ "plain_text": g.para_text }] }
                    }))
                },
            )
            .patch(
                |State(st): State<Arc<Mutex<SyncMockState>>>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("PATCH /blocks/{id}"));
                    if id == "blk-todo" {
                        if let Some(c) = body.pointer("/to_do/checked").and_then(Value::as_bool) {
                            g.todo_checked = c;
                        }
                        return Json(json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "parent": { "type": "page_id", "page_id": "page-a" },
                            "has_children": false,
                            "archived": false,
                            "to_do": { "checked": g.todo_checked, "rich_text": [{ "plain_text": "Check item" }] }
                        }));
                    }
                    if let Some(txt) = body
                        .pointer("/paragraph/rich_text/0/text/content")
                        .and_then(Value::as_str)
                    {
                        g.para_text = txt.to_string();
                    }
                    Json(json!({
                        "id": id,
                        "type": "paragraph",
                        "parent": { "type": "page_id", "page_id": "page-a" },
                        "has_children": false,
                        "archived": false,
                        "paragraph": { "rich_text": [{ "plain_text": g.para_text }] }
                    }))
                },
            )
            .delete(
                |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("DELETE /blocks/{id}"));
                    if id == "blk-del" {
                        g.del_archived = true;
                    }
                    Json(json!({
                        "id": id,
                        "archived": true,
                        "parent": { "type": "page_id", "page_id": "page-a" }
                    }))
                },
            ),
        )
        .route(
            "/v1/comments",
            get(|State(st): State<Arc<Mutex<SyncMockState>>>| async move {
                let mut g = st.lock().unwrap();
                g.hits.push("GET /comments".into());
                Json(json!({
                    "object": "list",
                    "results": g.comments.clone(),
                    "has_more": false,
                    "next_cursor": null
                }))
            }),
        )
        .route(
            "/v1/databases/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "database",
                    "id": id,
                    "url": format!("https://notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "title": [{ "plain_text": "Tasks DB" }],
                    "description": [],
                    "last_edited_time": "2026-11-20T01:00:00.000Z",
                    "properties": { "Name": { "id": "title", "type": "title", "title": {} } }
                }))
            }),
        )
        .route(
            "/v1/databases/{id}/query",
            post(
                move |State(st): State<Arc<Mutex<SyncMockState>>>,
                      AxumQuery(_q): AxumQuery<std::collections::HashMap<String, String>>,
                      Json(body): Json<Value>| async move {
                    let g = st.lock().unwrap();
                    let page_size = body.get("page_size").and_then(Value::as_u64).unwrap_or(100) as usize;
                    let mut rows = vec![mk_page(
                        "row-1",
                        "Row One",
                        "2026-11-20T01:00:00.000Z",
                        json!({"type":"database_id","database_id":"db-1"}),
                    )];
                    if !g.row2_archived && body.get("filter").is_none() {
                        rows.push(mk_page(
                            "row-2",
                            "Row Two",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"database_id","database_id":"db-1"}),
                        ));
                    }
                    let has_more = rows.len() > page_size;
                    let next_cursor = has_more.then_some("db-cur-2");
                    rows.truncate(page_size);
                    Json(json!({
                        "object": "list",
                        "results": rows,
                        "has_more": has_more,
                        "next_cursor": next_cursor
                    }))
                },
            ),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("sync_e2e.db");
    let cfg = test_config(&format!("http://{addr}"));
    let auth = AuthManager::from_token(cfg.clone(), "ntn_sync_e2e");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open(&db_path).unwrap();

    // Cache V2
    sync::sync_workspace_tree_with_options(&client, &storage, true, |_, _, _| {})
        .await
        .unwrap();
    let d_v2 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v2.blocks[0].plain_text(), "Body V2");

    // Idle conditional read reuses cached body (0 block GETs!)
    state.lock().unwrap().hits.clear();
    let d_idle = sync::sync_page_detail_with_mode(
        &client,
        &storage,
        "page-a",
        DetailSyncMode::Conditional,
    )
    .await
    .unwrap();
    assert_eq!(d_idle.blocks[0].plain_text(), "Body V2");
    assert!(!state
        .lock()
        .unwrap()
        .hits
        .iter()
        .any(|h| h.contains("/blocks/")));

    // ① List -> Detail with V2 -> V3: MUST fetch V3!
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V3 list-before-detail".into();
        g.page_ts = "2026-11-20T03:00:00.000Z".into();
    }
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    let d_v3 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v3.blocks[0].plain_text(), "Body V3 list-before-detail");

    // ② Detail -> List with V3 -> V4: MUST see V4!
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V4 detail-before-list".into();
        g.page_ts = "2026-11-20T04:00:00.000Z".into();
    }
    let d_v4 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v4.blocks[0].plain_text(), "Body V4 detail-before-list");
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    let d_v4_after_list = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(
        d_v4_after_list.blocks[0].plain_text(),
        "Body V4 detail-before-list"
    );

    // ③ Restart after list sync (reopen SQLite) -> online read MUST see V5!
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V5 after-restart".into();
        g.page_ts = "2026-11-20T05:00:00.000Z".into();
    }
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    drop(storage);
    let storage = Storage::open(&db_path).unwrap();
    let d_v5 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v5.blocks[0].plain_text(), "Body V5 after-restart");

    // ③b F4c: same-minute edit with current `last_edited_time` is not frozen in cache.
    let now_ts = chrono::Utc::now().to_rfc3339();
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body SameMinute V1".into();
        g.page_ts = now_ts.clone();
    }
    let d_sm1 = sync::sync_page_body(&client, &storage, "page-a", false)
        .await
        .unwrap();
    assert_eq!(d_sm1.blocks[0].plain_text(), "Body SameMinute V1");
    {
        let mut g = state.lock().unwrap();
        // Same timestamp (minute granularity), updated block content
        g.para_text = "Body SameMinute V2".into();
    }
    let d_sm2 = sync::sync_page_body(&client, &storage, "page-a", false)
        .await
        .unwrap();
    assert_eq!(d_sm2.blocks[0].plain_text(), "Body SameMinute V2");
    let d_sm_detail1 = sync::sync_page_detail(&client, &storage, "page-a")
        .await
        .unwrap();
    assert_eq!(d_sm_detail1.blocks[0].plain_text(), "Body SameMinute V2");
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body SameMinute V3".into();
    }
    let d_sm_detail2 = sync::sync_page_detail(&client, &storage, "page-a")
        .await
        .unwrap();
    assert_eq!(d_sm_detail2.blocks[0].plain_text(), "Body SameMinute V3");
    // Restore historical timestamp for subsequent assertions in this test
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V5 after-restart".into();
        g.page_ts = "2026-11-20T05:00:00.000Z".into();
    }
    let _ = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();

    // Frozen parent timestamp + nested toggle child edit + new comment:
    // RefreshAll on spawn_sync_engine MUST see both AND fetch block tree ONLY ONCE!
    {
        let mut g = state.lock().unwrap();
        g.toggle_child_text = "Nested V5 frozen-ts".into();
        g.comments.push(json!({
            "id": "cmt-2",
            "discussion_id": "disc-2",
            "created_time": "2026-11-20T05:00:30.000Z",
            "created_by": { "id": "u-2", "name": "Bob" },
            "rich_text": [{ "plain_text": "Comment V5 frozen-ts" }]
        }));
        g.hits.clear();
    }
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<CoreEvent>();
    let ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), ev_tx);
    ctrl.set_focus(FocusTarget {
        page_id: Some("page-a".into()),
        database_id: None,
    });
    ctrl.request(SyncCommand::RefreshAll);
    let mut refreshed_detail = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(CoreEvent::PageDetailUpdated(d))) =
            tokio::time::timeout(std::time::Duration::from_millis(500), ev_rx.recv()).await
        {
            refreshed_detail = Some(*d);
            break;
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    ctrl.request(SyncCommand::Shutdown);
    let refreshed_detail = refreshed_detail.expect("RefreshAll must emit PageDetailUpdated");
    assert_eq!(
        refreshed_detail.blocks[2].children[0].plain_text(),
        "Nested V5 frozen-ts"
    );
    assert!(refreshed_detail
        .comments
        .iter()
        .any(|c| c.body == "Comment V5 frozen-ts"));
    let root_block_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        root_block_gets, 1,
        "RefreshAll must not fetch block tree twice for the same page"
    );

    // Block writes (Patch 2): toggle todo, update text, delete block -> immediate online + offline agree!
    let toggled = cli::op_toggle_todo(&client, &storage, "blk-todo", Some(true))
        .await
        .unwrap();
    assert!(matches!(
        toggled.kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));
    let after_toggle_online = cli::op_get_page(&client, &storage, "page-a", false)
        .await
        .unwrap();
    let after_toggle_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert!(matches!(
        after_toggle_online.blocks[1].kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));
    assert!(matches!(
        after_toggle_offline.blocks[1].kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));

    cli::op_update_block_text(
        &client,
        &storage,
        "blk-para",
        Some("paragraph"),
        "Patched paragraph text",
        None,
    )
    .await
    .unwrap();
    let after_patch_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert_eq!(
        after_patch_offline.blocks[0].plain_text(),
        "Patched paragraph text"
    );

    cli::op_delete_block(&client, &storage, "blk-del")
        .await
        .unwrap();
    let after_del_online = cli::op_get_page(&client, &storage, "page-a", false)
        .await
        .unwrap();
    let after_del_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert!(!after_del_online.blocks.iter().any(|b| b.id == "blk-del"));
    assert!(!after_del_offline.blocks.iter().any(|b| b.id == "blk-del"));

    // Section 4A: Notion Search limitation safety & authoritative reconciliation + DB query cap
    // 1. Database query hitting its limit with has_more=true MUST NOT prune unseen cached rows!
    let capped_rows =
        cli::op_query_database(&client, &storage, "db-1", None, None, 1, None, false)
            .await
            .unwrap()
            .items;
    assert_eq!(capped_rows.len(), 1);
    let offline_after_cap =
        cli::op_query_database(&client, &storage, "db-1", None, None, 50, None, true)
            .await
            .unwrap()
            .items;
    assert_eq!(
        offline_after_cap.len(),
        2,
        "DB query at cap with has_more=true must preserve unseen row-2"
    );

    // 2. Simulate Search omitting 4 pages even with has_more=false:
    //    - page-valid-omitted: valid on GET /v1/pages -> MUST retain cache & FTS!
    //    - page-flaky-500: HTTP 500 on GET /v1/pages -> MUST retain cache & FTS!
    //    - page-beta: in_trash=true on GET /v1/pages -> MUST hide & prune FTS!
    //    - page-gone-404: HTTP 404 object_not_found on GET /v1/pages -> MUST hide & prune FTS!
    {
        let mut g = state.lock().unwrap();
        g.beta_archived = true;
        g.search_omit_valid_page = true;
        g.deleted_404_missing_from_search = true;
        g.ambiguous_500_missing_from_search = true;
        g.hits.clear();
    }
    // Ordinary incremental poll (full_rescan=false) stays cheap: 0 missing-candidate GET probes!
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    assert!(
        !state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.starts_with("GET /pages/")),
        "ordinary incremental sync must not probe missing candidates"
    );

    // Explicit full refresh confirms missing candidates individually via GET by ID:
    sync::sync_workspace_tree_with_options(&client, &storage, true, |_, _, _| {})
        .await
        .unwrap();
    let ws_id = client.auth().active_workspace_id().await;
    let active_pages = cli::op_list_pages(&client, &storage, false, false, true, usize::MAX, None)
        .await
        .unwrap()
        .items;
    assert!(
        active_pages.iter().any(|p| p.id == "page-valid-omitted"),
        "valid page missing from Search must remain cached"
    );
    assert!(
        !storage
            .search_local(&ws_id, "Valid Omitted", 10)
            .unwrap()
            .is_empty(),
        "valid page missing from Search must retain FTS entry"
    );
    assert!(
        active_pages.iter().any(|p| p.id == "page-flaky-500"),
        "candidate with ambiguous HTTP 500 must remain cached"
    );
    assert!(
        !storage
            .search_local(&ws_id, "Flaky 500", 10)
            .unwrap()
            .is_empty(),
        "candidate with ambiguous HTTP 500 must retain FTS entry"
    );
    assert!(
        !active_pages.iter().any(|p| p.id == "page-beta"),
        "authoritatively in_trash page must be hidden"
    );
    assert!(
        storage
            .search_local(&ws_id, "Page Beta", 10)
            .unwrap()
            .is_empty(),
        "authoritatively in_trash page must be removed from FTS"
    );
    assert!(
        !active_pages.iter().any(|p| p.id == "page-gone-404"),
        "authoritatively 404 object_not_found page must be hidden"
    );
    assert!(
        storage
            .search_local(&ws_id, "Gone 404", 10)
            .unwrap()
            .is_empty(),
        "authoritatively 404 object_not_found page must be removed from FTS"
    );

    // 3. Complete unfiltered DB query (has_more=false) after archiving row-2 prunes row-2:
    state.lock().unwrap().row2_archived = true;
    let db_rows_live =
        cli::op_query_database(&client, &storage, "db-1", None, None, 50, None, false)
            .await
            .unwrap()
            .items;
    assert_eq!(db_rows_live.len(), 1);
    let db_rows_offline =
        cli::op_query_database(&client, &storage, "db-1", None, None, 50, None, true)
            .await
            .unwrap()
            .items;
    assert_eq!(db_rows_offline.len(), 1);
    assert_eq!(db_rows_offline[0].id, "row-1");

    // Section 4B: Pagination boundaries — same-minute watermark continues to page 2,
    // and nested child fetch failure propagates Err without deleting cached children!
    state.lock().unwrap().search_two_pages_same_ts = true;
    let inc = client
        .search_incremental(
            None,
            notion_cli::notion::SearchFilterObject::All,
            100,
            Some("2026-11-20T05:00:00.000Z"),
            |_, _, _| {},
        )
        .await
        .unwrap();
    assert!(inc.pages.iter().any(|p| p.id == "page-same-min-2"));
    state.lock().unwrap().search_two_pages_same_ts = false;

    state.lock().unwrap().fail_toggle_children = true;
    assert!(sync::sync_page_detail_force(&client, &storage, "page-a")
        .await
        .is_err());
    let preserved_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert_eq!(
        preserved_offline.blocks[2].children[0].plain_text(),
        "Nested V5 frozen-ts"
    );
    state.lock().unwrap().fail_toggle_children = false;

    // Section 4C & 4D: TUI restart focus restoration + inline editor external edit conflict guard
    let (focus_tx, focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let mut ui = UiState::from_config(&cfg);
    let mut app = AppState::new(
        cfg.clone(),
        auth.clone(),
        client.clone(),
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    app.workspace_id = client.auth().active_workspace_id().await;
    app.load_cached_initial();
    assert_eq!(
        focus_rx.borrow().page_id.as_deref(),
        app.active_page_id.as_deref(),
        "load_cached_initial must restore SyncController focus on restart"
    );

    // Open page-a, enter inline editor, type draft, receive external edit -> draft preserved + conflict armed
    app.open_page("page-a", &mut ui, false);
    assert!(app.open_inline_editor(&mut ui));
    action::dispatch(Action::Input('X'), &mut app, &mut ui);
    let user_draft = ui.inline_editor.as_ref().unwrap().composer.text().to_string();
    assert!(user_draft.starts_with('X'));

    let mut remote_detail = app.active_page_detail.clone().unwrap();
    remote_detail.page.last_edited_time = Some("2026-11-20T06:00:00.000Z".into());
    remote_detail.markdown = "Remote concurrent change".into();
    app.handle_core_event(&mut ui, CoreEvent::PageDetailUpdated(Box::new(remote_detail)));

    let ed = ui.inline_editor.as_ref().unwrap();
    assert_eq!(ed.composer.text(), user_draft, "user editing buffer must be preserved");
    assert!(ed.remote_conflict.is_some());

    // First Ctrl+S warns and does NOT dispatch overwrite or write __optimistic_* to SQLite
    let dispatched_first = app.flush_inline_editor_sync(&mut ui, true);
    assert!(!dispatched_first);
    assert!(ui.inline_editor.as_ref().unwrap().confirm_overwrite);
    let cached_blocks = storage.load_page_blocks(&app.workspace_id, "page-a").unwrap();
    assert!(cached_blocks.iter().all(|b| !b.id.starts_with("__optimistic_")));

    // TUI toggle_todo_block applies server block directly without reverting checked state
    app.discard_inline_editor(&mut ui);
    app.toggle_todo_block("blk-todo", false, &mut ui);
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg, UiMsg::PageRefreshed { .. }));
    app.handle_ui_msg(&mut ui, msg);
    assert!(matches!(
        app.active_page_detail.as_ref().unwrap().blocks[1].kind,
        NotionBlockKind::ToDo { checked: false, .. }
    ));

    server_handle.abort();
}

