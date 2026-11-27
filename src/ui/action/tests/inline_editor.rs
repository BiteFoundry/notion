use ratatui::{backend::TestBackend, Terminal};

use crate::{
    auth::AuthManager,
    config::AppConfig,
    models::{NotionBlock, NotionBlockKind, NotionColor, NotionPage, NotionPageDetail, RichTextSpan},
    notion::NotionClient,
    storage::Storage,
    sync::{FocusTarget, SyncController},
    ui::{
        self,
        action::{dispatch, Action},
        app::{AppState, CanvasView, Focus, UiMsg, UiState},
        hitmap::{ButtonId, HitTarget},
    },
};

#[tokio::test]
async fn test_inline_editor_opens_in_canvas_without_popup_optimistic_update_and_esc_done_scroll_preserved() {
    use std::sync::{Arc, Mutex};
    use axum::{
        extract::Path as AxumPath,
        routing::{delete, get},
        Json, Router,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::net::TcpListener;

    type AppendPayloadLog = Arc<Mutex<Vec<(Option<String>, Vec<serde_json::Value>)>>>;
    let deleted_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let patched_blocks: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let appended_payloads: AppendPayloadLog = Arc::new(Mutex::new(Vec::new()));
    let current_blocks: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![
        serde_json::json!({
            "id": "blk-old-1",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "Old content" }] }
        }),
        serde_json::json!({
            "id": "blk-child-page",
            "type": "child_page",
            "has_children": false,
            "child_page": { "title": "Keep Child Page" }
        }),
    ]));

    let del_log = deleted_ids.clone();
    let patch_log = patched_blocks.clone();
    let blocks_del = current_blocks.clone();
    let blocks_get = current_blocks.clone();
    let blocks_append = current_blocks.clone();
    let blocks_update = current_blocks.clone();
    let app_log = appended_payloads.clone();

    let router = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(serde_json::json!({
                    "object": "page",
                    "id": id,
                    "url": "https://www.notion.so/page-edit-1",
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "properties": {
                        "title": { "type": "title", "title": [{ "plain_text": "Editable Page" }] }
                    }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(move || {
                let b = blocks_get.clone();
                async move {
                    Json(serde_json::json!({
                        "object": "list",
                        "results": b.lock().unwrap().clone(),
                        "has_more": false
                    }))
                }
            })
            .patch(move |Json(body): Json<serde_json::Value>| {
                let b = blocks_append.clone();
                let log = app_log.clone();
                async move {
                    let after = body
                        .get("after")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let children = body
                        .get("children")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    log.lock().unwrap().push((after, children.clone()));
                    let mut guard = b.lock().unwrap();
                    let mut created = Vec::new();
                    for (i, mut c) in children.into_iter().enumerate() {
                        c["id"] = serde_json::json!(format!("blk-new-{}-{}", guard.len(), i));
                        c["has_children"] = serde_json::json!(false);
                        guard.push(c.clone());
                        created.push(c);
                    }
                    Json(serde_json::json!({ "object": "list", "results": created }))
                }
            }),
        )
        .route(
            "/v1/blocks/{id}",
            delete(move |AxumPath(id): AxumPath<String>| {
                let d = del_log.clone();
                let b = blocks_del.clone();
                async move {
                    d.lock().unwrap().push(id.clone());
                    b.lock().unwrap().retain(|blk| blk["id"] != id);
                    Json(serde_json::json!({ "id": id, "archived": true }))
                }
            })
            .patch(
                move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                    let log = patch_log.clone();
                    let b = blocks_update.clone();
                    async move {
                        log.lock().unwrap().push((id.clone(), body.clone()));
                        let mut guard = b.lock().unwrap();
                        if let Some(existing) = guard.iter_mut().find(|blk| blk["id"] == id) {
                            if let Some(btype) = existing["type"].as_str().map(str::to_string) {
                                if let Some(updated_inner) = body.get(&btype) {
                                    existing[&btype] = updated_inner.clone();
                                }
                            }
                            return Json(existing.clone());
                        }
                        Json(serde_json::json!({
                            "id": id,
                            "type": "paragraph",
                            "has_children": false,
                            "paragraph": body.get("paragraph").cloned().unwrap_or_else(|| serde_json::json!({ "rich_text": [] }))
                        }))
                    }
                },
            ),
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
    let auth = AuthManager::from_token(cfg.clone(), "ntn_edit_test");
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
        id: "11111111-2222-3333-4444-555555555555".into(),
        workspace_id: app.workspace_id.clone(),
        title: "Editable Page".into(),
        ..Default::default()
    };
    let other_page = NotionPage {
        id: "22222222-2222-3333-4444-555555555555".into(),
        workspace_id: app.workspace_id.clone(),
        title: "Second Page".into(),
        ..Default::default()
    };
    app.storage.upsert_page(&page).unwrap();
    app.storage.upsert_page(&other_page).unwrap();
    app.pages = [&page, &other_page].into_iter().map(Into::into).collect();
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page.id.clone());
    app.active_page_detail = Some(NotionPageDetail {
        page: page.clone(),
        blocks: vec![
            NotionBlock {
                id: "blk-old-1".into(),
                page_id: page.id.clone(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Old content")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            NotionBlock {
                id: "blk-child-page".into(),
                page_id: page.id.clone(),
                block_type: "child_page".into(),
                kind: NotionBlockKind::ChildPage {
                    title: "Keep Child Page".into(),
                },
                ..Default::default()
            },
        ],
        comments: Vec::new(),
        comments_error: None,
        markdown: "Old content".into(),
        media: Vec::new(),
    });

    // 1. Pressing 'e' opens INLINE editor directly inside Canvas (ZERO popup overlays!)
    let act_e = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_e, Action::OpenPageEditor);
    dispatch(act_e, &mut app, &mut ui);
    assert!(!ui.editor_requested, "'e' must NEVER launch external editor");
    assert!(ui.inline_editor.is_some(), "'e' must open inline Canvas editor");
    assert!(ui.overlays.is_empty(), "inline editor must NOT open a popup overlay");

    // Pressing Esc when clean exits inline editor cleanly
    let act_esc = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_esc, Action::CancelInlineEdit);
    dispatch(act_esc, &mut app, &mut ui);
    assert!(ui.inline_editor.is_none());

    // 2. Clicking [✎ Edit] pill opens INLINE editor in Canvas (ZERO popup overlays!)
    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let edit_btn = ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::EditPage))
        .expect("[✎ Edit] button should map to ButtonId::EditPage");
    dispatch(
        Action::MouseClick {
            target: HitTarget::Button(ButtonId::EditPage),
            col: edit_btn.x + 1,
            row: edit_btn.y,
        },
        &mut app,
        &mut ui,
    );
    assert!(!ui.editor_requested, "[✎ Edit] must NEVER launch external editor");
    assert!(ui.inline_editor.is_some());
    assert!(ui.overlays.is_empty());

    // Render inline editor and verify header bar pills, cursor position, and hitmap targets
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui);
        })
        .unwrap();
    let edit_screen: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(edit_screen.contains("EDITING"));
    assert!(edit_screen.contains("[✓ Done]"));
    assert!(edit_screen.contains("[Ctrl+S Save]"));
    assert!(edit_screen.contains("[Ext $EDITOR]"));
    assert!(edit_screen.contains("Ln 1, Col 1"));
    assert!(ui.cursor_pos.is_some(), "inline editor must expose live terminal cursor");
    assert!(ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::FinishInlineEdit))
        .is_some());
    assert!(ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::SaveInlineEdit))
        .is_some());
    assert!(ui.hitmap.region_of(HitTarget::InlineEditorBody).is_some());

    // 3. Typing keys (including 'q', 'j', 'k', 'e', Korean UTF-8, Enter, Tab, Undo) routes to inline editor
    //    AND immediately updates optimistic local page detail in 0ms while preserving ChildPage!
    let editor_body = ui
        .hitmap
        .region_of(HitTarget::InlineEditorBody)
        .expect("inline editor body hit target");
    // Click at col 4 on line 1 ("Old |content") to verify mouse click-to-place cursor
    dispatch(
        Action::MouseClick {
            target: HitTarget::InlineEditorBody,
            col: editor_body.x + 4,
            row: editor_body.y,
        },
        &mut app,
        &mut ui,
    );
    assert_eq!(ui.inline_editor.as_ref().unwrap().cursor_line_col(), (1, 5));

    dispatch(Action::CursorEnd, &mut app, &mut ui);
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

    for ch in "qje 새 문단X".chars() {
        let act_ch = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_ch, Action::Input(ch));
        dispatch(act_ch, &mut app, &mut ui);
    }

    // Ctrl+Z undoes 'X'
    let act_undo = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_undo, Action::Undo);
    dispatch(act_undo, &mut app, &mut ui);

    // Rendering the draft leaves the active block model intact until save or preview.
    let ed_gen = ui.inline_editor.as_ref().unwrap().edit_generation;
    assert!(ed_gen > 0);
    let active_detail = app.active_page_detail.as_ref().unwrap();
    assert_eq!(active_detail.markdown, "Old content");
    assert!(!active_detail.page.plain_text.contains("qje 새 문단"));
    let opt_detail = app.detail_for_block_action(&ui).unwrap();
    assert_eq!(opt_detail.markdown, "Old content\nqje 새 문단");
    assert!(opt_detail.page.plain_text.contains("qje 새 문단"));
    assert!(
        opt_detail
            .blocks
            .iter()
            .any(|b| matches!(b.kind, NotionBlockKind::ChildPage { ref title } if title == "Keep Child Page")),
        "optimistic update must preserve ChildPage blocks"
    );
    drop(opt_detail);

    // 4. Ctrl+S flushes immediately while STAYING in inline edit mode!
    let act_save = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_save, Action::SaveInlineEdit);
    dispatch(act_save, &mut app, &mut ui);
    assert!(
        ui.inline_editor.is_some(),
        "Ctrl+S must stay in inline edit mode while flushing sync"
    );
    assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);

    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("timed out waiting for PageEdited")
        .expect("ui_rx closed");
    let expected_gen = ui.inline_editor.as_ref().unwrap().edit_generation;
    assert!(
        matches!(msg, UiMsg::PageEdited { generation, .. } if generation == expected_gen),
        "expected UiMsg::PageEdited with matching generation"
    );
    app.handle_ui_msg(&mut ui, msg);
    assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().sync_status().0,
        "✓ Saved"
    );
    // Unchanged first paragraph was NOT deleted! Only the new 2nd paragraph was appended with `after: blk-old-1`!
    assert!(deleted_ids.lock().unwrap().is_empty());
    assert_eq!(appended_payloads.lock().unwrap().len(), 1);
    assert_eq!(
        appended_payloads.lock().unwrap()[0].0.as_deref(),
        Some("blk-old-1")
    );

    // 5. Clicking [✓ Done] returns to rendered Canvas preserving scroll position
    ui.inline_editor.as_mut().unwrap().scroll_y = 7;
    let done_btn = ui
        .hitmap
        .region_of(HitTarget::Button(ButtonId::FinishInlineEdit))
        .expect("[✓ Done] pill should be in hitmap");
    dispatch(
        Action::MouseClick {
            target: HitTarget::Button(ButtonId::FinishInlineEdit),
            col: done_btn.x + 1,
            row: done_btn.y,
        },
        &mut app,
        &mut ui,
    );
    assert!(ui.inline_editor.is_none());
    assert_eq!(ui.canvas_scroll, 7);

    // 6. Ctrl+E still requests external $EDITOR
    let act_ext = crate::ui::event::key_to_action(
        KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
        ui.focus(),
        &ui.keymap,
        ui.accepts_text_input(),
        false,
        ui.db_view_mode,
    )
    .unwrap();
    assert_eq!(act_ext, Action::RequestExternalEditor);
    dispatch(act_ext, &mut app, &mut ui);
    assert!(ui.editor_requested);
    ui.editor_requested = false;

    // 7. Navigating to another page while inline editing flushes and closes cleanly
    app.open_inline_editor(&mut ui);
    assert!(ui.inline_editor.is_some());
    dispatch(Action::Input('!'), &mut app, &mut ui);
    app.open_page(&other_page.id, &mut ui, true);
    assert!(ui.inline_editor.is_none());
    assert_eq!(app.active_page_id.as_deref(), Some(other_page.id.as_str()));

    server_handle.abort();
}

