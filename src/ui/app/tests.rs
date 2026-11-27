use std::collections::HashSet;

use tokio::sync::mpsc;

use super::{views::flatten_visible_blocks, *};
use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{
        NotionBlock, NotionBlockKind, NotionColor, NotionPage, NotionPageDetail, NotionPageSummary,
        RichTextSpan,
    },
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::hitmap::HitMap,
};

fn test_app_and_ui() -> (AppState, UiState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::open(&dir.path().join("ui_app.db")).unwrap();
    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_ui_test");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
    let app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    let ui = UiState::new();
    (app, ui, dir)
}

#[test]
fn page_created_renders_preview_immediately_without_persisting_optimistic_ids() {
    let (mut app, mut ui, _dir) = test_app_and_ui();
    let page = NotionPage {
        id: "page-created-1".into(),
        workspace_id: app.workspace_id.clone(),
        parent_type: "workspace".into(),
        title: "Launch Plan".into(),
        ..Default::default()
    };
    app.storage.upsert_page(&page).unwrap();

    let preview = build_created_page_preview(page.clone(), "# Kickoff\n\nShip v1");
    app.handle_ui_msg(
        &mut ui,
        UiMsg::PageCreated {
            page: page.clone(),
            preview_detail: Some(Box::new(preview)),
        },
    );

    let active = app
        .active_page_detail
        .as_ref()
        .expect("active_page_detail should be populated immediately");
    assert_eq!(active.page.id, "page-created-1");
    assert_eq!(active.blocks.len(), 2);
    assert!(active.blocks.iter().all(|b| b.id.starts_with("__optimistic_create_")));
    assert!(active.markdown.contains("Kickoff"));
    assert!(active.markdown.contains("Ship v1"));

    // SQLite must NEVER contain synthetic __optimistic_* blocks.
    assert!(
        app.storage
            .load_cached_children(&app.workspace_id, "page-created-1")
            .unwrap()
            .is_none(),
        "optimistic preview must stay in memory only"
    );
}

#[test]
fn optimistic_append_renders_immediately_and_reverts_on_failure() {
    let (mut app, mut ui, _dir) = test_app_and_ui();
    let page = NotionPage {
        id: "page-append-1".into(),
        workspace_id: app.workspace_id.clone(),
        parent_type: "workspace".into(),
        title: "Notes".into(),
        ..Default::default()
    };
    let existing = NotionBlock {
        id: "blk-real-1".into(),
        page_id: "page-append-1".into(),
        parent_block_id: None,
        block_type: "paragraph".into(),
        has_children: false,
        archived: false,
        kind: NotionBlockKind::Paragraph {
            rich_text: vec![crate::models::RichTextSpan::plain("Original")],
            color: NotionColor::Default,
        },
        children: Vec::new(),
    };
    app.storage
        .save_page_detail(&NotionPageDetail {
            page: page.clone(),
            blocks: vec![existing],
            comments: Vec::new(),
            comments_error: None,
            markdown: "Original".into(),
            media: Vec::new(),
        })
        .unwrap();
    app.pages.push(NotionPageSummary::from(&page));
    app.open_page("page-append-1", &mut ui, false);

    assert!(app.apply_optimistic_append_local("page-append-1", "/todo Ship now", &mut ui));
    let after_opt = app.active_page_detail.as_ref().unwrap();
    assert_eq!(after_opt.blocks.len(), 2);
    assert!(after_opt.blocks[1].id.starts_with("__optimistic_append_"));
    assert!(after_opt.markdown.contains("Ship now"));

    // SQLite still holds only the single server-confirmed block.
    let cached = app
        .storage
        .load_page_detail(&app.workspace_id, "page-append-1")
        .unwrap()
        .unwrap();
    assert_eq!(cached.blocks.len(), 1);

    // Failure reverts in-memory canvas back to SQLite's authoritative block list.
    app.handle_ui_msg(
        &mut ui,
        UiMsg::AppendFailed {
            page_id: "page-append-1".into(),
            error: "network timeout".into(),
        },
    );
    let reverted = app.active_page_detail.as_ref().unwrap();
    assert_eq!(reverted.blocks.len(), 1);
    assert_eq!(reverted.blocks[0].id, "blk-real-1");
}

fn sidebar_page(id: &str, parent_type: &str, parent_id: Option<&str>) -> NotionPageSummary {
    NotionPageSummary {
        id: id.into(),
        title: id.into(),
        parent_type: parent_type.into(),
        parent_id: parent_id.map(str::to_string),
        ..Default::default()
    }
}

fn sidebar_index(items: &[SidebarItem], id: &str) -> Option<usize> {
    items.iter().position(|it| it.id == id && !it.is_section_header)
}

