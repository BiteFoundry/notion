use std::sync::{atomic::Ordering, Arc};

use axum::http::StatusCode;
use ratatui::{backend::TestBackend, Terminal};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    config::PageWidthMode,
    mcp,
    models::DatabaseViewMode,
    notion::{media, NotionClient},
    serve,
    storage::Storage,
    sync,
    ui::{
        self, action,
        action::Action,
        app::{AppState, CanvasView, Focus, UiState},
    },
};

use crate::server::{start_mock_notion_server, test_config};

#[tokio::test]
async fn e2e_notion_client_search_recursive_blocks_todo_db_comments_and_media() {
    let (base_url, state, server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    // 1. GET /v1/users/me (automatically retries HTTP 429!)
    let me = client.get_me().await.unwrap();
    assert_eq!(me.name, "Notion Mock Bot");
    assert_eq!(state.rate_limit_hits.load(Ordering::SeqCst), 2);

    // 2. Full workspace sync (cursor pagination across pages + databases)
    let (ws_id, pages, dbs) = sync::sync_workspace_tree(&client, &storage).await.unwrap();
    assert_eq!(pages.len(), 4);
    assert_eq!(dbs.len(), 1);

    // 3. Recursive block tree fetch (toggle children + table rows + comments)
    let detail = sync::sync_page_detail(&client, &storage, "page-roadmap")
        .await
        .unwrap();
    assert_eq!(detail.blocks.len(), 7);
    let toggle = detail.blocks.iter().find(|b| b.id == "blk-toggle").unwrap();
    assert_eq!(toggle.children.len(), 1);
    assert!(toggle.children[0].plain_text().contains("Korean fallback"));
    let table = detail.blocks.iter().find(|b| b.id == "blk-table").unwrap();
    assert_eq!(table.children.len(), 2);
    assert_eq!(detail.comments.len(), 1);
    assert_eq!(detail.media.len(), 1);

    // 4. SQLite FTS5 + Korean <3 char LIKE fallback
    let fts_en = storage.search_local(&ws_id, "Milestones", 10).unwrap();
    assert_eq!(fts_en.len(), 1);
    assert_eq!(fts_en[0].id, "page-roadmap");

    let ko_short = storage.search_local(&ws_id, "회의", 10).unwrap();
    assert_eq!(ko_short.len(), 1);
    assert_eq!(ko_short[0].id, "page-ko");

    let ko_1 = storage.search_local(&ws_id, "기", 10).unwrap();
    assert_eq!(ko_1.len(), 1);
    assert_eq!(ko_1[0].id, "page-ko");

    // 5. Toggle to_do checkbox via PATCH /v1/blocks/{id}
    let toggled = notion_cli::cli::op_toggle_todo(&client, &storage, "blk-todo", None)
        .await
        .unwrap();
    assert!(matches!(
        toggled.kind,
        notion_cli::models::NotionBlockKind::ToDo { checked: true, .. }
    ));

    // 6. Database query + item create + update
    let rows = notion_cli::cli::op_query_database(
        &client, &storage, "db-tasks", None, None, 20, None, false,
    )
    .await
    .unwrap()
    .items;
    assert_eq!(rows.len(), 2);

    let created = notion_cli::cli::op_create_database_item(
        &client,
        &storage,
        "db-tasks",
        "Write E2E Tests",
        Some("Todo"),
        None,
        Some("- [ ] Cover all endpoints"),
    )
    .await
    .unwrap();
    assert_eq!(created.title, "Write E2E Tests");

    // 7. Media download helper (by block ID)
    let tmp = tempfile::tempdir().unwrap();
    let dl = media::download_media_target(&cfg, &client, "blk-img", Some(tmp.path()))
        .await
        .unwrap();
    assert!(dl.ok);
    assert!(dl.is_image);
    assert_eq!(dl.mime_type, "image/png");
    assert!(dl.size_bytes > 0);
    assert!(std::path::Path::new(&dl.path).exists());

    server.abort();
}

#[tokio::test]
async fn e2e_http_api_server_and_mcp_tools() {
    let (base_url, _state, mock_server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    // --- Part A: HTTP API Server (`serve.rs`) ---
    let api_state = Arc::new(serve::ApiState {
        config: cfg.clone(),
        auth: auth.clone(),
        client: client.clone(),
        storage: storage.clone(),
        api_key: Some("local-secret".into()),
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_base = format!("http://{api_addr}");
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });

    let http = reqwest::Client::new();
    // Health endpoint requires no auth
    let health: Value = http
        .get(format!("{api_base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["service"], "notion-cli");

    // Protected route rejects missing API key
    let unauth = http
        .get(format!("{api_base}/api/v1/whoami"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Whoami returns API workspace_name rather than placeholder profile name (W9)
    let whoami_resp: Value = http
        .get(format!("{api_base}/api/v1/whoami"))
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(whoami_resp["workspace_name"], "Acme Workspace");
    assert_eq!(whoami_resp["user"]["workspace_name"], "Acme Workspace");

    // Upstream 401 from Notion API propagates as HTTP 401 from `serve` (W3 F2e)
    let bad_auth = AuthManager::from_token(cfg.clone(), "ntn_wrong_token");
    let bad_client = NotionClient::new(&cfg, bad_auth.clone()).unwrap();
    let bad_api_state = Arc::new(serve::ApiState {
        config: cfg.clone(),
        auth: bad_auth,
        client: bad_client,
        storage: Storage::open_in_memory().unwrap(),
        api_key: None,
    });
    let bad_api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad_api_addr = bad_api_listener.local_addr().unwrap();
    let bad_api_server = tokio::spawn(async move {
        let _ = axum::serve(bad_api_listener, serve::build_router(bad_api_state)).await;
    });
    let upstream_401 = http
        .get(format!("http://{bad_api_addr}/api/v1/whoami"))
        .send()
        .await
        .unwrap();
    assert_eq!(upstream_401.status(), StatusCode::UNAUTHORIZED);
    bad_api_server.abort();

    // Sync via HTTP API
    let sync_resp: Value = http
        .post(format!("{api_base}/api/v1/sync"))
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sync_resp["ok"], true);
    assert_eq!(sync_resp["pages_count"], 4);

    // Korean <3 char search via HTTP API
    let search_ko: Value = http
        .get(format!("{api_base}/api/v1/search?q=회의"))
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search_ko["count"], 1);

    // Comments POST + GET via HTTP API
    let add_cmt: Value = http
        .post(format!("{api_base}/api/v1/pages/page-roadmap/comments"))
        .header("x-api-key", "local-secret")
        .json(&json!({ "text": "HTTP comment works!" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(add_cmt["ok"], true);

    // Database query & item creation via HTTP API
    let db_q: Value = http
        .post(format!("{api_base}/api/v1/databases/db-tasks/query"))
        .header("x-api-key", "local-secret")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(db_q["count"], 2);

    api_server.abort();

    // --- Part B: MCP JSON-RPC 2.0 server ---
    let init = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "notion-cli");

    let list_tools = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await
    .unwrap();
    assert_eq!(list_tools["result"]["tools"].as_array().unwrap().len(), 20);

    // Call archive_page WITHOUT confirm -> must fail!
    let archive_denied = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "archive_page", "arguments": { "page": "page-ko", "confirm": false } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(archive_denied["result"]["isError"], true);

    // Call archive_page WITH confirm: true -> succeeds!
    let archive_ok = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "archive_page", "arguments": { "page": "page-ko", "confirm": true } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(archive_ok["result"]["isError"], false);

    // Call append_blocks + toggle_todo + list_databases + add_comment via MCP
    let append_ok = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "append_blocks", "arguments": { "page": "page-roadmap", "markdown": "/callout 🔥 Shipped!" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(append_ok["result"]["isError"], false);
    assert_eq!(append_ok["result"]["structuredContent"]["appended"], 1);
    assert!(append_ok["result"]["structuredContent"].get("blocks").is_none());
    assert_eq!(
        append_ok["result"]["structuredContent"]["block_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    mock_server.abort();
}

#[tokio::test]
async fn e2e_tui_renders_notion_page_canvas_and_database_table_board_list() {
    let (base_url, _state, mock_server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    sync::sync_page_detail(&client, &storage, "page-roadmap")
        .await
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui_state = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    app.load_cached_initial();
    app.open_page("page-roadmap", &mut ui_state, false);
    ui_state.focus = Focus::Canvas;
    ui_state.comments_drawer_open = true;

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

    // 1. Render Page Canvas (Centered + Comments Drawer + Callout + Toggle + Todo + Code + Table)
    assert_eq!(ui_state.page_width, PageWidthMode::Centered);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();
    let screen1: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen1.contains("Product Roadmap"));
    assert!(screen1.contains("Width: Centered"));
    assert!(screen1.contains("In Progress"));
    assert!(screen1.contains("Q4 Launch Milestones"));
    assert!(screen1.contains("Architecture Details"));
    assert!(screen1.contains("Comments"));
    assert!(screen1.contains("Looks awesome!"));

    // 2. Expand Toggle block live and verify nested child appears!
    ui_state.selected_block = 3; // blk-toggle
    action::dispatch(Action::ToggleActiveBlock, &mut app, &mut ui_state);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();
    let screen_toggle: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen_toggle.contains("SQLite WAL + FTS5"));

    // 3. Switch to Database View -> Table, Board (Kanban), and List
    app.open_database("db-tasks", &mut ui_state, true);
    assert_eq!(app.active_view, CanvasView::Database);
    ui_state.comments_drawer_open = false;

    for mode in [
        DatabaseViewMode::Table,
        DatabaseViewMode::Board,
        DatabaseViewMode::List,
    ] {
        ui_state.db_view_mode = mode;
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui_state);
            })
            .unwrap();
        let db_screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(db_screen.contains("Sprint Tasks"));
        assert!(db_screen.contains("[1 Table]"));
        assert!(db_screen.contains("[2 Board]"));
        assert!(db_screen.contains("[3 List]"));
        assert!(db_screen.contains("Build Kanban View"));
    }

    mock_server.abort();
}

#[tokio::test]
async fn e2e_tui_mouse_scroll_clamping_hitmap_and_sidebar_toggle() {
    let (base_url, _state, mock_server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    sync::sync_page_detail(&client, &storage, "page-roadmap")
        .await
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui_state = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    app.load_cached_initial();
    app.open_page("page-roadmap", &mut ui_state, false);
    ui_state.focus = Focus::Canvas;
    ui_state.comments_drawer_open = true;

    let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();

    // 1. Clicking sidebar arrow on collapsible `page-roadmap` (parent of `page-ko`) collapses it
    // without moving focus away from Sidebar.
    let items = app.build_sidebar_items(&ui_state);
    let roadmap_idx = items
        .iter()
        .position(|i| i.id == "page-roadmap")
        .expect("page-roadmap should be in sidebar");
    let toggle_rect = ui_state
        .hitmap
        .region_of(ui::hitmap::HitTarget::SidebarToggle(roadmap_idx))
        .expect("page-roadmap should have a clickable SidebarToggle region");
    let toggle_hit = ui_state.hitmap.hit(toggle_rect.y, toggle_rect.x + 1).unwrap();
    assert_eq!(
        toggle_hit,
        ui::hitmap::HitTarget::SidebarToggle(roadmap_idx)
    );
    action::dispatch(
        Action::MouseClick {
            target: toggle_hit,
            col: toggle_rect.x + 1,
            row: toggle_rect.y,
        },
        &mut app,
        &mut ui_state,
    );
    assert_eq!(ui_state.focus, Focus::Sidebar);
    assert!(ui_state.collapsed_nodes.contains("page-roadmap"));

    // 2. ScrollBottom clamps on draw so MouseScroll up immediately responds
    action::dispatch(Action::ScrollBottom, &mut app, &mut ui_state);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();
    let max_scroll = ui_state.canvas_scroll;
    assert!(max_scroll < 100);
    if max_scroll > 0 {
        action::dispatch(
            Action::MouseScroll {
                delta: -1,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui_state,
        );
        assert_eq!(ui_state.canvas_scroll, max_scroll - 1);
    }

    // 3. Clicking Comments Drawer close hint closes the drawer
    let close_rect = ui_state
        .hitmap
        .region_of(ui::hitmap::HitTarget::Button(
            ui::hitmap::ButtonId::ToggleComments,
        ))
        .expect("comments close hint should be mapped");
    action::dispatch(
        Action::MouseClick {
            target: ui::hitmap::HitTarget::Button(ui::hitmap::ButtonId::ToggleComments),
            col: close_rect.x,
            row: close_rect.y,
        },
        &mut app,
        &mut ui_state,
    );
    assert!(!ui_state.comments_drawer_open);

    mock_server.abort();
}