#[tokio::test]
async fn test_manual_save_only_sync_confirm_discard_and_undo_save_revert() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use axum::{
        extract::Path as AxumPath,
        routing::get,
        Json, Router,
    };
    use tokio::net::TcpListener;

    let patch_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let patch_log = patch_texts.clone();
    let confirmed_text = patch_texts.clone();

    let router = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(serde_json::json!({
                    "object": "page", "id": id,
                    "parent": { "type": "workspace", "workspace": true },
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": "Manual Save Page" }] } }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            axum::routing::patch(
                move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                    let log = patch_log.clone();
                    async move {
                        let content = body
                            .pointer("/paragraph/rich_text")
                            .and_then(|value| value.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|span| span.pointer("/text/content").and_then(|value| value.as_str()))
                            .collect::<String>();
                        log.lock().unwrap().push(content.clone());
                        Json(serde_json::json!({
                            "id": id,
                            "type": "paragraph",
                            "has_children": false,
                            "paragraph": { "rich_text": [{ "plain_text": content }] }
                        }))
                    }
                },
            ),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(move || {
                let confirmed_text = confirmed_text.clone();
                async move {
                    let content = confirmed_text.lock().unwrap().last().cloned()
                        .unwrap_or_else(|| "Hello".to_owned());
                    Json(serde_json::json!({
                        "object": "list", "has_more": false, "results": [{
                            "id": "blk-p-1", "type": "paragraph", "has_children": false,
                            "paragraph": { "rich_text": [{ "plain_text": content }] }
                        }]
                    }))
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
    let auth = AuthManager::from_token(cfg.clone(), "ntn_manual_save_test");
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
        id: "33333333-2222-3333-4444-555555555555".into(),
        workspace_id: app.workspace_id.clone(),
        title: "Manual Save Page".into(),
        ..Default::default()
    };
    app.storage.upsert_page(&page).unwrap();
    app.pages = vec![(&page).into()];
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(page.id.clone());
    app.active_page_detail = Some(NotionPageDetail {
        page: page.clone(),
        blocks: vec![NotionBlock {
            id: "blk-p-1".into(),
            page_id: page.id.clone(),
            block_type: "paragraph".into(),
            kind: NotionBlockKind::Paragraph {
                rich_text: vec![RichTextSpan::plain("Hello")],
                color: NotionColor::Default,
            },
            ..Default::default()
        }],
        comments: Vec::new(),
        comments_error: None,
        markdown: "Hello".into(),
        media: Vec::new(),
    });

    // 1. Typing makes 0 API calls (manual save-only) and shows "● Unsaved"
    assert!(app.open_inline_editor(&mut ui));
    assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "✓ Saved");
    dispatch(Action::CursorEnd, &mut app, &mut ui);
    dispatch(Action::Paste(" World".into()), &mut app, &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().is_dirty());
    assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
    assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "● Unsaved");
    assert!(patch_texts.lock().unwrap().is_empty(), "typing must NEVER auto-sync");

    // 2. Undo before saving reverts is_dirty() -> false and Ctrl+S makes 0 API calls
    dispatch(Action::Undo, &mut app, &mut ui);
    assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello");
    assert!(!ui.inline_editor.as_ref().unwrap().is_dirty());
    dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
    assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
    assert!(patch_texts.lock().unwrap().is_empty(), "saving unchanged buffer must make 0 API calls");
    assert_eq!(
        ui.toasts.latest().map(|t| t.message.as_str()),
        Some("No changes to save")
    );

    // 3. First Esc with unsaved changes arms confirm_discard WITHOUT syncing; second Esc discards & restores active_page_detail
    dispatch(Action::Paste(" Temp".into()), &mut app, &mut ui);
    assert_eq!(app.active_page_detail.as_ref().unwrap().markdown, "Hello");
    dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
    assert!(ui.inline_editor.is_some(), "1st Esc with unsaved edits must keep editor open");
    assert!(ui.inline_editor.as_ref().unwrap().confirm_discard);
    assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
    assert!(patch_texts.lock().unwrap().is_empty(), "1st Esc must NEVER fire Notion API sync");

    // Typing resets confirm_discard
    dispatch(Action::Input('!'), &mut app, &mut ui);
    assert!(!ui.inline_editor.as_ref().unwrap().confirm_discard);

    // First Esc again -> arm confirm_discard, Second Esc -> discard and restore last_synced_detail!
    dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().confirm_discard);
    dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
    assert!(ui.inline_editor.is_none(), "2nd Esc must exit inline edit mode");
    assert!(patch_texts.lock().unwrap().is_empty(), "2nd Esc discard must make 0 API calls");
    assert_eq!(
        app.active_page_detail.as_ref().unwrap().markdown,
        "Hello",
        "discard must restore active_page_detail back to last_synced_detail"
    );

    // 4. Undo + Save revert: Edit "Hello" -> "Hello World" -> Ctrl+S -> Ctrl+Z -> Ctrl+S
    assert!(app.open_inline_editor(&mut ui));
    dispatch(Action::CursorEnd, &mut app, &mut ui);
    dispatch(Action::Paste(" World".into()), &mut app, &mut ui);
    assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello World");

    // Save "Hello World"
    dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);
    assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "● Saving...");
    let msg1 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("timed out waiting for save 1")
        .expect("ui_rx closed");
    app.handle_ui_msg(&mut ui, msg1);
    assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "✓ Saved");
    assert_eq!(ui.inline_editor.as_ref().unwrap().last_synced_markdown, "Hello World");
    assert_eq!(app.active_page_detail.as_ref().unwrap().markdown, "Hello World");

    // Ctrl+Z (Undo back to "Hello") -> must be dirty!
    dispatch(Action::Undo, &mut app, &mut ui);
    assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello");
    assert!(
        ui.inline_editor.as_ref().unwrap().is_dirty(),
        "undoing after save must mark buffer dirty vs last_synced_markdown"
    );
    assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "● Unsaved");

    // Ctrl+S (Save reverted "Hello") -> MUST PATCH Notion back to "Hello"!
    dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);
    let msg2 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("timed out waiting for revert save")
        .expect("ui_rx closed");
    app.handle_ui_msg(&mut ui, msg2);

    let ed = ui.inline_editor.as_ref().unwrap();
    assert!(!ed.sync_in_flight);
    assert!(!ed.is_dirty());
    assert_eq!(ed.sync_status().0, "✓ Saved");
    assert_eq!(ed.last_synced_markdown, "Hello");
    assert_eq!(app.active_page_detail.as_ref().unwrap().markdown, "Hello");
    assert_eq!(
        *patch_texts.lock().unwrap(),
        vec!["Hello World".to_string(), "Hello".to_string()],
        "Notion API must receive the revert PATCH back to 'Hello'"
    );

    // 5. Typing while an explicit save is in flight must NOT overwrite newer keystrokes or auto-sync them
    dispatch(Action::Paste(" A".into()), &mut app, &mut ui);
    dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
    assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);
    dispatch(Action::Input('B'), &mut app, &mut ui);
    assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello AB");

    let msg3 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .expect("timed out waiting for in-flight save")
        .expect("ui_rx closed");
    app.handle_ui_msg(&mut ui, msg3);
    let ed = ui.inline_editor.as_ref().unwrap();
    assert_eq!(ed.composer.text(), "Hello AB");
    assert_eq!(ed.last_synced_markdown, "Hello A");
    assert!(!ed.sync_in_flight, "typing after Ctrl+S must NOT auto-sync");
    assert!(ed.is_dirty());
    assert_eq!(ed.sync_status().0, "● Unsaved");

    server_handle.abort();
}

