use ratatui::{backend::TestBackend, Terminal};

use super::make_test_app_and_ui;
use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{NotionBlock, NotionBlockKind, NotionColor, NotionMediaRef, NotionPage, NotionPageDetail, RichTextSpan},
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::{
        self,
        action::{dispatch, Action},
        app::{AppState, BlockCursorInit, CanvasView, Focus, UiState},
        hitmap::{ButtonId, HitTarget},
    },
};

#[test]
fn test_block_live_direct_click_cursor_placement_korean_and_controls_preserved() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use unicode_width::UnicodeWidthStr;

    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-root".into());
    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-root".into(),
            title: "Live Flow Page".into(),
            ..Default::default()
        },
        blocks: vec![
            NotionBlock {
                id: "b-h1".into(),
                page_id: "page-root".into(),
                block_type: "heading_1".into(),
                kind: NotionBlockKind::Heading1 {
                    rich_text: vec![RichTextSpan::plain("Section Header")],
                    is_toggleable: false,
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-para".into(),
                page_id: "page-root".into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Hello 한글 세계")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-todo".into(),
                page_id: "page-root".into(),
                block_type: "to_do".into(),
                kind: NotionBlockKind::ToDo {
                    rich_text: vec![RichTextSpan::plain("Ship live editor")],
                    checked: false,
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-toggle".into(),
                page_id: "page-root".into(),
                block_type: "toggle".into(),
                kind: NotionBlockKind::Toggle {
                    rich_text: vec![RichTextSpan::plain("Details Toggle")],
                    color: NotionColor::Default,
                },
                has_children: true,
                children: vec![NotionBlock {
                    id: "b-toggle-child".into(),
                    page_id: "page-root".into(),
                    parent_block_id: Some("b-toggle".into()),
                    block_type: "paragraph".into(),
                    kind: NotionBlockKind::Paragraph {
                        rich_text: vec![RichTextSpan::plain("Nested toggle body")],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                }],
                ..Default::default()
            },
            NotionBlock {
                id: "b-link".into(),
                page_id: "page-root".into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![
                        RichTextSpan::plain("Visit "),
                        RichTextSpan {
                            plain_text: "NotionDocs".into(),
                            href: Some("https://developers.notion.com".into()),
                            ..Default::default()
                        },
                    ],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-child".into(),
                page_id: "page-root".into(),
                block_type: "child_page".into(),
                kind: NotionBlockKind::ChildPage {
                    title: "Subpage Card".into(),
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-img".into(),
                page_id: "page-root".into(),
                block_type: "image".into(),
                kind: NotionBlockKind::Image {
                    url: "https://example.com/1.png".into(),
                    caption: vec![RichTextSpan::plain("Hero Img")],
                    is_hosted: false,
                },
                ..Default::default()
            },
        ],
        comments: Vec::new(),
        comments_error: None,
        markdown: String::new(),
        media: vec![NotionMediaRef {
            block_id: "b-img".into(),
            url: "https://example.com/1.png".into(),
            display_name: "Hero Img".into(),
            mime_type: "image/png".into(),
            is_image: true,
        }],
    });

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal.draw(|f| { ui::draw(f, &mut app, &mut ui); }).unwrap();

    // 1. Direct click on Paragraph ("Hello 한글 세계") at column of '글' (width("Hello 한") = 8)
    let para_lay = ui
        .canvas_block_layouts
        .iter()
        .find(|l| l.block_id == "b-para")
        .cloned()
        .expect("paragraph layout recorded");
    let click_col = para_lay.first_text_x + "Hello 한".width() as u16;
    let click_row = para_lay.first_screen_row;
    let hit = ui.hitmap.hit(click_row, click_col).expect("hit paragraph");
    assert_eq!(hit, HitTarget::CanvasBlock(1));
    dispatch(
        Action::MouseClick {
            target: hit,
            col: click_col,
            row: click_row,
        },
        &mut app,
        &mut ui,
    );

    let bed = ui
        .block_editor
        .as_ref()
        .expect("clicking paragraph must open block_editor directly");
    assert_eq!(bed.block_id, "b-para");
    assert_eq!(bed.composer.cursor(), "Hello 한".len());
    assert!(ui.inline_editor.is_none(), "must stay in block-live mode, not full-page mode");

    // Re-render and verify hardware cursor matches Korean wide-char width AND other blocks stay rendered!
    terminal.draw(|f| { ui::draw(f, &mut app, &mut ui); }).unwrap();
    assert_eq!(ui.cursor_pos, Some((click_col, click_row)));
    let screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen.contains("Section Header"), "heading must stay rendered");
    assert!(screen.contains("Ship live editor"), "todo must stay rendered");
    assert!(screen.contains("Details Toggle"), "toggle must stay rendered");
    assert!(screen.contains("Subpage Card"), "child page card must stay rendered");
    assert!(screen.contains("Hero Img"), "image chip must stay rendered");

    // 2. Clicking To-Do checkbox `[ ] ` toggles checkmark WITHOUT opening block editor on To-Do
    let todo_lay = ui
        .canvas_block_layouts
        .iter()
        .find(|l| l.block_id == "b-todo")
        .cloned()
        .expect("todo layout recorded");
    let cb_col = todo_lay.first_text_x.saturating_sub(2);
    let cb_row = todo_lay.first_screen_row;
    let cb_hit = ui.hitmap.hit(cb_row, cb_col).expect("hit todo checkbox");
    assert_eq!(cb_hit, HitTarget::TodoToggle(2));
    dispatch(
        Action::MouseClick {
            target: cb_hit,
            col: cb_col,
            row: cb_row,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.block_editor.is_none(), "checkbox click should toggle, not edit");
    assert!(matches!(
        app.active_page_detail.as_ref().unwrap().blocks[2].kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));

    // Clicking To-Do TEXT opens inline block editor on the To-Do block at clicked col!
    terminal.draw(|f| { ui::draw(f, &mut app, &mut ui); }).unwrap();
    let todo_text_col = todo_lay.first_text_x + 5;
    dispatch(
        Action::MouseClick {
            target: HitTarget::TodoToggle(2),
            col: todo_text_col,
            row: todo_lay.first_screen_row,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.block_editor.as_ref().map(|b| b.block_id.as_str()), Some("b-todo"));
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.cursor(), 5);

    // 3. Clicking Toggle triangle `▸ ` expands toggle (revealing nested child)
    let tog_lay = ui
        .canvas_block_layouts
        .iter()
        .find(|l| l.block_id == "b-toggle")
        .cloned()
        .expect("toggle layout recorded");
    let tri_col = tog_lay.first_text_x.saturating_sub(1);
    dispatch(
        Action::MouseClick {
            target: HitTarget::ToggleBlock(3),
            col: tri_col,
            row: tog_lay.first_screen_row,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.block_editor.is_none());
    assert!(ui.expanded_toggles.contains("b-toggle"));

    // 4. Clicking inline link opens URL instead of editing block
    terminal.draw(|f| { ui::draw(f, &mut app, &mut ui); }).unwrap();
    let link_hit = ui
        .canvas_link_hits
        .iter()
        .find(|h| h.url == "https://developers.notion.com")
        .cloned()
        .expect("inline link hit recorded");
    let link_target = ui
        .hitmap
        .hit(link_hit.rect.y, link_hit.rect.x)
        .expect("hit target on link");
    dispatch(
        Action::MouseClick {
            target: link_target,
            col: link_hit.rect.x,
            row: link_hit.rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.block_editor.is_none());
    assert!(
        ui.toasts
            .latest()
            .is_some_and(|t| t.message.contains("https://developers.notion.com")),
        "inline link click must open URL"
    );

    // 5. Pressing Enter and `i` on an editable block enters block editor; Esc exits
    ui.selected_block = 0; // Heading1
    let act_enter = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    dispatch(act_enter, &mut app, &mut ui);
    assert_eq!(ui.block_editor.as_ref().map(|b| b.block_id.as_str()), Some("b-h1"));

    dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
    assert!(ui.block_editor.is_none());

    let act_i = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_i, Action::OpenBlockEditor);
    dispatch(act_i, &mut app, &mut ui);
    assert_eq!(ui.block_editor.as_ref().map(|b| b.block_id.as_str()), Some("b-h1"));
}

#[test]
fn test_block_live_cross_block_up_down_navigation_with_optimistic_persistence() {
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-root".into());
    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-root".into(),
            title: "Nav Page".into(),
            ..Default::default()
        },
        blocks: vec![
            NotionBlock {
                id: "b-1".into(),
                page_id: "page-root".into(),
                block_type: "heading_1".into(),
                kind: NotionBlockKind::Heading1 {
                    rich_text: vec![RichTextSpan::plain("Top Heading")],
                    is_toggleable: false,
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-2".into(),
                page_id: "page-root".into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Line One\nLine Two")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-div".into(),
                page_id: "page-root".into(),
                block_type: "divider".into(),
                kind: NotionBlockKind::Divider,
                ..Default::default()
            },
            NotionBlock {
                id: "b-3".into(),
                page_id: "page-root".into(),
                block_type: "quote".into(),
                kind: NotionBlockKind::Quote {
                    rich_text: vec![RichTextSpan::plain("Bottom Quote")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
        ],
        comments: Vec::new(),
        comments_error: None,
        markdown: String::new(),
        media: Vec::new(),
    });

    // Open block 1 ("Line One\nLine Two") with cursor at end (Line Two)
    assert!(app.open_block_editor(&mut ui, 1, BlockCursorInit::End));
    assert_eq!(ui.block_editor.as_ref().unwrap().block_id, "b-2");

    // 1st Up moves inside b-2 from Line Two -> Line One (still editing b-2)
    dispatch(Action::CursorUp, &mut app, &mut ui);
    assert_eq!(ui.block_editor.as_ref().unwrap().block_id, "b-2");
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.cursor_line_col().0, 1);

    // Edit Line One before crossing up
    dispatch(Action::CursorEnd, &mut app, &mut ui);
    dispatch(Action::Input('!'), &mut app, &mut ui);
    assert_eq!(
        ui.block_editor.as_ref().unwrap().composer.text(),
        "Line One!\nLine Two"
    );

    // 2nd Up (already on first visual line) transfers to previous editable block b-1 with cursor at END!
    dispatch(Action::CursorUp, &mut app, &mut ui);
    let bed_top = ui.block_editor.as_ref().unwrap();
    assert_eq!(bed_top.block_id, "b-1");
    assert_eq!(bed_top.composer.cursor(), "Top Heading".len());
    assert_eq!(
        app.active_page_detail.as_ref().unwrap().blocks[1].plain_text(),
        "Line One!\nLine Two",
        "edits to b-2 must persist optimistically when navigating up"
    );

    // Down from single-line b-1 transfers to b-2 with cursor at START!
    dispatch(Action::CursorDown, &mut app, &mut ui);
    let bed_mid = ui.block_editor.as_ref().unwrap();
    assert_eq!(bed_mid.block_id, "b-2");
    assert_eq!(bed_mid.composer.cursor(), 0);

    // 1st Down inside b-2 moves Line One -> Line Two
    dispatch(Action::CursorDown, &mut app, &mut ui);
    assert_eq!(ui.block_editor.as_ref().unwrap().block_id, "b-2");
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.cursor_line_col().0, 2);

    // 2nd Down (already on last visual line) skips non-editable Divider and enters b-3 at START!
    dispatch(Action::CursorDown, &mut app, &mut ui);
    let bed_bot = ui.block_editor.as_ref().unwrap();
    assert_eq!(bed_bot.block_id, "b-3");
    assert_eq!(bed_bot.composer.cursor(), 0);
    assert_eq!(ui.selected_block, 3);
}

#[tokio::test]
async fn test_block_live_enter_splits_sibling_shift_enter_soft_newline_and_coalesced_sync() {
    use std::sync::{Arc, Mutex};
    use axum::{
        extract::Path as AxumPath,
        routing::{delete, get},
        Json, Router,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::net::TcpListener;

    let state_blocks: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![
        serde_json::json!({
            "id": "blk-1",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "HelloWorld" }] }
        }),
        serde_json::json!({
            "id": "blk-code",
            "type": "code",
            "has_children": false,
            "code": { "language": "rust", "rich_text": [{ "plain_text": "let x = 1;" }], "caption": [] }
        }),
    ]));
    let get_b = state_blocks.clone();
    let patch_b = state_blocks.clone();
    let append_b = state_blocks.clone();
    let del_b = state_blocks.clone();

    let router = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(serde_json::json!({
                    "object": "page", "id": id,
                    "parent": { "type": "workspace", "workspace": true },
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": "Split Page" }] } }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(move || {
                let b = get_b.clone();
                async move {
                    Json(serde_json::json!({
                        "object": "list", "results": b.lock().unwrap().clone(), "has_more": false
                    }))
                }
            })
            .patch(move |Json(body): Json<serde_json::Value>| {
                let b = append_b.clone();
                async move {
                    let after = body.get("after").and_then(|v| v.as_str()).map(str::to_string);
                    let children = body
                        .get("children")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut guard = b.lock().unwrap();
                    let insert_pos = after
                        .as_deref()
                        .and_then(|aid| guard.iter().position(|x| x["id"] == aid))
                        .map(|p| p + 1)
                        .unwrap_or(guard.len());
                    let mut created = Vec::new();
                    for (i, mut c) in children.into_iter().enumerate() {
                        c["id"] = serde_json::json!(format!("blk-srv-{}-{}", guard.len(), i));
                        c["has_children"] = serde_json::json!(false);
                        guard.insert(insert_pos + i, c.clone());
                        created.push(c);
                    }
                    Json(serde_json::json!({ "object": "list", "results": created }))
                }
            }),
        )
        .route(
            "/v1/blocks/{id}",
            delete(move |AxumPath(id): AxumPath<String>| {
                let b = del_b.clone();
                async move {
                    b.lock().unwrap().retain(|blk| blk["id"] != id);
                    Json(serde_json::json!({ "id": id, "archived": true }))
                }
            })
            .patch(move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                let b = patch_b.clone();
                async move {
                    let mut guard = b.lock().unwrap();
                    if let Some(existing) = guard.iter_mut().find(|blk| blk["id"] == id) {
                        let btype = existing["type"].as_str().unwrap().to_string();
                        if let Some(inner) = body.get(&btype) {
                            existing[&btype] = inner.clone();
                        }
                        return Json(existing.clone());
                    }
                    Json(serde_json::json!({ "id": id, "type": "paragraph", "paragraph": { "rich_text": [] } }))
                }
            }),
        )
        .route(
            "/v1/comments",
            get(|| async {
                Json(serde_json::json!({ "object": "list", "results": [], "has_more": false }))
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_split_test");
    let client = NotionClient::new(&cfg, auth.clone())
        .unwrap()
        .with_base_url(format!("http://{addr}/v1"));
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui = UiState::from_config(&cfg);
    ui.focus = Focus::Canvas;
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    let page = NotionPage {
        id: "55555555-2222-3333-4444-555555555555".into(),
        workspace_id: app.workspace_id.clone(),
        title: "Split Page".into(),
        ..Default::default()
    };
    app.storage.upsert_page(&page).unwrap();
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page.id.clone());
    app.active_page_detail = Some(NotionPageDetail {
        page: page.clone(),
        blocks: vec![
            NotionBlock {
                id: "blk-1".into(),
                page_id: page.id.clone(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("HelloWorld")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "blk-code".into(),
                page_id: page.id.clone(),
                block_type: "code".into(),
                kind: NotionBlockKind::Code {
                    rich_text: vec![RichTextSpan::plain("let x = 1;")],
                    language: "rust".into(),
                    caption: Vec::new(),
                },
                ..Default::default()
            },
        ],
        comments: Vec::new(),
        comments_error: None,
        markdown: "HelloWorld\n\n```rust\nlet x = 1;\n```".into(),
        media: Vec::new(),
    });

    // 1. Open Paragraph 0, place cursor between "Hello" and "World", press Enter -> splits in 0ms!
    assert!(app.open_block_editor(&mut ui, 0, BlockCursorInit::Start));
    ui.block_editor.as_mut().unwrap().composer.set_cursor(5);
    let act_enter = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_enter, Action::Newline);
    dispatch(act_enter, &mut app, &mut ui);

    {
        let blocks = &app.active_page_detail.as_ref().unwrap().blocks;
        assert_eq!(blocks.len(), 3, "Enter must insert a new sibling block optimistically");
        assert_eq!(blocks[0].plain_text(), "Hello");
        assert_eq!(blocks[1].plain_text(), "World");
        assert!(blocks[1].id.starts_with("__optimistic_live_"));
        let bed = ui.block_editor.as_ref().unwrap();
        assert_eq!(bed.block_id, blocks[1].id);
        assert_eq!(bed.composer.text(), "World");
        assert_eq!(bed.composer.cursor(), 0);
    }

    // 2. Shift+Enter inserts a soft newline `\n` inside the current block (no extra block created!)
    let act_shift_enter = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_shift_enter, Action::SoftNewline);
    dispatch(Action::CursorEnd, &mut app, &mut ui);
    dispatch(act_shift_enter, &mut app, &mut ui);
    dispatch(Action::Paste("Line2".into()), &mut app, &mut ui);
    assert_eq!(app.active_page_detail.as_ref().unwrap().blocks.len(), 3);
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.text(), "World\nLine2");

    // 3. Rapid edit & Enter on the optimistic block BEFORE server replies -> coalesces cleanly!
    dispatch(Action::Newline, &mut app, &mut ui);
    dispatch(Action::Paste("ThirdBlock".into()), &mut app, &mut ui);
    dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
    assert_eq!(app.active_page_detail.as_ref().unwrap().blocks.len(), 4);

    // Drain coalesced PageEdited messages until sync settles
    for _ in 0..4 {
        match tokio::time::timeout(std::time::Duration::from_secs(3), ui_rx.recv()).await {
            Ok(Some(msg)) => {
                app.handle_ui_msg(&mut ui, msg);
                if !ui.page_sync_in_flight.contains(&page.id)
                    && !ui.pending_page_sync.contains_key(&page.id)
                {
                    break;
                }
            }
            _ => break,
        }
    }
    let synced = app.active_page_detail.as_ref().unwrap();
    assert!(synced.markdown.contains("Hello"));
    assert!(synced.markdown.contains("World\nLine2"));
    assert!(synced.markdown.contains("ThirdBlock"));
    assert!(
        synced.blocks.iter().all(|b| !b.id.starts_with("__optimistic_")),
        "after coalesced sync completes, all blocks must have real server IDs"
    );

    // 4. Inside a Code block, plain Enter inserts soft `\n` (does NOT split into a new block!)
    let code_idx = app
        .visible_blocks(&ui)
        .iter()
        .position(|vb| matches!(vb.block.kind, NotionBlockKind::Code { .. }))
        .expect("code block present");
    let count_before = app.active_page_detail.as_ref().unwrap().blocks.len();
    assert!(app.open_block_editor(&mut ui, code_idx, BlockCursorInit::End));
    dispatch(Action::Newline, &mut app, &mut ui);
    dispatch(Action::Paste("let y = 2;".into()), &mut app, &mut ui);
    assert_eq!(
        app.active_page_detail.as_ref().unwrap().blocks.len(),
        count_before,
        "Enter inside Code block must insert newline, not create a sibling block"
    );
    assert_eq!(
        ui.block_editor.as_ref().unwrap().composer.text(),
        "let x = 1;\nlet y = 2;"
    );

    server_handle.abort();
}

