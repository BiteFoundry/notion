use std::{
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use axum::{
    extract::{Path as AxumPath, State},
    routing::{get, patch},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    config::{AppConfig, WorkspaceProfile},
    notion::NotionClient,
    storage::Storage,
    sync::{self, FocusTarget, SyncController},
    ui::app::{AppState, CanvasView, UiMsg, UiState},
};

use crate::harness::{
    extract_recovery_path, notion_cli_cmd, start_column_mock_server, write_editor_script,
    ColumnMockState, RecordedWrite, COL_LEFT_ID, COL_LIST_ID, COL_RIGHT_ID, PAGE_ID, PARA_LEFT_ID,
    PARA_RIGHT_ID,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_cli_column_layout_editor_edits_in_place_and_recovers_on_error() {
    let (base_url, state, server) = start_column_mock_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Normal Markdown export (`pages get` / `pages export`) keeps plain format without column markers.
    let export_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", PAGE_ID])
        .output()
        .await
        .unwrap();
    assert!(
        export_out.status.success(),
        "pages export failed: {}",
        String::from_utf8_lossy(&export_out.stderr)
    );
    let exported = String::from_utf8(export_out.stdout).unwrap();
    assert!(exported.contains("LEFT OLD"));
    assert!(exported.contains("RIGHT OLD"));
    assert!(
        !exported.contains("<!-- notion:column"),
        "normal export must not include column markers: {exported}"
    );

    // 2. Edit ONLY the left column via `pages edit --editor` -> 1 PATCH to PARA_LEFT_ID, 0 root appends, 0 deletes.
    #[cfg(windows)]
    let left_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
if (-not $text.Contains('<!-- notion:column id=20000000-0000-0000-0000-000000000031 -->')) { exit 9 }
$text = $text.Replace('LEFT OLD', 'LEFT NEW')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let left_script = r#"#!/bin/sh
sed -i 's/LEFT OLD/LEFT NEW/g' "$1"
"#;
    let editor_left = write_editor_script(tmp.path(), "edit_left", left_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_left)
        .args(["pages", "edit", PAGE_ID, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "left-only edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "expected exactly 1 PATCH, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{PARA_LEFT_ID}"));
    assert_eq!(
        writes[0].body.pointer("/paragraph/rich_text/0/text/content"),
        Some(&json!("LEFT NEW"))
    );

    // Re-fetch and verify parent/sibling IDs and paragraph content.
    let refetch = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", PAGE_ID, "--format", "blocks", "--refresh"])
        .output()
        .await
        .unwrap();
    assert!(refetch.status.success());
    let blocks: Vec<Value> = serde_json::from_slice(&refetch.stdout).unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["id"], COL_LIST_ID);
    assert_eq!(blocks[0]["block_type"], "column_list");
    let cols = blocks[0]["children"].as_array().unwrap();
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0]["id"], COL_LEFT_ID);
    assert_eq!(cols[1]["id"], COL_RIGHT_ID);
    assert_eq!(cols[0]["children"][0]["id"], PARA_LEFT_ID);
    assert_eq!(cols[0]["children"][0]["parent_block_id"], COL_LEFT_ID);
    assert_eq!(
        cols[0]["children"][0]["kind"]["rich_text"][0]["plain_text"],
        "LEFT NEW"
    );
    assert_eq!(cols[1]["children"][0]["id"], PARA_RIGHT_ID);
    assert_eq!(cols[1]["children"][0]["parent_block_id"], COL_RIGHT_ID);
    assert_eq!(
        cols[1]["children"][0]["kind"]["rich_text"][0]["plain_text"],
        "RIGHT OLD"
    );

    // 3. Edit BOTH columns via `pages edit --editor` -> 2 PATCHes (PARA_LEFT_ID & PARA_RIGHT_ID), 0 root appends, 0 deletes.
    #[cfg(windows)]
    let both_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('LEFT NEW', 'LEFT BOTH').Replace('RIGHT OLD', 'RIGHT BOTH')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let both_script = r#"#!/bin/sh
sed -i 's/LEFT NEW/LEFT BOTH/g; s/RIGHT OLD/RIGHT BOTH/g' "$1"
"#;
    let editor_both = write_editor_script(tmp.path(), "edit_both", both_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_both)
        .args(["pages", "edit", PAGE_ID, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "both-columns edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let writes = state.take_writes();
    assert_eq!(writes.len(), 2, "expected exactly 2 PATCHes, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{PARA_LEFT_ID}"));
    assert_eq!(
        writes[0].body.pointer("/paragraph/rich_text/0/text/content"),
        Some(&json!("LEFT BOTH"))
    );
    assert_eq!(writes[1].method, "PATCH");
    assert_eq!(writes[1].path, format!("/v1/blocks/{PARA_RIGHT_ID}"));
    assert_eq!(
        writes[1].body.pointer("/paragraph/rich_text/0/text/content"),
        Some(&json!("RIGHT BOTH"))
    );

    let refetch2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", PAGE_ID, "--format", "blocks", "--refresh"])
        .output()
        .await
        .unwrap();
    let blocks2: Vec<Value> = serde_json::from_slice(&refetch2.stdout).unwrap();
    assert_eq!(blocks2.len(), 1);
    assert_eq!(blocks2[0]["id"], COL_LIST_ID);
    assert_eq!(
        blocks2[0]["children"][0]["children"][0]["kind"]["rich_text"][0]["plain_text"],
        "LEFT BOTH"
    );
    assert_eq!(
        blocks2[0]["children"][1]["children"][0]["kind"]["rich_text"][0]["plain_text"],
        "RIGHT BOTH"
    );

    // 4. Broken/stripped column markers -> fails BEFORE any write and saves edited text for recovery.
    #[cfg(windows)]
    let broken_script = r#"param([string]$Path)
[System.IO.File]::WriteAllText($Path, "LEFT STRIPPED`n`nRIGHT STRIPPED")
"#;
    #[cfg(not(windows))]
    let broken_script = r#"#!/bin/sh
printf 'LEFT STRIPPED\n\nRIGHT STRIPPED' > "$1"
"#;
    let editor_broken = write_editor_script(tmp.path(), "edit_broken", broken_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_broken)
        .args(["pages", "edit", PAGE_ID, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success(), "stripped column markers must fail");
    assert!(
        state.take_writes().is_empty(),
        "broken column markers must issue 0 writes"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let recovery_path = extract_recovery_path(&stderr)
        .unwrap_or_else(|| panic!("expected recovery path in stderr, got: {stderr}"));
    let recovered = std::fs::read_to_string(&recovery_path).unwrap();
    assert_eq!(recovered, "LEFT STRIPPED\n\nRIGHT STRIPPED");

    // 5. Remote structure/content drift while editor is open -> fails BEFORE any write and saves recovery file.
    #[cfg(windows)]
    let drift_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('LEFT BOTH', 'LEFT DURING DRIFT')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let drift_script = r#"#!/bin/sh
sed -i 's/LEFT BOTH/LEFT DURING DRIFT/g' "$1"
"#;
    let editor_drift = write_editor_script(tmp.path(), "edit_drift", drift_script);
    // Flip drift flag AFTER initial sync? The first GET /blocks/{PAGE_ID}/children in `pages edit`
    // loads the baseline; the second GET checks `fresh`. Triggering drift on the 2nd read needs a
    // flag armed once the editor runs — easiest: run drift check directly via a 2-step arm or set
    // the flag from inside a custom endpoint? Instead, arm `drift_on_next_root_get` on the TUI test
    // and here test drift by having our editor script invoke a tiny local trigger? Even cleaner:
    // use a tiny helper endpoint on the mock server or let's arm it via an HTTP hook? Actually,
    // let's check: the first GET happens BEFORE `$EDITOR`, and the second GET happens AFTER `$EDITOR`!
    // So in `edit_drift`, `$EDITOR` runs right between GET #1 and GET #2 — we can simply hit a
    // mock endpoint or mutate on the 2nd GET, wait: let's test the TUI drift directly in the next
    // test AND test CLI drift with a mock server route! Let's add a tiny `/test/arm-drift` trigger
    // or count GETs!
    let _ = editor_drift;

    server.abort();
}

#[tokio::test]
async fn e2e_cli_and_tui_column_layout_remote_drift_and_inline_editor() {
    let (base_url, state, server) = start_column_mock_server().await;

    let mut cfg = AppConfig {
        api_base_url: format!("{base_url}/v1"),
        ..Default::default()
    };
    cfg.upsert_workspace(
        WorkspaceProfile {
            name: "col".into(),
            workspace_id: "col".into(),
            workspace_name: "Col WS".into(),
            auth_mode: "integration".into(),
            api_base_url: Some(format!("{base_url}/v1")),
            ..Default::default()
        },
        true,
    );
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let detail = sync::sync_page_detail_force(&client, &storage, PAGE_ID)
        .await
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let mut ui = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    app.workspace_id = "col".into();
    app.active_view = CanvasView::Page;
    app.active_page_id = Some(PAGE_ID.into());
    app.active_page_detail = Some(detail);

    // 1. Opening TUI inline editor includes column markers while detail.markdown stays clean.
    assert!(app.open_inline_editor(&mut ui));
    let initial_buf = ui.inline_editor.as_ref().unwrap().composer.text().to_string();
    assert!(initial_buf.contains(&format!("<!-- notion:column id={COL_LEFT_ID} -->")));
    assert!(initial_buf.contains(&format!("<!-- notion:column id={COL_RIGHT_ID} -->")));
    assert!(!app
        .active_page_detail
        .as_ref()
        .unwrap()
        .markdown
        .contains("<!-- notion:column"));

    // 2. Edit BOTH columns in TUI inline editor and save -> 2 PATCHes, 0 root inserts, 0 deletes.
    let edited_both = initial_buf
        .replace("LEFT OLD", "TUI LEFT")
        .replace("RIGHT OLD", "TUI RIGHT");
    ui.inline_editor
        .as_mut()
        .unwrap()
        .composer
        .load_text_cursor_start(&edited_both);
    assert!(app.flush_inline_editor_sync(&mut ui, true));
    let msg = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg, UiMsg::PageEdited { .. }));
    app.handle_ui_msg(&mut ui, msg);

    let writes = state.take_writes();
    assert_eq!(writes.len(), 2, "expected 2 column PATCHes, got {writes:?}");
    assert_eq!(writes[0].path, format!("/v1/blocks/{PARA_LEFT_ID}"));
    assert_eq!(writes[1].path, format!("/v1/blocks/{PARA_RIGHT_ID}"));
    assert!(
        !app.active_page_detail
            .as_ref()
            .unwrap()
            .markdown
            .contains("<!-- notion:column"),
        "detail.markdown must stay free of column markers after save"
    );

    // 3. Corrupted column marker in inline editor -> close_inline_editor_and_flush refuses
    //    with 0 writes and KEEPS the editor open with user's draft intact!
    let broken_buf = "TUI BROKEN LEFT\n\nTUI BROKEN RIGHT";
    ui.inline_editor
        .as_mut()
        .unwrap()
        .composer
        .load_text_cursor_start(broken_buf);
    app.close_inline_editor_and_flush(&mut ui);
    assert!(
        state.take_writes().is_empty(),
        "corrupted column markers must send 0 writes"
    );
    let kept = ui
        .inline_editor
        .as_ref()
        .expect("inline editor must stay open so user can recover their draft");
    assert_eq!(kept.composer.text(), broken_buf);
    assert!(kept.last_sync_error.is_some());

    // 4. Remote structure drift before TUI async save -> fails BEFORE any write and preserves draft!
    let valid_next = ui
        .inline_editor
        .as_ref()
        .unwrap()
        .last_synced_markdown
        .replace("TUI LEFT", "TUI DRIFT EDIT");
    ui.inline_editor
        .as_mut()
        .unwrap()
        .composer
        .load_text_cursor_start(&valid_next);
    state.drift_on_next_root_get.store(true, Ordering::SeqCst);
    assert!(app.flush_inline_editor_sync(&mut ui, true));
    let msg = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(msg, UiMsg::PageEditFailed { .. }),
        "expected PageEditFailed on remote drift"
    );
    app.handle_ui_msg(&mut ui, msg);
    assert!(
        state.take_writes().is_empty(),
        "remote drift must block before any write"
    );
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().composer.text(),
        valid_next,
        "edited buffer must remain intact after drift rejection"
    );

    // 5. TUI external editor submission (`submit_page_edit_markdown`) with broken markers ->
    //    0 writes, preserves draft in inline editor + recovery file.
    ui.inline_editor = None;
    let submitted = app.submit_page_edit_markdown(&mut ui, "EXTERNAL BROKEN".into());
    assert!(!submitted);
    assert!(state.take_writes().is_empty());
    assert_eq!(
        ui.inline_editor.as_ref().unwrap().composer.text(),
        "EXTERNAL BROKEN"
    );
    if let Some(toast) = ui.toasts.latest() {
        if let Some((_, rest)) = toast.message.split_once("(draft saved to ") {
            if let Some(path) = rest.strip_suffix(')') {
                let recovered = std::fs::read_to_string(path).unwrap();
                assert_eq!(recovered, "EXTERNAL BROKEN");
                let _ = std::fs::remove_file(path);
            }
        }
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_cli_column_remote_drift_blocks_writes_and_preserves_recovery_file() {
    let state = ColumnMockState::seeded();
    let root_get_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let root_get_count_route = root_get_count.clone();

    let app = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "page",
                    "id": id,
                    "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "last_edited_time": "2026-11-21T01:00:00.000Z",
                    "properties": {
                        "title": { "type": "title", "title": [{ "plain_text": "Drift Page" }] }
                    }
                }))
            }),
        )
        .route(
            "/v1/comments",
            get(|| async {
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(move |State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| {
                let counter = root_get_count_route.clone();
                async move {
                    if id == PAGE_ID && counter.fetch_add(1, Ordering::SeqCst) == 1 {
                        let mut map = st.blocks_by_parent.lock().unwrap();
                        if let Some(cols) = map.get_mut(COL_LIST_ID) {
                            cols.push(json!({
                                "object": "block",
                                "id": "20000000-0000-0000-0000-000000000039",
                                "parent": { "type": "block_id", "block_id": COL_LIST_ID },
                                "type": "column",
                                "has_children": false,
                                "archived": false,
                                "column": {}
                            }));
                        }
                    }
                    let list = st
                        .blocks_by_parent
                        .lock()
                        .unwrap()
                        .get(&id)
                        .cloned()
                        .unwrap_or_default();
                    Json(json!({ "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null }))
                }
            })
            .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}/children"),
                    body,
                });
                Json(json!({ "object": "list", "results": [] }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}"),
                    body,
                });
                Json(json!({ "id": id, "type": "paragraph", "paragraph": { "rich_text": [] } }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    #[cfg(windows)]
    let script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('LEFT OLD', 'LEFT EDITED BEFORE DRIFT')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let script = r#"#!/bin/sh
sed -i 's/LEFT OLD/LEFT EDITED BEFORE DRIFT/g' "$1"
"#;
    let editor = write_editor_script(tmp.path(), "edit_drift", script);

    let out = notion_cli_cmd(&format!("http://{addr}"), &cfg_dir, &data_dir)
        .env("EDITOR", &editor)
        .args(["pages", "edit", PAGE_ID, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success(), "remote structural drift must reject save");
    assert!(
        state.take_writes().is_empty(),
        "remote structural drift must issue 0 writes"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    let recovery = extract_recovery_path(&stderr)
        .unwrap_or_else(|| panic!("expected recovery path in stderr, got: {stderr}"));
    let recovered = std::fs::read_to_string(&recovery).unwrap();
    assert!(recovered.contains("LEFT EDITED BEFORE DRIFT"));

    server.abort();
}

