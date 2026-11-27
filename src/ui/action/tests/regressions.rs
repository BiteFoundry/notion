use std::collections::BTreeMap;

use ratatui::{backend::TestBackend, layout::Rect, Terminal};

use super::make_test_app_and_ui;
use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{
        DatabaseViewMode, NotionBlock, NotionBlockKind, NotionColor, NotionComment, NotionDatabase,
        NotionMediaRef, NotionPage, NotionPageDetail, NotionPropertyValue, RichTextSpan,
        SelectOption,
    },
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::{
        self,
        action::{dispatch, Action},
        app::{AppState, CanvasView, Focus, NarrowPane, UiMsg, UiState},
        hitmap::{ButtonId, HitTarget},
        overlays::{context_menu::ContextTarget, Overlay, OverlayKind, OverlayOutcome},
    },
};

#[tokio::test]
async fn test_sidebar_classification_block_id_nesting_db_exclusion_and_natural_sort() {
    use axum::{
        extract::Path as AxumPath,
        routing::{get, post},
        Json, Router,
    };
    use tokio::net::TcpListener;

    let router = Router::new()
        .route(
            "/v1/search",
            post(|| async {
                Json(serde_json::json!({
                    "object": "list",
                    "results": [
                        {
                            "object": "page",
                            "id": "page-parent-a",
                            "url": "https://notion.so/page-parent-a",
                            "parent": { "type": "workspace", "workspace": true },
                            "archived": false,
                            "last_edited_time": "2026-11-20T05:00:00Z",
                            "properties": {
                                "title": { "type": "title", "title": [{ "plain_text": "Idea Pet" }] }
                            }
                        },
                        {
                            "object": "page",
                            "id": "page-sub-via-api",
                            "url": "https://notion.so/page-sub-via-api",
                            "parent": { "type": "block_id", "block_id": "blk-nested-col" },
                            "archived": false,
                            "last_edited_time": "2026-11-20T06:00:00Z",
                            "properties": {
                                "title": { "type": "title", "title": [{ "plain_text": "03. API Nested Subpage" }] }
                            }
                        }
                    ],
                    "has_more": false
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                let parent = match id.as_str() {
                    "blk-nested-col" => serde_json::json!({ "type": "block_id", "block_id": "blk-heading-2" }),
                    "blk-heading-2" => serde_json::json!({ "type": "page_id", "page_id": "page-parent-a" }),
                    _ => serde_json::json!({ "type": "workspace", "workspace": true }),
                };
                Json(serde_json::json!({
                    "object": "block",
                    "id": id,
                    "type": "heading_2",
                    "parent": parent
                }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_sidebar_test");
    let client = NotionClient::new(&cfg, auth.clone())
        .unwrap()
        .with_base_url(format!("http://{addr}/v1"));
    let storage = Storage::open_in_memory().unwrap();

    // 1. Online sync resolves multi-hop block_id -> block_id -> page_id into SQLite block_owners!
    let (ws_id, _, _) = crate::sync::sync_workspace_tree(&client, &storage)
        .await
        .unwrap();
    let (owners, _) = storage.load_page_parent_hints(&ws_id).unwrap();
    assert_eq!(
        owners.get("blk-nested-col").map(String::as_str),
        Some("page-parent-a")
    );
    assert_eq!(
        owners.get("blk-heading-2").map(String::as_str),
        Some("page-parent-a")
    );

    // 2. Add local blocks + numbered sub-pages (out of order by last_edited_time),
    //    plus a database_id row (with NO matching database in app.databases!) and an Untitled root.
    storage
        .replace_page_blocks(
            &ws_id,
            "page-parent-a",
            &[NotionBlock {
                id: "blk-local-toggle".into(),
                page_id: "page-parent-a".into(),
                block_type: "toggle".into(),
                kind: NotionBlockKind::Toggle {
                    rich_text: vec![RichTextSpan::plain("Section")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            }],
        )
        .unwrap();

    let mk_page = |id: &str, title: &str, ptype: &str, pid: Option<&str>, edited: &str| NotionPage {
        id: id.into(),
        workspace_id: ws_id.clone(),
        parent_type: ptype.into(),
        parent_id: pid.map(str::to_string),
        title: title.into(),
        last_edited_time: Some(edited.into()),
        ..Default::default()
    };

    storage
        .upsert_pages(&[
            mk_page("page-untitled", "Untitled", "workspace", None, "2026-11-20T09:00:00Z"),
            mk_page("page-alpha", "Alpha Root", "workspace", None, "2026-11-20T01:00:00Z"),
            mk_page("page-db-row", "Orphan DB Row", "database_id", Some("db-unloaded"), "2026-11-20T09:30:00Z"),
            mk_page("sub-05", "05. Fifth", "page_id", Some("page-parent-a"), "2026-11-20T08:00:00Z"),
            mk_page("sub-02", "02. Second", "block_id", Some("blk-local-toggle"), "2026-11-20T07:00:00Z"),
            mk_page("sub-01", "01. First", "block_id", Some("blk-local-toggle"), "2026-11-20T02:00:00Z"),
            mk_page("sub-04", "04. Fourth", "page_id", Some("page-parent-a"), "2026-11-20T06:00:00Z"),
        ])
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let ui = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    app.workspace_id = ws_id.clone();
    app.load_cached_initial();
    assert!(app.databases.is_empty(), "databases intentionally empty for exclusion test");

    let items = app.build_sidebar_items(&ui);
    let ws_items: Vec<&crate::ui::app::SidebarItem> =
        items.iter().filter(|i| !i.is_section_header).collect();

    // database_id row MUST NOT appear in top-level WORKSPACE roots!
    assert!(
        !ws_items.iter().any(|i| i.id == "page-db-row"),
        "database_id rows must NEVER be dumped into WORKSPACE roots"
    );

    // Top-level roots: Alpha Root, Idea Pet, Untitled (Untitled pushed to bottom!)
    let root_ids: Vec<&str> = ws_items
        .iter()
        .filter(|i| i.depth == 0)
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(root_ids, vec!["page-alpha", "page-parent-a", "page-untitled"]);

    // All 5 sub-pages (2 from local toggle block_id, 1 from API block_owners, 2 from page_id)
    // must nest at depth 1 under Idea Pet in exact natural order: 01 -> 02 -> 03 -> 04 -> 05!
    let parent_item = ws_items.iter().find(|i| i.id == "page-parent-a").unwrap();
    assert!(parent_item.collapsible);
    let child_ids: Vec<&str> = ws_items
        .iter()
        .filter(|i| i.depth == 1)
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(
        child_ids,
        vec!["sub-01", "sub-02", "page-sub-via-api", "sub-04", "sub-05"]
    );

    // When child_page block positions exist in `blocks`, exact Notion page order wins!
    app.child_page_positions.insert("sub-05".into(), 0);
    app.child_page_positions.insert("sub-01".into(), 1);
    let reordered = app.build_sidebar_items(&ui);
    let reordered_kids: Vec<&str> = reordered
        .iter()
        .filter(|i| !i.is_section_header && i.depth == 1)
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(reordered_kids[0], "sub-05");
    assert_eq!(reordered_kids[1], "sub-01");

    server_handle.abort();
}
#[tokio::test]
async fn tree_updates_of_the_previous_workspace_are_dropped_after_a_switch() {
    use crate::models::{CoreEvent, NotionPageSummary};
    use crate::sync::SyncCommand;
    use crate::ui::app::UiMsg;

    let _lock = crate::config::TEST_ENV_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_string_lossy().to_string();
    let _guard = crate::config::EnvVarGuard::set(&[("NOTION_DATA_DIR", Some(&data_dir))]);
    let (mut app, mut ui_state) = make_test_app_and_ui();
    let (focus_tx, focus_rx) = tokio::sync::watch::channel(FocusTarget {
        page_id: Some("page-root".into()),
        database_id: None,
    });
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    app.sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let tree = |workspace_id: &str, page_id: &str| CoreEvent::WorkspaceTreeUpdated {
        workspace_id: workspace_id.into(),
        pages: vec![NotionPageSummary {
            id: page_id.into(),
            title: page_id.into(),
            ..Default::default()
        }],
        databases: Vec::new(),
    };
    let page_ids = |app: &AppState| app.pages.iter().map(|p| p.id.clone()).collect::<Vec<_>>();

    // Before any switch a tree update names the workspace, whatever it is.
    assert!(app.handle_core_event(&mut ui_state, tree("A", "page-a")));
    assert_eq!(app.workspace_id, "A");
    assert_eq!(page_ids(&app), vec!["page-a"]);

    // Without a supervisor the switch asks the running engine for a full refresh.
    while cmd_rx.try_recv().is_ok() {}
    app.handle_ui_msg(
        &mut ui_state,
        UiMsg::WorkspaceSwitched { name: "b".into(), workspace_id: "B".into() },
    );
    assert!(matches!(cmd_rx.try_recv(), Ok(SyncCommand::RefreshAll)));
    assert_eq!(app.workspace_id, "B");
    assert_eq!(app.config.active_workspace, "b");
    assert_eq!(*focus_rx.borrow(), FocusTarget::default(), "the old page is no longer polled");
    assert!(page_ids(&app).is_empty(), "the new workspace's cache is empty");

    // A sync of the previous workspace that finishes late changes nothing.
    assert!(!app.handle_core_event(&mut ui_state, tree("A", "page-late")));
    assert_eq!(app.workspace_id, "B");
    assert!(page_ids(&app).is_empty());

    // The new workspace's tree is applied.
    assert!(app.handle_core_event(&mut ui_state, tree("B", "page-b")));
    assert_eq!(app.workspace_id, "B");
    assert_eq!(page_ids(&app), vec!["page-b"]);

    // With a supervisor the switch is handed to it and the old engine gets no refresh.
    let (switch_tx, mut switch_rx) = tokio::sync::mpsc::unbounded_channel();
    app.workspace_switch_tx = Some(switch_tx);
    while cmd_rx.try_recv().is_ok() {}
    app.handle_ui_msg(
        &mut ui_state,
        UiMsg::WorkspaceSwitched { name: "c".into(), workspace_id: "C".into() },
    );
    assert_eq!(switch_rx.try_recv().unwrap(), "C");
    assert!(cmd_rx.try_recv().is_err());
    assert!(!app.handle_core_event(&mut ui_state, tree("B", "page-b")));
    assert_eq!(app.workspace_id, "C");
}

#[test]
fn regression_p0_kanban_oob_clamp_windowing_hover_column_and_no_status_skip() {
    let (mut app, mut ui) = make_test_app_and_ui();
    app.open_database("db-1", &mut ui, false);
    ui.focus = Focus::Canvas;
    ui.db_view_mode = DatabaseViewMode::Board;

    // Populate 10 rows in "Todo" and 1 row with no status (synthetic "No Status" column).
    let mut rows = Vec::new();
    for i in 0..10 {
        let mut props = BTreeMap::new();
        props.insert(
            "Status".into(),
            NotionPropertyValue::Status(Some(SelectOption {
                id: Some("s1".into()),
                name: "Todo".into(),
                color: NotionColor::Blue,
            })),
        );
        rows.push(NotionPage {
            id: format!("card-{i}"),
            title: format!("Card {i}"),
            parent_type: "database_id".into(),
            parent_id: Some("db-1".into()),
            properties: props,
            ..Default::default()
        });
    }
    rows.push(NotionPage {
        id: "card-nostatus".into(),
        title: "Untriaged".into(),
        parent_type: "database_id".into(),
        parent_id: Some("db-1".into()),
        ..Default::default()
    });
    app.active_database_rows = rows;

    // Force out-of-bounds column/row and render: must NOT panic!
    ui.db_board_col = 99;
    ui.db_board_row = 99;
    let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();

    // ApplyDbFilter resets and clamps db_board_col/row cleanly without panic.
    dispatch(Action::ApplyDbFilter("Card 9".into()), &mut app, &mut ui);
    assert_eq!(ui.db_board_col, 0);
    assert_eq!(ui.db_board_row, 0);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();

    // Clear filter and move down to card 8 -> vertical scroll windowing brings Card 8 on screen!
    dispatch(Action::ApplyDbFilter(String::new()), &mut app, &mut ui);
    ui.db_board_col = 0;
    ui.db_board_row = 8;
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        screen.contains("Card 8"),
        "windowed Kanban board should render selected card past row 6"
    );

    // Hover-scroll over column 1 container hit target (empty space below cards) targets column 1.
    let col1_rect = ui
        .hitmap
        .region_of(HitTarget::DbBoardColumn(1))
        .expect("column 1 should register DbBoardColumn hit target");
    ui.db_board_col = 0;
    dispatch(
        Action::MouseScroll {
            delta: 1,
            column: col1_rect.x + 1,
            row: col1_rect.bottom().saturating_sub(1),
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.db_board_col, 1);

    // BoardColumn for "No Status" is synthetic, while schema columns ("Todo", "Done") are not.
    let cols = app.build_board_columns(&ui);
    assert!(cols.iter().any(|c| c.label == "No Status" && c.is_synthetic));
    assert!(cols.iter().any(|c| c.label == "Done" && !c.is_synthetic));

    // BoardCardMoveFailed rolls back optimistic col/row selection.
    ui.db_board_col = 1;
    ui.db_board_row = 0;
    app.handle_ui_msg(
        &mut ui,
        UiMsg::BoardCardMoveFailed {
            database_id: "db-1".into(),
            prev_col: 0,
            prev_row: 3,
            error: "rate limited".into(),
        },
    );
    assert_eq!(ui.db_board_col, 0);
    assert_eq!(ui.db_board_row, 3);
}

#[test]
fn regression_p0_sidebar_skips_headers_and_never_opens_synthetic_header() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Sidebar;
    ui.sidebar_selected = 0; // synthetic "__sec_workspace" header
    let items = app.build_sidebar_items(&ui);
    assert!(items[0].is_section_header);

    // Activate on header 0 must NOT open "__sec_workspace" and must snap to first non-header.
    let before_page = app.active_page_id.clone();
    dispatch(Action::Activate, &mut app, &mut ui);
    assert_eq!(app.active_page_id, before_page);
    assert_ne!(app.active_page_id.as_deref(), Some("__sec_workspace"));
    assert!(!items[ui.sidebar_selected].is_section_header);

    // MoveUp / MoveDown / PageDown / ScrollTop / ScrollBottom never land on section headers.
    for act in [
        Action::MoveUp,
        Action::MoveDown,
        Action::MoveDown,
        Action::MoveDown,
        Action::MoveDown,
        Action::PageDown,
        Action::PageUp,
        Action::ScrollBottom,
        Action::ScrollTop,
    ] {
        dispatch(act, &mut app, &mut ui);
        let cur_items = app.build_sidebar_items(&ui);
        assert!(
            !cur_items[ui.sidebar_selected].is_section_header,
            "sidebar_selected={} landed on header {:?}",
            ui.sidebar_selected,
            cur_items[ui.sidebar_selected].id
        );
    }
}

#[test]
fn regression_p0_settings_page_width_and_inline_hrefs_toggle_via_keyboard_and_mouse() {
    let (mut app, mut ui) = make_test_app_and_ui();
    dispatch(Action::OpenOverlay(OverlayKind::Settings), &mut app, &mut ui);

    // Keyboard: move to index 1 ("Page Width Mode") and activate
    let before_width = ui.page_width;
    dispatch(Action::MoveDown, &mut app, &mut ui);
    dispatch(Action::Activate, &mut app, &mut ui);
    assert_ne!(
        ui.page_width, before_width,
        "Settings 'Page Width Mode' must not be swallowed by overlay"
    );
    assert!(matches!(ui.overlays.last(), Some(Overlay::Settings(_))));

    // Mouse click on index 5 ("Inline Link [href] URLs")
    let before_hrefs = ui.show_inline_hrefs;
    dispatch(
        Action::MouseClick {
            target: HitTarget::OverlayItem { index: 5 },
            col: 20,
            row: 10,
        },
        &mut app,
        &mut ui,
    );
    assert_ne!(
        ui.show_inline_hrefs, before_hrefs,
        "Settings 'Inline Link [href] URLs' must toggle on mouse click"
    );
    assert!(matches!(ui.overlays.last(), Some(Overlay::Settings(_))));
}

#[test]
fn regression_p0_database_view_blocks_hidden_page_mutations_and_clears_filter_on_switch() {
    let (mut app, mut ui) = make_test_app_and_ui();
    assert_eq!(app.active_view, CanvasView::Page);
    assert_eq!(app.active_page_id.as_deref(), Some("page-root"));

    // Switch to database -> clears hidden active_page_id/detail and closes comments drawer.
    ui.comments_drawer_open = true;
    ui.focus = Focus::Comments;
    app.open_database("db-1", &mut ui, true);
    assert_eq!(app.active_view, CanvasView::Database);
    assert!(app.active_page_id.is_none());
    assert!(app.active_page_detail.is_none());
    assert!(!ui.comments_drawer_open);
    assert_eq!(ui.focus, Focus::Canvas);

    // Even if active_page_id were somehow stale, Database view blocks page-only actions.
    app.active_page_id = Some("page-root".into());
    dispatch(Action::OpenQuickAppend, &mut app, &mut ui);
    assert!(ui.overlays.is_empty());
    dispatch(Action::RequestExternalEditor, &mut app, &mut ui);
    assert!(!ui.editor_requested);
    dispatch(Action::ToggleCommentsDrawer, &mut app, &mut ui);
    assert!(!ui.comments_drawer_open);
    assert_ne!(ui.focus, Focus::Comments);
    dispatch(Action::OpenLinkExtractor, &mut app, &mut ui);
    assert!(ui.overlays.is_empty());
    dispatch(Action::OpenImagePreview, &mut app, &mut ui);
    assert!(ui.overlays.is_empty());

    // ConfirmArchivePage in Database view targets selected DB row (never hidden page-root).
    app.active_database_rows = vec![NotionPage {
        id: "db-row-42".into(),
        title: "DB Row 42".into(),
        parent_type: "database_id".into(),
        parent_id: Some("db-1".into()),
        ..Default::default()
    }];
    dispatch(Action::ConfirmArchivePage, &mut app, &mut ui);
    match ui.overlays.last() {
        Some(Overlay::Confirm(dlg)) => {
            assert_eq!(
                dlg.on_confirm,
                Some(Action::ArchivePageById("db-row-42".into()))
            );
        }
        other => panic!("expected Confirm dialog for selected DB row, got {other:?}"),
    }
    ui.overlays.clear();

    // Switching to a different database clears ui.db_filter.
    ui.db_filter = "sprint".into();
    app.open_database("db-2", &mut ui, false);
    assert!(ui.db_filter.is_empty());
}

#[test]
fn regression_p0_conflicted_inline_editor_aborts_navigation_instead_of_hijacking_next_page() {
    let (mut app, mut ui) = make_test_app_and_ui();
    assert!(app.open_inline_editor(&mut ui));
    dispatch(Action::Input('Z'), &mut app, &mut ui);
    ui.inline_editor.as_mut().unwrap().last_sync_error = Some("save conflict".into());

    // Attempting to open another page or database must abort and keep user on page-root.
    assert!(!app.try_open_page("page-sibling", &mut ui, true));
    assert_eq!(app.active_page_id.as_deref(), Some("page-root"));
    assert!(ui.inline_editor.is_some());

    assert!(!app.try_open_database("db-1", &mut ui, true));
    assert_eq!(app.active_view, CanvasView::Page);
    assert_eq!(app.active_page_id.as_deref(), Some("page-root"));
    assert!(ui.inline_editor.is_some());

    // Remote conflict also blocks switching until user resolves or discards.
    let detail_clone = app.active_page_detail.clone().unwrap();
    let ed = ui.inline_editor.as_mut().unwrap();
    ed.last_sync_error = None;
    ed.remote_conflict = Some(detail_clone);
    assert!(!app.try_open_page("page-sibling", &mut ui, true));
    assert_eq!(app.active_page_id.as_deref(), Some("page-root"));
    assert!(ui.inline_editor.as_ref().unwrap().confirm_overwrite);
}

#[test]
fn regression_p0_cross_workspace_events_dropped_and_truncated_db_poll_preserves_loaded_rows() {
    let (mut app, mut ui) = make_test_app_and_ui();
    app.workspace_id = "ws-B".into();
    app.expected_workspace_id = Some("ws-B".into());
    app.active_page_id = None;
    app.active_page_detail = None;
    app.databases.clear();

    // Late PageDetailUpdated from ws-A is dropped.
    let stale_page = NotionPageDetail {
        page: NotionPage {
            id: "page-from-a".into(),
            workspace_id: "ws-A".into(),
            title: "Stale A".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    assert!(!app.handle_core_event(&mut ui, crate::models::CoreEvent::PageDetailUpdated(Box::new(stale_page))));

    // Late DatabaseRowsUpdated from ws-A does NOT pollute self.databases.
    let stale_db = NotionDatabase {
        id: "db-from-a".into(),
        workspace_id: "ws-A".into(),
        title: "Stale DB A".into(),
        ..Default::default()
    };
    assert!(!app.handle_core_event(
        &mut ui,
        crate::models::CoreEvent::DatabaseRowsUpdated {
            database_id: "db-from-a".into(),
            database: Some(stale_db),
            rows: Vec::new(),
            rows_truncated: false,
        },
    ));
    assert!(app.databases.is_empty());

    // Truncated 250-row poll must NOT drop already-loaded rows 251..300.
    app.open_database("db-big", &mut ui, false);
    app.active_database_rows = (0..300)
        .map(|i| NotionPage {
            id: format!("r-{i}"),
            workspace_id: "ws-B".into(),
            title: format!("Row {i}"),
            ..Default::default()
        })
        .collect();
    let first_250: Vec<NotionPage> = app.active_database_rows.iter().take(250).cloned().collect();
    app.handle_core_event(
        &mut ui,
        crate::models::CoreEvent::DatabaseRowsUpdated {
            database_id: "db-big".into(),
            database: None,
            rows: first_250,
            rows_truncated: true,
        },
    );
    assert_eq!(
        app.active_database_rows.len(),
        300,
        "truncated background poll must preserve rows past 250"
    );
}

#[test]
fn regression_p1_right_click_targets_clicked_item_and_blocks_modal_stacking() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();

    // Find a SidebarRow hit target and right-click it while Canvas is focused.
    let sb_rect = ui
        .hitmap
        .region_of(HitTarget::SidebarRow(1))
        .expect("sidebar row 1 should exist");
    dispatch(
        Action::RightClick {
            col: sb_rect.x + 1,
            row: sb_rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.focus, Focus::Sidebar);
    assert_eq!(ui.sidebar_selected, 1);
    match ui.overlays.last() {
        Some(Overlay::ContextMenu(menu)) => {
            assert_eq!(menu.context, ContextTarget::Sidebar);
            assert!(menu.items.iter().any(|i| i.label.contains("Open Page")));
            assert!(menu.items.iter().any(|i| i.label.contains("Toggle Favorite")));
            assert!(menu.items.iter().any(|i| i.label.contains("Archive Page")));
        }
        other => panic!("expected Sidebar ContextMenu, got {other:?}"),
    }

    // Right-clicking again replaces existing ContextMenu (overlay count stays 1).
    let canvas_rect = ui
        .hitmap
        .region_of(HitTarget::Pane(Focus::Canvas))
        .expect("canvas pane rect");
    dispatch(
        Action::RightClick {
            col: canvas_rect.x + 2,
            row: canvas_rect.y + 2,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.overlays.len(), 1);
    assert!(matches!(
        ui.overlays.last(),
        Some(Overlay::ContextMenu(m)) if m.context == ContextTarget::PageCanvas
    ));

    // When a non-ContextMenu modal (Settings) is open, RightClick must NOT stack a ContextMenu.
    ui.overlays.clear();
    dispatch(Action::OpenOverlay(OverlayKind::Settings), &mut app, &mut ui);
    dispatch(
        Action::RightClick {
            col: sb_rect.x + 1,
            row: sb_rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.overlays.len(), 1);
    assert!(matches!(ui.overlays.last(), Some(Overlay::Settings(_))));
}

#[test]
fn regression_p1_toggleable_heading_hit_target_narrow_guards_html_toggle_and_seq_guards() {
    let (mut app, mut ui) = make_test_app_and_ui();
    app.active_page_detail.as_mut().unwrap().blocks = vec![NotionBlock {
        id: "blk-th1".into(),
        page_id: "page-root".into(),
        kind: NotionBlockKind::Heading1 {
            rich_text: vec![RichTextSpan::plain("Toggle Heading")],
            is_toggleable: true,
            color: NotionColor::Default,
        },
        children: vec![NotionBlock {
            id: "blk-inner".into(),
            page_id: "page-root".into(),
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain("Hidden child")],
                color: NotionColor::Default,
            },
            ..Default::default()
        }],
        ..Default::default()
    }];
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    assert!(
        ui.hitmap.region_of(HitTarget::ToggleBlock(0)).is_some(),
        "toggleable heading must register HitTarget::ToggleBlock"
    );

    // Comments drawer refuses to open when canvas width < 72.
    ui.hitmap.clear();
    ui.term_width = 60;
    ui.focus = Focus::Canvas;
    dispatch(Action::ToggleCommentsDrawer, &mut app, &mut ui);
    assert!(!ui.comments_drawer_open);
    assert_ne!(ui.focus, Focus::Comments);

    // Narrow pills update both narrow_pane and focus.
    dispatch(
        Action::MouseClick {
            target: HitTarget::Button(ButtonId::NarrowSidebar),
            col: 1,
            row: 1,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.narrow_pane, NarrowPane::Sidebar);
    assert_eq!(ui.focus, Focus::Sidebar);

    // HtmlPreviewOverlay [m: Rendered] button toggles raw mode even while editing <input>.
    let mut html_ov = crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(
        "Form",
        "<input type=\"text\" value=\"hi\">".into(),
    );
    html_ov.session.focus_next_control();
    let _ = html_ov.session.activate_focused();
    assert!(html_ov.session.editing_input.is_some());
    ui.overlays.push(Overlay::HtmlPreview(Box::new(html_ov)));
    dispatch(
        Action::MouseClick {
            target: HitTarget::Button(ButtonId::HtmlToggleMode),
            col: 5,
            row: 0,
        },
        &mut app,
        &mut ui,
    );
    match ui.overlays.last() {
        Some(Overlay::HtmlPreview(ov)) => {
            assert!(ov.session.raw_mode, "button click must toggle raw mode instead of typing 'm'");
            assert!(ov.session.editing_input.is_none());
        }
        other => panic!("expected HtmlPreview, got {other:?}"),
    }
    ui.overlays.clear();

    // Monotonic preview sequence guard drops stale HtmlPreviewLoaded and ImagePreviewFailed.
    let stale_seq = ui.bump_preview_request_seq();
    let _newer_seq = ui.bump_preview_request_seq();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::HtmlPreviewLoaded {
            seq: stale_seq,
            title: "Stale".into(),
            html: "<p>stale</p>".into(),
            mime: None,
        },
    );
    assert!(ui.overlays.is_empty(), "stale HtmlPreviewLoaded must not hijack screen");

    // Monotonic PageRefreshed guard drops stale refresh and only finishes matching task label.
    let s1 = ui.next_page_refresh_seq("page-root");
    let _s2 = ui.next_page_refresh_seq("page-root");
    ui.tasks.begin("append");
    ui.tasks.begin("todo");
    let stale_detail = NotionPageDetail {
        page: NotionPage {
            id: "page-root".into(),
            title: "STALE TITLE".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    app.handle_ui_msg(
        &mut ui,
        UiMsg::PageRefreshed {
            detail: Box::new(stale_detail),
            seq: s1,
            task_label: "todo",
        },
    );
    assert_ne!(
        app.active_page_detail.as_ref().unwrap().page.title,
        "STALE TITLE"
    );
    assert_eq!(
        ui.tasks.first_running().map(|t| t.label.as_str()),
        Some("append"),
        "PageRefreshed for 'todo' must not finish 'append' task"
    );

    // PageArchived updates active_database_rows, active_page_detail, and closes inline_editor.
    app.open_inline_editor(&mut ui);
    app.active_database_rows.push(NotionPage {
        id: "page-root".into(),
        title: "Root Page".into(),
        ..Default::default()
    });
    app.handle_ui_msg(&mut ui, UiMsg::PageArchived("page-root".into()));
    assert!(ui.inline_editor.is_none());
    assert!(app.active_page_detail.as_ref().unwrap().page.archived);
    assert!(app.active_database_rows.iter().all(|r| r.id != "page-root" || r.archived));
}

#[tokio::test]
async fn test_markdown_file_block_preview_and_raw_toggle() {
    use axum::{http::header, routing::get, Router};
    use bitefoundry_html::SourceKind;
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::net::TcpListener;

    let md_body = "# Title\n\n**bold** | [link](https://example.com)\n";
    let router = Router::new().route(
        "/files/README.md",
        get(move || async move {
            (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                md_body,
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let md_url = format!("http://{addr}/files/README.md");
    let (mut app, mut ui) = make_test_app_and_ui();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    app.ui_tx = ui_tx;
    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-md".into());
    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-md".into(),
            title: "Markdown Attachment Page".into(),
            ..Default::default()
        },
        blocks: vec![NotionBlock {
            id: "blk-md-1".into(),
            page_id: "page-md".into(),
            kind: NotionBlockKind::File {
                url: md_url.clone(),
                name: Some("README.md".into()),
                caption: Vec::new(),
            },
            ..Default::default()
        }],
        comments: Vec::new(),
        comments_error: None,
        markdown: String::new(),
        media: vec![
            NotionMediaRef {
                block_id: "blk-md-1".into(),
                url: md_url.clone(),
                display_name: "README.md".into(),
                mime_type: "text/markdown".into(),
                is_image: false,
            },
            NotionMediaRef {
                block_id: "blk-md-2".into(),
                url: md_url.clone(),
                display_name: "README.md".into(),
                mime_type: "application/octet-stream".into(),
                is_image: false,
            },
        ],
    });

    // 1. Canvas File block chip and Attachments overlay both display `[MD]`
    let area = Rect::new(0, 0, 100, 28);
    let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
    terminal
        .draw(|frame| {
            ui::draw(frame, &mut app, &mut ui);
        })
        .unwrap();
    let canvas_text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        canvas_text.contains("[MD] README.md"),
        "canvas file block should show [MD] badge"
    );

    let mut att = crate::ui::overlays::attachments::AttachmentsState::default();
    terminal
        .draw(|frame| {
            let mut hm = crate::ui::HitMap::new();
            att.draw(frame, area, &app, &ui, &mut hm);
        })
        .unwrap();
    let att_text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        att_text.contains("[MD] README.md"),
        "attachments tray should show [MD] badge"
    );

    // 2. Pressing `v` on both `text/markdown` and `application/octet-stream` `.md` items dispatches FetchHtmlPreview
    for idx in [0usize, 1usize] {
        att.selected = idx;
        match att.handle(&Action::Input('v'), &app) {
            OverlayOutcome::CloseAndDispatch(Action::FetchHtmlPreview { title, url, .. }) => {
                assert_eq!(title, "README.md");
                assert_eq!(url, md_url);
            }
            other => panic!("expected FetchHtmlPreview for README.md idx {idx}, got {other:?}"),
        }
    }

    // 3. Clicking canvas MediaChip dispatches FetchHtmlPreview and opens Overlay::HtmlPreview with SourceKind::Markdown
    let chip_rect = ui
        .hitmap
        .region_of(HitTarget::MediaChip(0))
        .expect("MediaChip(0) should be in hitmap");
    dispatch(
        Action::MouseClick {
            target: HitTarget::MediaChip(0),
            col: chip_rect.x + 2,
            row: chip_rect.y,
        },
        &mut app,
        &mut ui,
    );

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("timed out waiting for HtmlPreviewLoaded")
        .expect("ui_rx closed");
    app.handle_ui_msg(&mut ui, msg);

    let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() else {
        panic!("expected Overlay::HtmlPreview for README.md");
    };
    assert_eq!(ov.session.source_kind(), SourceKind::Markdown);
    assert!(!ov.session.js_active, "Markdown preview must not spawn QuickJS");

    // Rendered mode: Title, bold, link shown; 0 raw `**`
    let html_theme =
        crate::ui::overlays::html_preview::HtmlPreviewOverlay::html_theme_from(&ui.theme);
    let rendered_text: String = ov
        .session
        .render(80, &html_theme, false)
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(rendered_text.contains("Title"));
    assert!(rendered_text.contains("bold"));
    assert!(rendered_text.contains("link"));
    assert!(
        !rendered_text.contains("**"),
        "rendered Markdown preview must not leak raw **: {rendered_text}"
    );

    // `r` toggles raw mode and shows raw `# Title`
    assert_eq!(ov.handle(&Action::Input('r')), OverlayOutcome::Handled);
    assert!(ov.session.raw_mode);
    let raw_text: String = ov
        .session
        .render(80, &html_theme, false)
        .lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(
        raw_text.contains("# Title"),
        "raw mode must show raw `# Title`: {raw_text}"
    );

    // `o` dispatches ServeActiveHtml and session.serve_html() produces standalone HTML
    assert_eq!(
        ov.handle(&Action::Input('o')),
        OverlayOutcome::Dispatch(Action::ServeActiveHtml)
    );
    let served_html = ov.session.serve_html();
    assert!(served_html.contains("<!DOCTYPE html>"));
    assert!(served_html.contains("<strong>bold</strong>"));

    server_handle.abort();
}

#[test]
fn test_equation_block_inline_math_and_comment_rich_text_rendering() {
    use crate::models::{RichTextAnnotations, RichTextKind};
    use ratatui::{backend::TestBackend, style::Modifier, Terminal};

    let (mut app, mut ui) = make_test_app_and_ui();
    ui.theme = crate::ui::theme::Theme::clean_dark();
    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-math".into());
    ui.comments_drawer_open = true;

    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-math".into(),
            title: "Math & Comments".into(),
            ..Default::default()
        },
        blocks: vec![
            NotionBlock {
                id: "blk-eq".into(),
                page_id: "page-math".into(),
                kind: NotionBlockKind::Equation {
                    expression: "E = mc^2\n+ p^2 c^2".into(),
                },
                ..Default::default()
            },
            NotionBlock {
                id: "blk-p".into(),
                page_id: "page-math".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![
                        RichTextSpan::plain("Inline "),
                        RichTextSpan {
                            plain_text: "a+b".into(),
                            kind: RichTextKind::Equation {
                                expression: "a+b".into(),
                            },
                            ..Default::default()
                        },
                    ],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
        ],
        comments: vec![NotionComment {
            id: "c1".into(),
            page_id: "page-math".into(),
            discussion_id: "d1".into(),
            author: "Alice".into(),
            body: "BoldWord and CodeWord".into(),
            rich_text: vec![
                RichTextSpan {
                    plain_text: "BoldWord".into(),
                    annotations: RichTextAnnotations {
                        bold: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                RichTextSpan::plain(" and "),
                RichTextSpan {
                    plain_text: "CodeWord".into(),
                    annotations: RichTextAnnotations {
                        code: true,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
            created_time: "2026-01-01T12:00:00Z".into(),
        }],
        comments_error: None,
        markdown: String::new(),
        media: Vec::new(),
    });

    let area = Rect::new(0, 0, 120, 30);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|frame| {
            ui::draw(frame, &mut app, &mut ui);
        })
        .unwrap();

    let buf = terminal.backend().buffer();
    let find_word_cell = |word: &str| {
        let first_ch = word.chars().next().unwrap().to_string();
        for y in 0..area.height {
            let row: String = (0..area.width)
                .map(|x| buf[(x, y)].symbol())
                .collect();
            if let Some(byte_idx) = row.find(word) {
                let col = row[..byte_idx].chars().count() as u16;
                if buf[(col, y)].symbol() == first_ch {
                    return Some(&buf[(col, y)]);
                }
            }
        }
        None
    };

    // Block equation (line 1 & line 2) and inline equation carry distinct ITALIC math styling
    let eq_cell = find_word_cell("E = mc^2").expect("block equation line 1 rendered");
    assert!(
        eq_cell.modifier.contains(Modifier::ITALIC),
        "block equation must have distinct italic math styling"
    );
    let eq_line2 = find_word_cell("+ p^2 c^2").expect("block equation line 2 rendered");
    assert!(eq_line2.modifier.contains(Modifier::ITALIC));

    let inline_math_cell = find_word_cell("a+b").expect("inline equation rendered");
    assert!(
        inline_math_cell.modifier.contains(Modifier::ITALIC),
        "inline equation must have distinct italic math styling"
    );
    assert_ne!(
        inline_math_cell.fg,
        ui.theme.code_fg,
        "math styling should be visually distinct from plain code_fg"
    );

    // Comment rich-text annotations respected in drawer
    let bold_comment = find_word_cell("BoldWord").expect("bold comment span rendered");
    assert!(
        bold_comment.modifier.contains(Modifier::BOLD),
        "comment rich_text bold annotation must be applied"
    );
    let code_comment = find_word_cell("CodeWord").expect("code comment span rendered");
    assert_eq!(
        code_comment.fg,
        ui.theme.code_fg,
        "comment rich_text code annotation must apply code_fg"
    );
}