#[test]
fn test_block_live_markdown_prefix_shortcuts_convert_paragraph_on_the_fly() {
    let cases: &[(&str, &str)] = &[
        ("# ", "heading_1"),
        ("## ", "heading_2"),
        ("### ", "heading_3"),
        ("- ", "bulleted_list_item"),
        ("* ", "bulleted_list_item"),
        ("[] ", "to_do"),
        ("[ ] ", "to_do"),
        ("> ", "quote"),
    ];

    for &(prefix, expected_type) in cases {
        let (mut app, mut ui) = make_test_app_and_ui();
        ui.focus = Focus::Canvas;
        app.active_view = CanvasView::Page;
        app.active_page_id = Some("page-root".into());
        app.active_page_detail = Some(NotionPageDetail {
            page: NotionPage {
                id: "page-root".into(),
                title: "Prefix Page".into(),
                ..Default::default()
            },
            blocks: vec![NotionBlock {
                id: "b-p".into(),
                page_id: "page-root".into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: Vec::new(),
                    color: NotionColor::Default,
                },
                ..Default::default()
            }],
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: Vec::new(),
        });

        assert!(app.open_block_editor(&mut ui, 0, BlockCursorInit::Start));
        for ch in prefix.chars() {
            dispatch(Action::Input(ch), &mut app, &mut ui);
        }
        let bed = ui.block_editor.as_ref().unwrap();
        assert_eq!(
            bed.block_type, expected_type,
            "prefix '{prefix}' should convert Paragraph to '{expected_type}'"
        );
        assert_eq!(
            bed.composer.text(),
            "",
            "prefix '{prefix}' should be stripped from composer text"
        );

        // Type body text after conversion and verify optimistic block updates cleanly
        for ch in "Converted".chars() {
            dispatch(Action::Input(ch), &mut app, &mut ui);
        }
        let blk = &app.active_page_detail.as_ref().unwrap().blocks[0];
        assert_eq!(blk.block_type, expected_type);
        assert_eq!(blk.plain_text(), "Converted");
    }

    // Verify Backspace at column 0 reverts a converted Heading back to Paragraph
    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-root".into());
    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-root".into(),
            title: "Prefix Page".into(),
            ..Default::default()
        },
        blocks: vec![NotionBlock {
            id: "b-p".into(),
            page_id: "page-root".into(),
            block_type: "paragraph".into(),
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain("Keep Me")],
                color: NotionColor::Default,
            },
            ..Default::default()
        }],
        comments: Vec::new(),
        comments_error: None,
        markdown: "Keep Me".into(),
        media: Vec::new(),
    });
    assert!(app.open_block_editor(&mut ui, 0, BlockCursorInit::Start));
    dispatch(Action::Input('#'), &mut app, &mut ui);
    dispatch(Action::Input(' '), &mut app, &mut ui);
    assert_eq!(ui.block_editor.as_ref().unwrap().block_type, "heading_1");
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.text(), "Keep Me");
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.cursor(), 0);

    dispatch(Action::Backspace, &mut app, &mut ui);
    assert_eq!(ui.block_editor.as_ref().unwrap().block_type, "paragraph");
    assert_eq!(ui.block_editor.as_ref().unwrap().composer.text(), "Keep Me");
}