#[test]
fn sidebar_lists_depth_seven_page_with_capped_indent_and_clickable_toggle() {
    use crate::ui::action::{dispatch, Action};
    use crate::ui::hitmap::HitTarget;

    let (mut app, mut ui, _dir) = test_app_and_ui();
    // page-depth-0 (workspace) -> page-depth-1 -> ... -> page-depth-8
    app.pages = (0..=8)
        .map(|d| {
            let parent = (d > 0).then(|| format!("page-depth-{}", d - 1));
            let parent_type = if d == 0 { "workspace" } else { "page_id" };
            sidebar_page(&format!("page-depth-{d}"), parent_type, parent.as_deref())
        })
        .collect();

    let items = app.build_sidebar_items(&ui);
    for d in 0..=8 {
        let idx = sidebar_index(&items, &format!("page-depth-{d}"))
            .unwrap_or_else(|| panic!("depth-{d} page must be listed"));
        assert_eq!(items[idx].depth, d);
    }
    let d7 = sidebar_index(&items, "page-depth-7").unwrap();
    assert!(items[d7].collapsible && !items[d7].collapsed);

    // Indentation stops growing at the cap, and the arrow hit rect sits on the drawn arrow.
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(48, 20)).unwrap();
    let mut hitmap = HitMap::new();
    terminal
        .draw(|f| crate::ui::nav_pane::draw(f, f.area(), &app, &ui, &mut hitmap))
        .unwrap();
    let arrow = hitmap
        .region_of(HitTarget::SidebarToggle(d7))
        .expect("depth-7 arrow must be clickable");
    let capped = (crate::ui::nav_pane::SIDEBAR_MAX_INDENT_LEVEL * 2) as u16;
    assert_eq!(arrow.x, capped);
    let buf = terminal.backend().buffer();
    assert_eq!(buf[(arrow.x + 1, arrow.y)].symbol(), "▾");
    let d8_row = hitmap
        .region_of(HitTarget::SidebarRow(sidebar_index(&items, "page-depth-8").unwrap()))
        .unwrap();
    assert_eq!(buf[(capped + 1, d8_row.y)].symbol(), "└");

    // Clicking the depth-7 arrow collapses its subtree; clicking again expands it.
    let click = |app: &mut AppState, ui: &mut UiState| {
        dispatch(
            Action::MouseClick {
                target: HitTarget::SidebarToggle(d7),
                col: arrow.x + 1,
                row: arrow.y,
            },
            app,
            ui,
        );
    };
    click(&mut app, &mut ui);
    assert!(ui.collapsed_nodes.contains("page-depth-7"));
    let collapsed = app.build_sidebar_items(&ui);
    assert!(collapsed[sidebar_index(&collapsed, "page-depth-7").unwrap()].collapsed);
    assert!(sidebar_index(&collapsed, "page-depth-8").is_none());

    click(&mut app, &mut ui);
    assert!(!ui.collapsed_nodes.contains("page-depth-7"));
    let expanded = app.build_sidebar_items(&ui);
    assert_eq!(expanded[sidebar_index(&expanded, "page-depth-8").unwrap()].depth, 8);
}

#[test]
fn sidebar_tree_depth_cap_still_breaks_parent_cycles() {
    let (mut app, ui, _dir) = test_app_and_ui();
    // A 30-level chain is listed up to the cap; a parent cycle must not recurse forever.
    app.pages = (0..30)
        .map(|d| {
            let parent = (d > 0).then(|| format!("deep-{}", d - 1));
            let parent_type = if d == 0 { "workspace" } else { "page_id" };
            sidebar_page(&format!("deep-{d}"), parent_type, parent.as_deref())
        })
        .chain([
            sidebar_page("cycle-a", "page_id", Some("cycle-b")),
            sidebar_page("cycle-b", "page_id", Some("cycle-a")),
        ])
        .collect();

    let items = app.build_sidebar_items(&ui);
    let last = sidebar_index(&items, &format!("deep-{SIDEBAR_MAX_TREE_DEPTH}")).unwrap();
    assert_eq!(items[last].depth, SIDEBAR_MAX_TREE_DEPTH);
    assert!(items[last].collapsible && items[last].collapsed);
    assert!(sidebar_index(&items, &format!("deep-{}", SIDEBAR_MAX_TREE_DEPTH + 1)).is_none());
    assert!(sidebar_index(&items, "cycle-a").is_some());
    assert!(sidebar_index(&items, "cycle-b").is_some());
}

