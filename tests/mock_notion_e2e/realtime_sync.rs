use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    notion::NotionClient,
    storage::Storage,
    sync,
    ui::app::{AppState, UiState},
};

use crate::server::{test_config, CommentQuery};

#[tokio::test]
async fn e2e_realtime_sync_dedup_churn_sidebar_merge_and_rate_limit() {
    use notion_cli::models::{CoreEvent, NotionPageSummary};
    use notion_cli::sync::{FocusTarget, SyncCommand, SyncController};
    use tokio::sync::oneshot;

    #[derive(Default)]
    struct RtMock {
        slow_delay_ms: u64,
        return_429_once: bool,
        return_529_once: bool,
        return_529_blocked: bool,
        para_by_page: std::collections::HashMap<String, String>,
        nested_text: String,
        title_alpha: String,
        icon_alpha: String,
        comments_alpha: Vec<Value>,
        hits: Vec<String>,
        block_response_gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    }

    let state = Arc::new(Mutex::new(RtMock {
        slow_delay_ms: 0,
        return_429_once: false,
        return_529_once: false,
        return_529_blocked: false,
        para_by_page: [
            ("page-a".to_string(), "Alpha V1".to_string()),
            ("page-b".to_string(), "Beta V1".to_string()),
            ("page-c".to_string(), "Gamma V1".to_string()),
            ("page-d".to_string(), "Delta V1".to_string()),
        ]
        .into_iter()
        .collect(),
        nested_text: "Nested V1".to_string(),
        title_alpha: "Alpha Spec".to_string(),
        icon_alpha: "🚀".to_string(),
        comments_alpha: vec![json!({
            "id": "c-1",
            "discussion_id": "d-1",
            "created_time": "2026-11-20T01:00:00.000Z",
            "created_by": { "id": "u-1", "name": "Alice" },
            "rich_text": [{ "plain_text": "Comment 1" }]
        })],
        hits: Vec::new(),
        block_response_gate: None,
    }));

    let app_router = Router::new()
        .route(
            "/v1/users/me",
            get(|State(st): State<Arc<Mutex<RtMock>>>| async move {
                st.lock().unwrap().hits.push("GET /users/me".into());
                Json(json!({
                    "object": "user", "id": "bot-rt", "name": "RT Bot",
                    "type": "bot", "bot": { "workspace_name": "RT WS" }
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/search",
            post(|State(st): State<Arc<Mutex<RtMock>>>| async move {
                let mut g = st.lock().unwrap();
                g.hits.push("POST /search".into());
                let title_a = g.title_alpha.clone();
                let icon_a = g.icon_alpha.clone();
                Json(json!({
                    "object": "list",
                    "results": [
                        {
                            "object": "page", "id": "page-a", "url": "https://notion.so/page-a",
                            "icon": { "type": "emoji", "emoji": icon_a },
                            "parent": { "type": "workspace", "workspace": true },
                            "archived": false, "last_edited_time": "2026-11-20T01:00:00.000Z",
                            "properties": { "title": { "type": "title", "title": [{ "plain_text": title_a }] } }
                        },
                        {
                            "object": "page", "id": "page-d", "url": "https://notion.so/page-d",
                            "icon": { "type": "emoji", "emoji": "📝" },
                            "parent": { "type": "workspace", "workspace": true },
                            "archived": false, "last_edited_time": "2026-11-20T01:00:00.000Z",
                            "properties": { "title": { "type": "title", "title": [{ "plain_text": "Delta" }] } }
                        }
                    ],
                    "has_more": false,
                    "next_cursor": Value::Null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(|State(st): State<Arc<Mutex<RtMock>>>, AxumPath(id): AxumPath<String>| async move {
                let (delay, do_429, do_529, do_529_blocked, title, icon) = {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /pages/{id}"));
                    let d = g.slow_delay_ms;
                    let r429 = std::mem::take(&mut g.return_429_once);
                    let r529 = std::mem::take(&mut g.return_529_once);
                    let r529b = std::mem::take(&mut g.return_529_blocked);
                    let t = if id == "page-a" { g.title_alpha.clone() } else { format!("Title {id}") };
                    let ic = if id == "page-a" { g.icon_alpha.clone() } else { "📝".to_string() };
                    (d, r429, r529, r529b, t, ic)
                };
                if do_429 {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "0.2")],
                        Json(json!({ "code": "rate_limited", "message": "Slow down" })),
                    )
                        .into_response();
                }
                if do_529 {
                    return (
                        StatusCode::from_u16(529).unwrap(),
                        [(header::RETRY_AFTER, "0.15")],
                        Json(json!({ "code": "service_overload", "message": "Overloaded" })),
                    )
                        .into_response();
                }
                if do_529_blocked {
                    return (
                        StatusCode::from_u16(529).unwrap(),
                        [(header::RETRY_AFTER, "1")],
                        Json(json!({ "code": "public_api_request_blocked", "message": "Blocked" })),
                    )
                        .into_response();
                }
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Json(json!({
                    "object": "page", "id": id, "url": format!("https://notion.so/{id}"),
                    "icon": { "type": "emoji", "emoji": icon },
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false, "last_edited_time": "2026-11-20T01:00:00.000Z",
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": title }] } }
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(|State(st): State<Arc<Mutex<RtMock>>>, AxumPath(id): AxumPath<String>| async move {
                let (delay, results, gate) = {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /blocks/{id}/children"));
                    let d = g.slow_delay_ms;
                    let res = if id == "blk-nested-parent" {
                        vec![json!({
                            "id": "blk-nested-child",
                            "type": "paragraph",
                            "has_children": false,
                            "paragraph": { "rich_text": [{ "plain_text": g.nested_text.clone() }] }
                        })]
                    } else {
                        let txt = g
                            .para_by_page
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| format!("Body {id}"));
                        let mut v = vec![json!({
                            "id": format!("blk-p-{id}"),
                            "type": "paragraph",
                            "has_children": false,
                            "paragraph": { "rich_text": [{ "plain_text": txt }] }
                        })];
                        if id == "page-a" {
                            v.push(json!({
                                "id": "blk-nested-parent",
                                "type": "bulleted_list_item",
                                "has_children": true,
                                "bulleted_list_item": { "rich_text": [{ "plain_text": "Bullet" }] }
                            }));
                        }
                        v
                    };
                    let gate = if id == "page-a" {
                        g.block_response_gate.take()
                    } else {
                        None
                    };
                    (d, res, gate)
                };
                if let Some((received, resume)) = gate {
                    let _ = received.send(());
                    let _ = resume.await;
                }
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Json(json!({
                    "object": "list", "results": results, "has_more": false, "next_cursor": Value::Null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/comments",
            get(|State(st): State<Arc<Mutex<RtMock>>>, Query(q): Query<CommentQuery>| async move {
                let mut g = st.lock().unwrap();
                g.hits.push(format!("GET /comments?block_id={}", q.block_id));
                let list = if q.block_id == "page-a" {
                    g.comments_alpha.clone()
                } else {
                    Vec::new()
                };
                Json(json!({
                    "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null
                }))
                .into_response()
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let mut cfg = test_config(&base_url);
    cfg.active_poll_ms = 3_000;
    cfg.sync_poll_ms = 60_000;
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let storage = Storage::open(&cache_path).unwrap();

    // 1. 429 & 529 Retry-After handling (and 529 public_api_request_blocked does NOT retry)
    state.lock().unwrap().return_429_once = true;
    let t0 = std::time::Instant::now();
    let p = client.get_page("page-a").await.unwrap();
    assert_eq!(p.id, "page-a");
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(160),
        "429 Retry-After 0.2s must be honored"
    );

    state.lock().unwrap().return_529_once = true;
    let t1 = std::time::Instant::now();
    let p2 = client.get_page("page-a").await.unwrap();
    assert_eq!(p2.id, "page-a");
    assert!(
        t1.elapsed() >= std::time::Duration::from_millis(120),
        "529 service_overload Retry-After 0.15s must be honored"
    );

    state.lock().unwrap().hits.clear();
    state.lock().unwrap().return_529_blocked = true;
    let blocked_err = client.get_page("page-a").await.unwrap_err();
    assert!(blocked_err.to_string().contains("public_api_request_blocked"));
    assert_eq!(
        state
            .lock()
            .unwrap()
            .hits
            .iter()
            .filter(|h| *h == "GET /pages/page-a")
            .count(),
        1,
        "public_api_request_blocked must not retry"
    );

    // Seed workspace tree & initial page detail
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    sync::sync_page_detail_force(&client, &storage, "page-a")
        .await
        .unwrap();

    // 2. Coalesced SetFocus + explicit RefreshPage on an already-cached unchanged page MUST:
    //    - fetch the block tree only once (deduped)
    //    - deliver PageDetailUpdated (explicit command result delivery)
    //    - not flash SyncStateChanged { busy: true }
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<CoreEvent>();
    let ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), ev_tx);
    state.lock().unwrap().slow_delay_ms = 180;
    state.lock().unwrap().hits.clear();
    ctrl.set_focus(FocusTarget {
        page_id: Some("page-a".into()),
        database_id: None,
    });
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let root_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        root_gets, 1,
        "coalesced SetFocus + overlapping RefreshPage must share one in-flight fetch"
    );
    let mut saw_busy = false;
    let mut page_events = 0usize;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            CoreEvent::SyncStateChanged { busy: true, .. } => saw_busy = true,
            CoreEvent::PageDetailUpdated(_) => page_events += 1,
            _ => {}
        }
    }
    assert!(!saw_busy, "explicit RefreshPage must not flash busy=true");
    assert_eq!(
        page_events, 1,
        "explicit SetFocus+RefreshPage on unchanged cached page must still deliver PageDetailUpdated"
    );

    // 2b. Unchanged PERIODIC active poll (~3s) fetches body directly once and stays completely silent
    //     (0 PageDetailUpdated, 0 busy=true).
    state.lock().unwrap().slow_delay_ms = 0;
    state.lock().unwrap().hits.clear();
    let periodic_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < periodic_deadline {
        if state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.as_str() == "GET /blocks/page-a/children")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let periodic_block_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        periodic_block_gets, 1,
        "periodic active poll must directly check open page body once"
    );
    let mut periodic_busy = false;
    let mut periodic_page_events = 0usize;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            CoreEvent::SyncStateChanged { busy: true, .. } => periodic_busy = true,
            CoreEvent::PageDetailUpdated(_) => periodic_page_events += 1,
            _ => {}
        }
    }
    assert!(!periodic_busy, "unchanged periodic poll must not flash busy");
    assert_eq!(
        periodic_page_events, 0,
        "unchanged periodic poll must emit 0 PageDetailUpdated events"
    );

    // 2c. Upgrading an already-running silent periodic fetch with an explicit RefreshPage:
    //     must share ONE network fetch (0 extra GETs) AND emit PageDetailUpdated even when unchanged!
    state.lock().unwrap().slow_delay_ms = 240;
    state.lock().unwrap().hits.clear();
    let upgrade_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < upgrade_deadline {
        if state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.as_str() == "GET /blocks/page-a/children")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    // Silent periodic fetch is now in flight (~240ms delay): send explicit RefreshPage while in flight!
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    let upgraded_ev = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(d)) = ev_rx.recv().await {
                break *d;
            }
        }
    })
    .await
    .expect("upgrading in-flight silent periodic fetch with RefreshPage must deliver PageDetailUpdated");
    assert_eq!(upgraded_ev.page.id, "page-a");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let upgrade_block_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        upgrade_block_gets, 1,
        "upgrading in-flight silent periodic fetch must not issue a second GET"
    );

    // 3. Same-timestamp nested block + comment + title/icon update picked up by quiet RefreshPage with 0 POST /search
    {
        let mut g = state.lock().unwrap();
        g.slow_delay_ms = 0;
        g.nested_text = "Nested V2 same-ts".into();
        g.title_alpha = "Alpha Renamed".into();
        g.icon_alpha = "🔥".into();
        g.comments_alpha.push(json!({
            "id": "c-2",
            "discussion_id": "d-2",
            "created_time": "2026-11-20T01:00:30.000Z",
            "created_by": { "id": "u-2", "name": "Bob" },
            "rich_text": [{ "plain_text": "Comment 2 same-ts" }]
        }));
        g.hits.clear();
    }
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    let detail_v2 = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(d)) = ev_rx.recv().await {
                break *d;
            }
        }
    })
    .await
    .expect("same-ts nested/comment/title change must emit PageDetailUpdated");
    assert_eq!(detail_v2.page.title, "Alpha Renamed");
    assert_eq!(detail_v2.page.icon.as_deref(), Some("🔥"));
    assert_eq!(
        detail_v2.blocks[1].children[0].plain_text(),
        "Nested V2 same-ts"
    );
    assert_eq!(detail_v2.comments.len(), 2);
    assert!(
        !state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h == "POST /search"),
        "visible page refresh must execute 0 workspace searches"
    );

    // A separate forced refresh can commit while the quiet poll is still awaiting its response.
    let (received, request) = oneshot::channel();
    let (resume, release_response) = oneshot::channel();
    {
        let mut g = state.lock().unwrap();
        g.para_by_page.insert("page-a".into(), "Concurrent refresh".into());
        g.block_response_gate = Some((received, release_response));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("periodic poll must request page blocks")
        .unwrap();
    let other_storage = Storage::open(&cache_path).unwrap();
    let written = sync::sync_page_detail_force(&client, &other_storage, "page-a")
        .await
        .unwrap();
    assert_eq!(written.blocks[0].plain_text(), "Concurrent refresh");
    resume.send(()).unwrap();
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(detail)) = ev_rx.recv().await {
                break detail;
            }
        }
    })
    .await
    .expect("the periodic poll must deliver the edit already written by the other connection");
    assert_eq!(delivered.blocks[0].plain_text(), "Concurrent refresh");

    // 4. Rapid focus churn A -> B -> C -> D cannot lose latest target D!
    state.lock().unwrap().slow_delay_ms = 120;
    while ev_rx.try_recv().is_ok() {}
    for pid in ["page-a", "page-b", "page-c", "page-d"] {
        ctrl.set_focus(FocusTarget {
            page_id: Some(pid.into()),
            database_id: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let final_detail = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(d)) = ev_rx.recv().await {
                if d.page.id == "page-d" {
                    break *d;
                }
            }
        }
    })
    .await
    .expect("rapid focus churn must converge on latest target page-d");
    assert_eq!(final_detail.blocks[0].plain_text(), "Delta V1");

    ctrl.request(SyncCommand::Shutdown);

    // 5. UI merge: PageDetailUpdated updates sidebar title/icon + Canvas in one update
    //    while preserving selection, scroll, and dirty inline editor draft.
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
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
    app.pages = vec![NotionPageSummary {
        id: "page-a".into(),
        workspace_id: app.workspace_id.clone(),
        title: "Alpha Spec".into(),
        icon: Some("🚀".into()),
        is_favorite: true,
        ..Default::default()
    }];
    app.open_page("page-a", &mut ui, false);
    ui.selected_block = 1;
    ui.canvas_scroll = 7;
    ui.sidebar_selected = 2;

    let changed_redraw =
        app.handle_core_event(&mut ui, CoreEvent::PageDetailUpdated(Box::new(detail_v2.clone())));
    assert!(changed_redraw, "changed PageDetailUpdated must trigger redraw");
    let unchanged_redraw =
        app.handle_core_event(&mut ui, CoreEvent::PageDetailUpdated(Box::new(detail_v2)));
    assert!(
        !unchanged_redraw,
        "identical unchanged PageDetailUpdated must not trigger redundant TUI redraw"
    );
    assert_eq!(app.pages[0].title, "Alpha Renamed");
    assert_eq!(app.pages[0].icon.as_deref(), Some("🔥"));
    assert!(app.pages[0].is_favorite, "local favorite flag must be preserved");
    assert_eq!(
        app.active_page_detail.as_ref().unwrap().page.title,
        "Alpha Renamed"
    );
    assert_eq!(ui.selected_block, 1, "selected block must be preserved");
    assert_eq!(ui.canvas_scroll, 7, "canvas scroll must be preserved");
    assert_eq!(ui.sidebar_selected, 2, "sidebar selection must be preserved");

    // refresh_active_item dispatches RefreshPage for active page (FocusGained path)
    while cmd_rx.try_recv().is_ok() {}
    app.refresh_active_item();
    assert_eq!(
        cmd_rx.try_recv().ok(),
        Some(SyncCommand::RefreshPage {
            page_id: "page-a".into()
        })
    );

    srv_handle.abort();
}