#[test]
fn test_ctrl_m_toggles_between_rendered_block_live_and_markdown_source_preserving_edits() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use unicode_width::UnicodeWidthStr;

    let (mut app, mut ui) = make_test_app_and_ui();
    ui.focus = Focus::Canvas;
    app.active_view = CanvasView::Page;
    app.active_page_id = Some("page-toggle".into());
    app.active_page_detail = Some(NotionPageDetail {
        page: NotionPage {
            id: "page-toggle".into(),
            title: "Mode Toggle Page".into(),
            ..Default::default()
        },
        blocks: vec![
            NotionBlock {
                id: "b-h1".into(),
                page_id: "page-toggle".into(),
                block_type: "heading_1".into(),
                kind: NotionBlockKind::Heading1 {
                    rich_text: vec![RichTextSpan::plain("Section Title")],
                    is_toggleable: false,
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-p".into(),
                page_id: "page-toggle".into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Hello 안녕")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-todo".into(),
                page_id: "page-toggle".into(),
                block_type: "to_do".into(),
                kind: NotionBlockKind::ToDo {
                    rich_text: vec![RichTextSpan::plain("Check item")],
                    checked: false,
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-quote".into(),
                page_id: "page-toggle".into(),
                block_type: "quote".into(),
                kind: NotionBlockKind::Quote {
                    rich_text: vec![RichTextSpan::plain("Quoted line")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "b-callout".into(),
                page_id: "page-toggle".into(),
                block_type: "callout".into(),
                kind: NotionBlockKind::Callout {
                    rich_text: vec![RichTextSpan::plain("Callout note")],
                    icon: Some("💡".into()),
                    color: NotionColor::Yellow,
                },
                ..Default::default()
            },
        ],
        comments: Vec::new(),
        comments_error: None,
        markdown: "# Section Title\n\nHello 안녕\n\n- [ ] Check item\n\n> Quoted line\n\n> 💡 Callout note"
            .into(),
        media: Vec::new(),
    });

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

    // 1. From rendered Page Canvas (before editing), Ctrl+M jumps straight into Markdown Source
    let act_ctrl_m_canvas = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    );
    assert_eq!(act_ctrl_m_canvas, Some(Action::ToggleEditorMode));
    dispatch(Action::ToggleEditorMode, &mut app, &mut ui);
    assert!(ui.inline_editor.is_some());
    assert!(ui.block_editor.is_none());

    // Pressing Ctrl+M again toggles back into Rendered Block-Live WYSIWYG
    let act_ctrl_m_src = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    );
    assert_eq!(act_ctrl_m_src, Some(Action::ToggleEditorMode));
    dispatch(Action::ToggleEditorMode, &mut app, &mut ui);
    assert!(ui.inline_editor.is_none());
    assert!(ui.block_editor.is_some());

    // 2. Edit paragraph block (index 1, mixed ASCII + Korean) in Block-Live WYSIWYG
    assert!(app.open_block_editor(&mut ui, 1, BlockCursorInit::End));
    for ch in " 세계!".chars() {
        dispatch(Action::Input(ch), &mut app, &mut ui);
    }
    assert_eq!(
        ui.block_editor.as_ref().unwrap().composer.text(),
        "Hello 안녕 세계!"
    );

    // Draw Block-Live WYSIWYG and verify exact ASCII + Korean cursor column + status bar hint
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let bed = ui.block_editor.as_ref().unwrap();
    let (cx, _cy) = ui.cursor_pos.expect("block editor must expose cursor_pos");
    assert_eq!(
        cx,
        bed.last_text_x + "Hello 안녕 세계!".width() as u16,
        "Block-Live cursor X must account for ASCII + CJK/Korean display width"
    );
    let live_screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        live_screen.contains("Ctrl+M Src"),
        "status bar should advertise Ctrl+M Src in Block-Live mode"
    );
    assert!(
        live_screen.contains("[Src]"),
        "rendered page should show clickable [Src] pill"
    );
    assert!(ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::ToggleEditorMode))
        .is_some());

    // 3. Press Ctrl+M in Block-Live WYSIWYG -> commits edits & opens full-page Markdown Source near same block
    let act_to_src = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    );
    assert_eq!(act_to_src, Some(Action::ToggleEditorMode));
    dispatch(Action::ToggleEditorMode, &mut app, &mut ui);

    assert!(ui.block_editor.is_none());
    let ed = ui
        .inline_editor
        .as_ref()
        .expect("Ctrl+M from Block-Live must open full-page Markdown Source");
    assert!(
        ed.composer.text().contains("Hello 안녕 세계!"),
        "full-page Markdown Source must contain Block-Live edits, got: {}",
        ed.composer.text()
    );
    let (cursor_line, _) = ed.cursor_line_col();
    let edited_line = ed
        .composer
        .text()
        .lines()
        .nth(cursor_line.saturating_sub(1))
        .unwrap_or("");
    assert!(
        edited_line.contains("Hello 안녕 세계!"),
        "cursor in Markdown Source should land on the edited block's line, got line {cursor_line}: {edited_line:?}"
    );

    // Render Markdown Source and verify clickable [Render] pill and status bar hint
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let src_screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        src_screen.contains("[Render]"),
        "full-page Markdown Source header bar should render [Render] pill"
    );
    assert!(
        src_screen.contains("Ctrl+M Render"),
        "status bar should advertise Ctrl+M Render in Source mode"
    );
    let render_pill_rect = ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::ToggleEditorMode))
        .expect("[Render] pill must map to ButtonId::ToggleEditorMode");

    // 4. Edit inside full-page Markdown Source, then click [Render] pill -> switches back to Block-Live with edits preserved
    for ch in " [Source추가]".chars() {
        dispatch(Action::Input(ch), &mut app, &mut ui);
    }
    assert!(ui
        .inline_editor
        .as_ref()
        .unwrap()
        .composer
        .text()
        .contains("Hello 안녕 세계! [Source추가]"));

    dispatch(
        Action::MouseClick {
            target: HitTarget::Button(ButtonId::ToggleEditorMode),
            col: render_pill_rect.x + 1,
            row: render_pill_rect.y,
        },
        &mut app,
        &mut ui,
    );

    assert!(ui.inline_editor.is_none());
    let bed_after = ui
        .block_editor
        .as_ref()
        .expect("clicking [Render] pill must switch back into Rendered Block-Live WYSIWYG");
    assert!(
        bed_after.composer.text().contains("Hello 안녕 세계! [Source추가]"),
        "Block-Live editor must show the updated text from Markdown Source, got: {:?}",
        bed_after.composer.text()
    );
    assert!(
        app.active_page_detail
            .as_ref()
            .unwrap()
            .markdown
            .contains("Hello 안녕 세계! [Source추가]"),
        "optimistic page detail must preserve Source edits"
    );

    // 5. Clicking [Src] pill from Block-Live switches back to Markdown Source cleanly too
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let src_pill_rect = ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::ToggleEditorMode))
        .expect("[Src] pill must map to ButtonId::ToggleEditorMode");
    dispatch(
        Action::MouseClick {
            target: HitTarget::Button(ButtonId::ToggleEditorMode),
            col: src_pill_rect.x + 1,
            row: src_pill_rect.y,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.block_editor.is_none());
    assert!(ui
        .inline_editor
        .as_ref()
        .expect("clicking [Src] pill must open Markdown Source")
        .composer
        .text()
        .contains("Hello 안녕 세계! [Source추가]"));
}