#[test]
fn sidebar_lists_subpages_nested_inside_database_rows() {
    let (mut app, ui, _dir) = test_app_and_ui();
    app.pages = vec![
        sidebar_page("page-root", "workspace", None),
        sidebar_page("db-row-1", "database_id", Some("db-1")),
        sidebar_page("page-in-row", "page_id", Some("db-row-1")),
        sidebar_page("page-in-row-child", "page_id", Some("page-in-row")),
        sidebar_page("page-in-row-block", "block_id", Some("blk-in-row")),
    ];
    app.block_owner_pages
        .insert("blk-in-row".into(), "db-row-1".into());

    let items = app.build_sidebar_items(&ui);
    let workspace = items
        .iter()
        .position(|it| it.id == "__sec_workspace")
        .expect("WORKSPACE section");
    let pos = |id: &str| {
        sidebar_index(&items, id).unwrap_or_else(|| panic!("{id} must not be orphaned"))
    };
    assert!(sidebar_index(&items, "db-row-1").is_none(), "database rows stay out of WORKSPACE");
    for id in ["page-in-row", "page-in-row-block"] {
        assert!(pos(id) > workspace);
        assert_eq!(items[pos(id)].depth, 0);
    }
    assert!(items[pos("page-in-row")].collapsible);
    assert_eq!(items[pos("page-in-row-child")].depth, 1);
    assert_eq!(pos("page-in-row-child"), pos("page-in-row") + 1);
}

fn para(id: &str, text: &str) -> NotionBlock {
    NotionBlock {
        id: id.into(),
        block_type: "paragraph".into(),
        kind: NotionBlockKind::Paragraph {
            rich_text: vec![RichTextSpan::plain(text)],
            color: NotionColor::Default,
        },
        ..Default::default()
    }
}

fn wrapper(id: &str, block_type: &str, kind: NotionBlockKind, children: Vec<NotionBlock>) -> NotionBlock {
    NotionBlock {
        id: id.into(),
        block_type: block_type.into(),
        has_children: true,
        kind,
        children,
        ..Default::default()
    }
}

#[test]
fn flatten_visible_blocks_skips_zero_line_container_wrappers() {
    let blocks = vec![
        wrapper(
            "cl",
            "column_list",
            NotionBlockKind::ColumnList,
            vec![
                wrapper("col-1", "column", NotionBlockKind::Column, vec![para("a", "A")]),
                wrapper("col-2", "column", NotionBlockKind::Column, vec![para("b", "B")]),
            ],
        ),
        wrapper(
            "sync",
            "synced_block",
            NotionBlockKind::SyncedBlock {
                synced_from_block_id: None,
            },
            vec![para("c", "C")],
        ),
        para("d", "D"),
    ];
    let mut out = Vec::new();
    flatten_visible_blocks(&blocks, 0, &HashSet::new(), &mut out);
    let stops: Vec<(&str, usize)> =
        out.iter().map(|v| (v.block.id.as_str(), v.depth)).collect();
    assert_eq!(stops, vec![("a", 2), ("b", 2), ("c", 1), ("d", 0)]);
}

#[test]
fn hydrated_toggle_children_attach_to_detail_and_confirmed_baseline() {
    let (mut app, mut ui, _dir) = test_app_and_ui();
    let deep_toggle = NotionBlock {
        id: "blk-deep-toggle".into(),
        block_type: "toggle".into(),
        has_children: true,
        kind: NotionBlockKind::Toggle {
            rich_text: vec![RichTextSpan::plain("Deep")],
            color: NotionColor::Default,
        },
        ..Default::default()
    };
    let outer = wrapper(
        "blk-outer",
        "toggle",
        NotionBlockKind::Toggle {
            rich_text: vec![RichTextSpan::plain("Outer")],
            color: NotionColor::Default,
        },
        vec![deep_toggle],
    );
    let blocks = vec![outer];
    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-hydrate".into(),
            ..Default::default()
        },
        markdown: crate::parser::blocks_to_markdown(&blocks),
        blocks: blocks.clone(),
        comments: Vec::new(),
        comments_error: None,
        media: Vec::new(),
    });
    ui.page_confirmed_blocks
        .insert("page-hydrate".into(), blocks);
    ui.hydrating_blocks.insert("blk-deep-toggle".into());
    ui.expanded_toggles.insert("blk-outer".into());
    ui.expanded_toggles.insert("blk-deep-toggle".into());

    app.handle_ui_msg(
        &mut ui,
        UiMsg::BlockChildrenHydrated {
            page_id: "page-hydrate".into(),
            block_id: "blk-deep-toggle".into(),
            children: vec![para("blk-lazy", "Lazy child")],
        },
    );

    assert!(ui.hydrating_blocks.is_empty());
    let detail = app.active_page_detail.as_ref().unwrap();
    assert_eq!(detail.blocks[0].children[0].children[0].id, "blk-lazy");
    assert!(detail.markdown.contains("Lazy child"));
    let confirmed = &ui.page_confirmed_blocks["page-hydrate"];
    assert_eq!(confirmed[0].children[0].children[0].id, "blk-lazy");
    let visible: Vec<String> = app
        .visible_blocks(&ui)
        .into_iter()
        .map(|v| v.block.id)
        .collect();
    assert_eq!(visible, vec!["blk-outer", "blk-deep-toggle", "blk-lazy"]);
}