#[tokio::test]
async fn test_diff_sync_single_paragraph_edit_calls_one_patch_and_zero_deletes() {
    use std::sync::{Arc, Mutex};
    use axum::{
        extract::Path as AxumPath,
        routing::{delete, get},
        Json, Router,
    };
    use tokio::net::TcpListener;

    type AppendCallLog = Arc<Mutex<Vec<(Option<String>, usize)>>>;
    let deleted_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let patched_ids: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let appended_calls: AppendCallLog = Arc::new(Mutex::new(Vec::new()));

    let initial_blocks = vec![
        serde_json::json!({
            "id": "blk-1",
            "type": "heading_1",
            "has_children": false,
            "heading_1": { "rich_text": [{ "plain_text": "Title Section" }] }
        }),
        serde_json::json!({
            "id": "blk-2",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "First untouched paragraph" }] }
        }),
        serde_json::json!({
            "id": "blk-3",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "Middle line to edit" }] }
        }),
        serde_json::json!({
            "id": "blk-4",
            "type": "to_do",
            "has_children": false,
            "to_do": { "checked": false, "rich_text": [{ "plain_text": "Untouched todo" }] }
        }),
        serde_json::json!({
            "id": "blk-5",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "Last untouched paragraph" }] }
        }),
        serde_json::json!({
            "id": "blk-child",
            "type": "child_page",
            "has_children": false,
            "child_page": { "title": "Preserved Child" }
        }),
    ];

    let state_blocks = Arc::new(Mutex::new(initial_blocks));
    let get_blocks = state_blocks.clone();
    let del_blocks = state_blocks.clone();
    let patch_blocks = state_blocks.clone();
    let append_blocks = state_blocks.clone();
    let del_log = deleted_ids.clone();
    let patch_log = patched_ids.clone();
    let app_log = appended_calls.clone();

    let router = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(serde_json::json!({
                    "object": "page",
                    "id": id,
                    "url": "https://www.notion.so/page-diff",
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "properties": {
                        "title": { "type": "title", "title": [{ "plain_text": "Diff Page" }] }
                    }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(move || {
                let b = get_blocks.clone();
                async move {
                    Json(serde_json::json!({
                        "object": "list",
                        "results": b.lock().unwrap().clone(),
                        "has_more": false
                    }))
                }
            })
            .patch(move |Json(body): Json<serde_json::Value>| {
                let b = append_blocks.clone();
                let log = app_log.clone();
                async move {
                    let after = body
                        .get("after")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let children = body
                        .get("children")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    log.lock().unwrap().push((after.clone(), children.len()));
                    let mut guard = b.lock().unwrap();
                    let mut created = Vec::new();
                    let insert_pos = after
                        .as_deref()
                        .and_then(|aid| guard.iter().position(|x| x["id"] == aid))
                        .map(|p| p + 1)
                        .unwrap_or_else(|| {
                            if body.pointer("/position/type").and_then(|v| v.as_str()) == Some("start") {
                                0
                            } else {
                                guard.len()
                            }
                        });
                    for (i, mut c) in children.into_iter().enumerate() {
                        c["id"] = serde_json::json!(format!("blk-ins-{i}"));
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
                let d = del_log.clone();
                let b = del_blocks.clone();
                async move {
                    d.lock().unwrap().push(id.clone());
                    b.lock().unwrap().retain(|blk| blk["id"] != id);
                    Json(serde_json::json!({ "id": id, "archived": true }))
                }
            })
            .patch(
                move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                    let p = patch_log.clone();
                    let b = patch_blocks.clone();
                    async move {
                        p.lock().unwrap().push((id.clone(), body.clone()));
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
                },
            ),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_diff_test");
    let client = NotionClient::new(&cfg, auth)
        .unwrap()
        .with_base_url(format!("http://{addr}/v1"));
    let storage = Storage::open_in_memory().unwrap();
    let page_id = "44444444-2222-3333-4444-555555555555";

    // 1. Change ONLY paragraph 3 (`blk-3`) on a 5-block + child_page page
    let edited_md = "# Title Section\n\nFirst untouched paragraph\n\nMiddle line UPDATED!\n\n- [ ] Untouched todo\n\nLast untouched paragraph";
    let detail = crate::cli::op_replace_page_markdown(&client, &storage, page_id, edited_md)
        .await
        .unwrap();

    assert_eq!(
        deleted_ids.lock().unwrap().len(),
        0,
        "editing 1 paragraph in place must make 0 DELETE calls"
    );
    assert_eq!(
        appended_calls.lock().unwrap().len(),
        0,
        "editing 1 paragraph in place must make 0 append calls"
    );
    {
        let patches = patched_ids.lock().unwrap();
        assert_eq!(
            patches.len(),
            1,
            "editing 1 paragraph on a 5-block page must make exactly 1 PATCH call"
        );
        assert_eq!(patches[0].0, "blk-3");
        assert_eq!(
            patches[0].1.pointer("/paragraph/rich_text/0/text/content"),
            Some(&serde_json::json!("Middle line UPDATED!"))
        );
    }
    assert!(detail.markdown.contains("Middle line UPDATED!"));
    assert!(detail
        .blocks
        .iter()
        .any(|b| matches!(b.kind, NotionBlockKind::ChildPage { ref title } if title == "Preserved Child")));

    // 2. Pure middle insertion after `blk-3` -> 0 DELETEs, 1 PATCH /children with `"after": "blk-3"`
    patched_ids.lock().unwrap().clear();
    let inserted_md = "# Title Section\n\nFirst untouched paragraph\n\nMiddle line UPDATED!\n\nBrand new inserted line\n\n- [ ] Untouched todo\n\nLast untouched paragraph";
    let detail2 =
        crate::cli::op_replace_page_markdown(&client, &storage, page_id, inserted_md)
            .await
            .unwrap();
    assert_eq!(deleted_ids.lock().unwrap().len(), 0);
    assert_eq!(patched_ids.lock().unwrap().len(), 0);
    {
        let appends = appended_calls.lock().unwrap();
        assert_eq!(appends.len(), 1);
        assert_eq!(appends[0].0.as_deref(), Some("blk-3"));
        assert_eq!(appends[0].1, 1);
    }
    assert!(detail2.markdown.contains("Brand new inserted line"));

    server_handle.abort();
}

#[tokio::test]
async fn test_diff_sync_preserves_hosted_image_and_recovers_from_stale_known_id() {
    use std::sync::{Arc, Mutex};
    use axum::{
        extract::Path as AxumPath,
        http::StatusCode,
        response::IntoResponse,
        routing::{delete, get},
        Json, Router,
    };
    use tokio::net::TcpListener;

    type AppendPayloadLog = Arc<Mutex<Vec<(Option<String>, Vec<serde_json::Value>)>>>;
    let deleted_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let patched_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let appended_payloads: AppendPayloadLog = Arc::new(Mutex::new(Vec::new()));

    let hosted_url = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk/demo.png?X-Amz=sig";
    let state_blocks = Arc::new(Mutex::new(vec![
        serde_json::json!({
            "id": "blk-top",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "Top" }] }
        }),
        serde_json::json!({
            "id": "blk-hosted-img",
            "type": "image",
            "has_children": false,
            "image": {
                "type": "file",
                "file": { "url": hosted_url },
                "caption": []
            }
        }),
        serde_json::json!({
            "id": "blk-bottom",
            "type": "paragraph",
            "has_children": false,
            "paragraph": { "rich_text": [{ "plain_text": "Bottom" }] }
        }),
    ]));

    let get_blocks = state_blocks.clone();
    let del_blocks = state_blocks.clone();
    let patch_blocks = state_blocks.clone();
    let append_blocks = state_blocks.clone();
    let del_log = deleted_ids.clone();
    let patch_log = patched_ids.clone();
    let app_log = appended_payloads.clone();

    let router = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(serde_json::json!({
                    "object": "page",
                    "id": id,
                    "url": "https://www.notion.so/page-img",
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "properties": {
                        "title": { "type": "title", "title": [{ "plain_text": "Hosted Image Page" }] }
                    }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(move || {
                let b = get_blocks.clone();
                async move {
                    Json(serde_json::json!({
                        "object": "list",
                        "results": b.lock().unwrap().clone(),
                        "has_more": false
                    }))
                }
            })
            .patch(move |Json(body): Json<serde_json::Value>| {
                let b = append_blocks.clone();
                let log = app_log.clone();
                async move {
                    let after = body
                        .get("after")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let children = body
                        .get("children")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    log.lock().unwrap().push((after.clone(), children.clone()));
                    let mut guard = b.lock().unwrap();
                    let mut created = Vec::new();
                    let insert_pos = after
                        .as_deref()
                        .and_then(|aid| guard.iter().position(|x| x["id"] == aid))
                        .map(|p| p + 1)
                        .unwrap_or_else(|| {
                            if body.pointer("/position/type").and_then(|v| v.as_str()) == Some("start") {
                                0
                            } else {
                                guard.len()
                            }
                        });
                    for (i, mut c) in children.into_iter().enumerate() {
                        c["id"] = serde_json::json!(format!("blk-ins-{}-{}", guard.len(), i));
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
                let d = del_log.clone();
                let b = del_blocks.clone();
                async move {
                    d.lock().unwrap().push(id.clone());
                    b.lock().unwrap().retain(|blk| blk["id"] != id);
                    Json(serde_json::json!({ "id": id, "archived": true }))
                }
            })
            .patch(
                move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                    let p = patch_log.clone();
                    let b = patch_blocks.clone();
                    async move {
                        p.lock().unwrap().push(id.clone());
                        let mut guard = b.lock().unwrap();
                        if let Some(existing) = guard.iter_mut().find(|blk| blk["id"] == id) {
                            let btype = existing["type"].as_str().unwrap().to_string();
                            if let Some(inner) = body.get(&btype) {
                                existing[&btype] = inner.clone();
                            }
                            return (StatusCode::OK, Json(existing.clone())).into_response();
                        }
                        (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({
                                "code": "object_not_found",
                                "message": "Could not find block with ID"
                            })),
                        )
                            .into_response()
                    }
                },
            ),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let cfg = AppConfig::default();
    let auth = AuthManager::from_token(cfg.clone(), "ntn_img_test");
    let client = NotionClient::new(&cfg, auth)
        .unwrap()
        .with_base_url(format!("http://{addr}/v1"));
    let storage = Storage::open_in_memory().unwrap();
    let page_id = "55555555-2222-3333-4444-555555555555";

    // 1. Edit "Top" -> "Top Updated": 0 deletes on hosted image, 1 PATCH on blk-top!
    let edited_md = format!("Top Updated\n\n![]({hosted_url})\n\nBottom");
    let detail = crate::cli::op_replace_page_markdown(&client, &storage, page_id, &edited_md)
        .await
        .unwrap();
    assert!(
        !deleted_ids.lock().unwrap().iter().any(|id| id == "blk-hosted-img"),
        "hosted image must NEVER be deleted"
    );
    assert!(deleted_ids.lock().unwrap().is_empty());
    assert_eq!(*patched_ids.lock().unwrap(), vec!["blk-top".to_string()]);
    assert!(appended_payloads.lock().unwrap().is_empty());
    assert!(detail.blocks.iter().any(|b| b.id == "blk-hosted-img"));
    assert_eq!(detail.media.len(), 1);

    // 2. Insert a new paragraph ABOVE the hosted image: 0 deletes on hosted image, exact order!
    patched_ids.lock().unwrap().clear();
    let inserted_above_md =
        format!("Intro\n\nTop Updated\n\n![]({hosted_url})\n\nBottom");
    let detail2 =
        crate::cli::op_replace_page_markdown(&client, &storage, page_id, &inserted_above_md)
            .await
            .unwrap();
    assert!(
        !deleted_ids.lock().unwrap().iter().any(|id| id == "blk-hosted-img"),
        "inserting above hosted image must make 0 DELETE calls on the hosted image"
    );
    assert!(
        appended_payloads
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, c)| c.iter())
            .all(|c| c["type"] != "image"),
        "hosted image must NEVER be re-appended as an external image payload"
    );
    let types: Vec<&str> = detail2.blocks.iter().map(|b| b.block_type.as_str()).collect();
    assert_eq!(types, vec!["paragraph", "paragraph", "image", "paragraph"]);
    assert_eq!(detail2.blocks[0].plain_text(), "Intro");
    assert_eq!(detail2.blocks[1].plain_text(), "Top Updated");
    assert_eq!(detail2.blocks[2].id, "blk-hosted-img");
    assert_eq!(detail2.blocks[3].plain_text(), "Bottom");

    // 3. Optimistic local preview preserves hosted image & media when editing text around it
    let mut opt_detail = detail2.clone();
    crate::parser::apply_optimistic_markdown_to_detail(
        &mut opt_detail,
        &format!("Intro Edited\n\nTop Updated\n\n![]({hosted_url})\n\nBottom"),
    );
    assert!(opt_detail.blocks.iter().any(|b| b.id == "blk-hosted-img"));
    assert_eq!(opt_detail.media.len(), 1);

    // 4. Stale known_existing ID -> 404 triggers clean fallback refetch + retry!
    // The cache holds "Intro" under an ID that no longer exists; the content is unchanged.
    let stale_known = vec![
        NotionBlock {
            id: "blk-stale-gone".into(),
            page_id: page_id.into(),
            block_type: "paragraph".into(),
            kind: detail2.blocks[0].kind.clone(),
            ..Default::default()
        },
        detail2.blocks[1].clone(),
        detail2.blocks[2].clone(),
        detail2.blocks[3].clone(),
    ];
    let retry_md = format!("Intro Final\n\nTop Updated\n\n![]({hosted_url})\n\nBottom");
    let detail3 = crate::cli::op_diff_sync_page_markdown(
        &client,
        &storage,
        page_id,
        &retry_md,
        Some(stale_known),
    )
    .await
    .unwrap();
    assert_eq!(detail3.blocks[0].plain_text(), "Intro Final");
    assert!(detail3.blocks.iter().any(|b| b.id == "blk-hosted-img"));

    // 5. Empty image URL must NEVER emit an `image` payload
    assert!(
        crate::parser::markdown_to_notion_blocks("![]()\n![caption]()")
            .iter()
            .all(|b| b["type"] != "image")
    );
    let empty_img_blk = NotionBlock {
        id: "blk-empty-img".into(),
        page_id: page_id.into(),
        block_type: "image".into(),
        kind: NotionBlockKind::Image {
            url: String::new(),
            caption: Vec::new(),
            is_hosted: true,
        },
        ..Default::default()
    };
    assert!(crate::parser::is_preserved_anchor_block(&empty_img_blk));
    assert!(!crate::parser::blocks_to_markdown(&[empty_img_blk]).contains("![]("));

    server_handle.abort();
}
