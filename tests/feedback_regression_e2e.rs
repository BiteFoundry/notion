//! Targeted end-to-end regression tests for:
//! 1. In-place column layout editing across CLI (`pages edit --editor`) and TUI,
//!    including normal Markdown export preservation and draft recovery on broken
//!    column markers or remote structural drift.
//! 2. Hyphen-prefixed free-text CLI arguments across all audited Notion commands,
//!    verifying actual mock-server HTTP payloads, two-line bulleted lists, and
//!    preservation of known flags, missing-value errors, `=`, `--`, and `--help`.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::process::Command;

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
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

const PAGE_ID: &str = "10000000-0000-0000-0000-000000000001";
const COL_LIST_ID: &str = "20000000-0000-0000-0000-000000000030";
const COL_LEFT_ID: &str = "20000000-0000-0000-0000-000000000031";
const COL_RIGHT_ID: &str = "20000000-0000-0000-0000-000000000032";
const PARA_LEFT_ID: &str = "20000000-0000-0000-0000-000000000034";
const PARA_RIGHT_ID: &str = "20000000-0000-0000-0000-000000000035";

#[derive(Clone, Debug)]
struct RecordedWrite {
    method: String,
    path: String,
    body: Value,
}

#[derive(Clone)]
struct ColumnMockState {
    blocks_by_parent: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    writes: Arc<Mutex<Vec<RecordedWrite>>>,
    drift_on_next_root_get: Arc<AtomicBool>,
}

impl ColumnMockState {
    fn seeded() -> Self {
        let mut map = HashMap::new();
        map.insert(
            PAGE_ID.to_string(),
            vec![json!({
                "object": "block",
                "id": COL_LIST_ID,
                "parent": { "type": "page_id", "page_id": PAGE_ID },
                "type": "column_list",
                "has_children": true,
                "archived": false,
                "column_list": {}
            })],
        );
        map.insert(
            COL_LIST_ID.to_string(),
            vec![
                json!({
                    "object": "block",
                    "id": COL_LEFT_ID,
                    "parent": { "type": "block_id", "block_id": COL_LIST_ID },
                    "type": "column",
                    "has_children": true,
                    "archived": false,
                    "column": {}
                }),
                json!({
                    "object": "block",
                    "id": COL_RIGHT_ID,
                    "parent": { "type": "block_id", "block_id": COL_LIST_ID },
                    "type": "column",
                    "has_children": true,
                    "archived": false,
                    "column": {}
                }),
            ],
        );
        map.insert(
            COL_LEFT_ID.to_string(),
            vec![paragraph_json(PARA_LEFT_ID, COL_LEFT_ID, "LEFT OLD")],
        );
        map.insert(
            COL_RIGHT_ID.to_string(),
            vec![paragraph_json(PARA_RIGHT_ID, COL_RIGHT_ID, "RIGHT OLD")],
        );

        Self {
            blocks_by_parent: Arc::new(Mutex::new(map)),
            writes: Arc::new(Mutex::new(Vec::new())),
            drift_on_next_root_get: Arc::new(AtomicBool::new(false)),
        }
    }

    fn take_writes(&self) -> Vec<RecordedWrite> {
        std::mem::take(&mut *self.writes.lock().unwrap())
    }
}

fn paragraph_json(id: &str, parent_block_id: &str, content: &str) -> Value {
    json!({
        "object": "block",
        "id": id,
        "parent": { "type": "block_id", "block_id": parent_block_id },
        "type": "paragraph",
        "has_children": false,
        "archived": false,
        "paragraph": {
            "rich_text": [{
                "type": "text",
                "text": { "content": content, "link": null },
                "plain_text": content,
                "href": null,
                "annotations": {
                    "bold": false, "italic": false, "strikethrough": false,
                    "underline": false, "code": false, "color": "default"
                }
            }]
        }
    })
}

async fn start_column_mock_server() -> (String, ColumnMockState, tokio::task::JoinHandle<()>) {
    let state = ColumnMockState::seeded();
    let ret = state.clone();

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
                        "title": { "type": "title", "title": [{ "plain_text": "Column Page" }] }
                    }
                }))
            }),
        )
        .route(
            "/v1/comments",
            get(|| async {
                Json(json!({
                    "object": "list",
                    "results": [],
                    "has_more": false,
                    "next_cursor": Value::Null
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                if id == PAGE_ID && st.drift_on_next_root_get.swap(false, Ordering::SeqCst) {
                    let mut map = st.blocks_by_parent.lock().unwrap();
                    if let Some(right_children) = map.get_mut(COL_RIGHT_ID) {
                        if let Some(p) = right_children.first_mut() {
                            *p = paragraph_json(PARA_RIGHT_ID, COL_RIGHT_ID, "RIGHT REMOTE DRIFT");
                        }
                    }
                }
                let list = st
                    .blocks_by_parent
                    .lock()
                    .unwrap()
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                Json(json!({
                    "object": "list",
                    "results": list,
                    "has_more": false,
                    "next_cursor": Value::Null
                }))
            })
            .patch(
                |State(st): State<ColumnMockState>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.writes.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}/children"),
                        body: body.clone(),
                    });
                    let mut map = st.blocks_by_parent.lock().unwrap();
                    let entry = map.entry(id.clone()).or_default();
                    let mut created = Vec::new();
                    let insert_pos = body
                        .get("after")
                        .and_then(Value::as_str)
                        .and_then(|aid| entry.iter().position(|b| b["id"] == aid))
                        .map(|p| p + 1)
                        .unwrap_or_else(|| {
                            if body.pointer("/position/type").and_then(Value::as_str)
                                == Some("start")
                            {
                                0
                            } else {
                                entry.len()
                            }
                        });
                    if let Some(children) = body.get("children").and_then(Value::as_array) {
                        for (i, ch) in children.iter().enumerate() {
                            let mut blk = ch.clone();
                            blk["id"] = json!(format!("blk-inserted-{}-{}", entry.len(), i));
                            blk["has_children"] = json!(false);
                            entry.insert(insert_pos + i, blk.clone());
                            created.push(blk);
                        }
                    }
                    Json(json!({ "object": "list", "results": created }))
                },
            ),
        )
        .route(
            "/v1/blocks/{id}",
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                let map = st.blocks_by_parent.lock().unwrap();
                for list in map.values() {
                    if let Some(blk) = list.iter().find(|b| b["id"] == id) {
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .patch(
                |State(st): State<ColumnMockState>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.writes.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}"),
                        body: body.clone(),
                    });
                    let mut map = st.blocks_by_parent.lock().unwrap();
                    for list in map.values_mut() {
                        if let Some(blk) = list.iter_mut().find(|b| b["id"] == id) {
                            let btype = blk["type"].as_str().unwrap_or("paragraph").to_string();
                            if let Some(inner) = body.get(&btype) {
                                let mut updated = inner.clone();
                                if let Some(arr) =
                                    updated.get_mut("rich_text").and_then(Value::as_array_mut)
                                {
                                    for span in arr {
                                        if span.get("plain_text").is_none() {
                                            if let Some(c) = span
                                                .pointer("/text/content")
                                                .and_then(Value::as_str)
                                                .map(str::to_owned)
                                            {
                                                span["plain_text"] = json!(c);
                                            }
                                        }
                                    }
                                }
                                blk[&btype] = updated;
                            }
                            return (StatusCode::OK, Json(blk.clone())).into_response();
                        }
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" })))
                        .into_response()
                },
            )
            .delete(
                |State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                    st.writes.lock().unwrap().push(RecordedWrite {
                        method: "DELETE".into(),
                        path: format!("/v1/blocks/{id}"),
                        body: Value::Null,
                    });
                    let mut map = st.blocks_by_parent.lock().unwrap();
                    for list in map.values_mut() {
                        list.retain(|b| b["id"] != id);
                    }
                    Json(json!({ "id": id, "archived": true }))
                },
            ),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), ret, handle)
}

fn write_editor_script(dir: &std::path::Path, name: &str, body_script: &str) -> String {
    #[cfg(windows)]
    {
        let script = dir.join(format!("{name}.ps1"));
        std::fs::write(&script, body_script).unwrap();
        format!(
            "powershell -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script.display()
        )
    }
    #[cfg(not(windows))]
    {
        let script = dir.join(format!("{name}.sh"));
        std::fs::write(&script, body_script).unwrap();
        format!("sh \"{}\"", script.display())
    }
}

fn notion_cli_cmd(base_url: &str, cfg_dir: &std::path::Path, data_dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_notion"));
    cmd.env("NOTION_API_BASE_URL", format!("{base_url}/v1"))
        .env("NOTION_TOKEN", "ntn_mock_token")
        .env("NOTION_CONFIG_DIR", cfg_dir)
        .env("NOTION_DATA_DIR", data_dir)
        .env("NOTION_DISABLE_KEYRING", "1")
        .env("NOTION_CLI_CACHE_SECS", "0");
    cmd
}

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

    let mut cfg = AppConfig::default();
    cfg.api_base_url = format!("{base_url}/v1");
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

fn extract_recovery_path(stderr: &str) -> Option<PathBuf> {
    stderr.lines().find_map(|line| {
        line.split_once("Edited text saved for recovery at:")
            .map(|(_, path)| PathBuf::from(path.trim()))
    })
}

#[derive(Clone, Default)]
struct CliMockState {
    requests: Arc<Mutex<Vec<RecordedWrite>>>,
}

impl CliMockState {
    fn take(&self) -> Vec<RecordedWrite> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_cli_hyphen_inputs_and_flags_against_mock_server() {
    let state = CliMockState::default();
    let app = Router::new()
        .route(
            "/v1/pages",
            post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/pages".into(),
                    body: body.clone(),
                });
                Json(json!({
                    "object": "page",
                    "id": "page-created-1",
                    "url": "https://www.notion.so/page-created-1",
                    "parent": body.get("parent").cloned().unwrap_or_else(|| json!({"type":"workspace"})),
                    "archived": false,
                    "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                }))
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                let (parent, properties) = if id == "10000000-0000-0000-0000-000000000010" {
                    (
                        json!({ "type": "workspace", "workspace": true }),
                        json!({
                            "title": { "type": "title", "title": [{ "plain_text": "Page 1" }] }
                        }),
                    )
                } else {
                    (
                        json!({ "type": "database_id", "database_id": "db-1" }),
                        json!({
                            "Name": { "type": "title", "title": [{ "plain_text": "Row 1" }] }
                        }),
                    )
                };
                Json(json!({
                    "object": "page",
                    "id": id,
                    "url": format!("https://www.notion.so/{id}"),
                    "parent": parent,
                    "archived": false,
                    "properties": properties
                }))
            })
            .patch(
                |State(st): State<CliMockState>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.requests.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/pages/{id}"),
                        body: body.clone(),
                    });
                    Json(json!({
                        "object": "page",
                        "id": id,
                        "url": format!("https://www.notion.so/{id}"),
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false,
                        "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                    }))
                },
            ),
        )
        .route(
            "/v1/databases/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "database",
                    "id": id,
                    "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "title": [{ "plain_text": "Tasks" }],
                    "description": [],
                    "properties": {
                        "Name": { "id": "title", "type": "title", "title": {} }
                    }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "block",
                    "id": id,
                    "type": "paragraph",
                    "has_children": false,
                    "paragraph": { "rich_text": [{ "type": "text", "text": { "content": "Old" }, "plain_text": "Old" }] }
                }))
            })
            .patch(
                |State(st): State<CliMockState>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.requests.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}"),
                        body: body.clone(),
                    });
                    Json(json!({
                        "object": "block",
                        "id": id,
                        "type": "paragraph",
                        "has_children": false,
                        "paragraph": body.get("paragraph").cloned().unwrap_or_else(|| json!({ "rich_text": [] }))
                    }))
                },
            ),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(|| async {
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            })
            .patch(
                |State(st): State<CliMockState>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.requests.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}/children"),
                        body: body.clone(),
                    });
                    let mut created = Vec::new();
                    if let Some(children) = body.get("children").and_then(Value::as_array) {
                        for (i, ch) in children.iter().enumerate() {
                            let mut blk = ch.clone();
                            blk["id"] = json!(format!("blk-new-{i}"));
                            blk["has_children"] = json!(false);
                            created.push(blk);
                        }
                    }
                    Json(json!({ "object": "list", "results": created }))
                },
            ),
        )
        .route(
            "/v1/comments",
            get(|| async {
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            })
            .post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/comments".into(),
                    body: body.clone(),
                });
                Json(json!({
                    "id": "cmt-1",
                    "discussion_id": "disc-1",
                    "created_time": "2026-11-21T01:00:00Z",
                    "created_by": { "id": "bot-1", "name": "Bot" },
                    "rich_text": body.get("rich_text").cloned().unwrap_or_else(|| json!([]))
                }))
            }),
        )
        .route(
            "/v1/search",
            post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/search".into(),
                    body,
                });
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let page_uuid = "10000000-0000-0000-0000-000000000010";
    let blk_uuid = "20000000-0000-0000-0000-000000000010";
    let db_uuid = "30000000-0000-0000-0000-000000000010";
    let row_uuid = "40000000-0000-0000-0000-000000000010";
    let ko = "- 목록 항목";
    let two_lines = "- first\n- second";

    // 1. pages create --title / --body (two-line list -> 2 bulleted_list_item blocks)
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages", "create", "--parent", page_uuid, "--title", ko, "--body", two_lines, "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/pages");
    assert_eq!(
        reqs[0].body.pointer("/properties/title/title/0/text/content"),
        Some(&json!(ko))
    );
    let children = reqs[0].body["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["type"], "bulleted_list_item");
    assert_eq!(children[1]["type"], "bulleted_list_item");

    // 2. pages append (two-line list -> 2 bulleted_list_item blocks)
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", page_uuid, two_lines, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, format!("/v1/blocks/{page_uuid}/children"));
    let children = reqs[0].body["children"].as_array().unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0]["type"], "bulleted_list_item");
    assert_eq!(
        children[0].pointer("/bulleted_list_item/rich_text/0/text/content"),
        Some(&json!("first"))
    );
    assert_eq!(children[1]["type"], "bulleted_list_item");
    assert_eq!(
        children[1].pointer("/bulleted_list_item/rich_text/0/text/content"),
        Some(&json!("second"))
    );

    // 3. blocks append
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "append", blk_uuid, ko, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, format!("/v1/blocks/{blk_uuid}/children"));
    assert_eq!(reqs[0].body["children"][0]["type"], "bulleted_list_item");
    assert_eq!(
        reqs[0].body.pointer("/children/0/bulleted_list_item/rich_text/0/text/content"),
        Some(&json!("목록 항목"))
    );

    // 4. blocks edit --text
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "edit", blk_uuid, "--text", ko, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, format!("/v1/blocks/{blk_uuid}"));
    assert_eq!(
        reqs[0].body.pointer("/paragraph/rich_text/0/text/content"),
        Some(&json!(ko))
    );

    // 5. comments add
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "add", page_uuid, ko, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/comments");
    assert_eq!(
        reqs[0].body.pointer("/rich_text/0/text/content"),
        Some(&json!(ko))
    );

    // 6. db create-item --title / --body
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db", "create-item", db_uuid, "--title", ko, "--body", two_lines, "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/pages");
    assert_eq!(
        reqs[0].body.pointer("/properties/Name/title/0/text/content"),
        Some(&json!(ko))
    );
    assert_eq!(reqs[0].body["children"][0]["type"], "bulleted_list_item");
    assert_eq!(reqs[0].body["children"][1]["type"], "bulleted_list_item");

    // 7. pages edit --title
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "edit", page_uuid, "--title", ko, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, format!("/v1/pages/{page_uuid}"));
    assert_eq!(
        reqs[0].body.pointer("/properties/title/title/0/text/content"),
        Some(&json!(ko))
    );

    // 8. db update-item --title
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "update-item", row_uuid, "--title", ko, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, format!("/v1/pages/{row_uuid}"));
    assert_eq!(
        reqs[0].body.pointer("/properties/Name/title/0/text/content"),
        Some(&json!(ko))
    );

    // 9. search
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["search", ko, "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/search");
    assert_eq!(reqs[0].body["query"], ko);

    // 10. Explicit `--flag=value` and positional `--` still allow flag-looking text.
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "edit", blk_uuid, "--text=--dry-run", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body.pointer("/paragraph/rich_text/0/text/content"),
        Some(&json!("--dry-run"))
    );

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", page_uuid, "--", "--dry-run"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body.pointer("/children/0/paragraph/rich_text/0/text/content"),
        Some(&json!("--dry-run"))
    );

    // 11. Missing values / real flags / unknown flags produce 0 writes.
    for bad in [
        vec!["blocks", "edit", blk_uuid, "--text", "--json"],
        vec!["blocks", "edit", blk_uuid, "--text", "--help"],
        vec!["pages", "create", "--parent", page_uuid, "--title", "Test", "--body", "--dry-run"],
        vec!["pages", "append", page_uuid, "--json"],
        vec!["pages", "append", page_uuid, "--dry-run"],
        vec!["search", "--dry-run"],
    ] {
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .args(&bad)
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "expected failure for {bad:?}");
        assert!(
            state.take().is_empty(),
            "failed invocation {bad:?} must issue 0 requests"
        );
    }

    // 12. Real `--help` calls print help with exit 0 and 0 network requests.
    for help in [
        vec!["pages", "create", "--parent", page_uuid, "--title", "Test", "--body", ko, "--help"],
        vec!["pages", "append", page_uuid, ko, "--help"],
        vec!["blocks", "edit", blk_uuid, "--text", ko, "--help"],
        vec!["blocks", "edit", "--help"],
        vec!["search", ko, "--help"],
    ] {
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .args(&help)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "help failed for {help:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Usage:"),
            "expected usage help for {help:?}"
        );
        assert!(state.take().is_empty(), "help must issue 0 requests");
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_02_nested_multiline_code_file_and_empty_para_roundtrip() {
    const PAGE: &str = "22000000-0000-0000-0000-000000000001";
    const TOGGLE_ID: &str = "22000000-0000-0000-0000-000000000010";
    const CHILD_MULTI_ID: &str = "22000000-0000-0000-0000-000000000011";
    const CHILD_CODE1_ID: &str = "22000000-0000-0000-0000-000000000012";
    const CHILD_CODE3_ID: &str = "22000000-0000-0000-0000-000000000013";
    const CHILD_FILE_ID: &str = "22000000-0000-0000-0000-000000000014";
    const BULLET_ID: &str = "22000000-0000-0000-0000-000000000020";
    const BULLET_MULTI_ID: &str = "22000000-0000-0000-0000-000000000021";
    const EMPTY_PARA_ID: &str = "22000000-0000-0000-0000-000000000030";
    const ROOT_MULTI_ID: &str = "22000000-0000-0000-0000-000000000031";
    const TAIL_PARA_ID: &str = "22000000-0000-0000-0000-000000000040";

    let mut map: HashMap<String, Vec<Value>> = HashMap::new();
    map.insert(
        PAGE.to_string(),
        vec![
            json!({
                "object": "block",
                "id": TOGGLE_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "toggle",
                "has_children": true,
                "archived": false,
                "toggle": { "rich_text": [{ "type": "text", "text": { "content": "Toggle Title" }, "plain_text": "Toggle Title" }] }
            }),
            json!({
                "object": "block",
                "id": BULLET_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "bulleted_list_item",
                "has_children": true,
                "archived": false,
                "bulleted_list_item": { "rich_text": [{ "type": "text", "text": { "content": "Bullet Title" }, "plain_text": "Bullet Title" }] }
            }),
            json!({
                "object": "block",
                "id": EMPTY_PARA_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "paragraph",
                "has_children": false,
                "archived": false,
                "paragraph": { "rich_text": [] }
            }),
            paragraph_json(ROOT_MULTI_ID, PAGE, "Root Line 1\nRoot Line 2"),
            paragraph_json(TAIL_PARA_ID, PAGE, "Tail Stable"),
        ],
    );
    map.insert(
        TOGGLE_ID.to_string(),
        vec![
            paragraph_json(CHILD_MULTI_ID, TOGGLE_ID, "Line A\nLine B"),
            json!({
                "object": "block",
                "id": CHILD_CODE1_ID,
                "parent": { "type": "block_id", "block_id": TOGGLE_ID },
                "type": "code",
                "has_children": false,
                "archived": false,
                "code": {
                    "language": "python",
                    "rich_text": [{ "type": "text", "text": { "content": "x = 1" }, "plain_text": "x = 1" }]
                }
            }),
            json!({
                "object": "block",
                "id": CHILD_CODE3_ID,
                "parent": { "type": "block_id", "block_id": TOGGLE_ID },
                "type": "code",
                "has_children": false,
                "archived": false,
                "code": {
                    "language": "python",
                    "rich_text": [{ "type": "text", "text": { "content": "def f():\n    return 1\n# end" }, "plain_text": "def f():\n    return 1\n# end" }]
                }
            }),
            json!({
                "object": "block",
                "id": CHILD_FILE_ID,
                "parent": { "type": "block_id", "block_id": TOGGLE_ID },
                "type": "file",
                "has_children": false,
                "archived": false,
                "file": {
                    "type": "external",
                    "name": "spec.pdf",
                    "external": { "url": "https://files.example.com/spec.pdf" }
                }
            }),
        ],
    );
    map.insert(
        BULLET_ID.to_string(),
        vec![paragraph_json(BULLET_MULTI_ID, BULLET_ID, "Bullet A\nBullet B")],
    );

    let state = ColumnMockState {
        blocks_by_parent: Arc::new(Mutex::new(map)),
        writes: Arc::new(Mutex::new(Vec::new())),
        drift_on_next_root_get: Arc::new(AtomicBool::new(false)),
    };
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
                    "last_edited_time": "2026-11-21T02:00:00.000Z",
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": "Nested Edit Page" }] } }
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
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                let list = st.blocks_by_parent.lock().unwrap().get(&id).cloned().unwrap_or_default();
                Json(json!({ "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null }))
            })
            .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}/children"),
                    body: body.clone(),
                });
                let mut map = st.blocks_by_parent.lock().unwrap();
                let entry = map.entry(id).or_default();
                let mut created = Vec::new();
                if let Some(children) = body.get("children").and_then(Value::as_array) {
                    for (i, ch) in children.iter().enumerate() {
                        let mut blk = ch.clone();
                        blk["id"] = json!(format!("blk-inserted-{}-{}", entry.len(), i));
                        blk["has_children"] = json!(false);
                        entry.push(blk.clone());
                        created.push(blk);
                    }
                }
                Json(json!({ "object": "list", "results": created }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                for list in st.blocks_by_parent.lock().unwrap().values() {
                    if let Some(blk) = list.iter().find(|b| b["id"] == id) {
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: body.clone(),
                });
                let mut map = st.blocks_by_parent.lock().unwrap();
                for list in map.values_mut() {
                    if let Some(blk) = list.iter_mut().find(|b| b["id"] == id) {
                        let btype = blk["type"].as_str().unwrap_or("paragraph").to_string();
                        if let Some(patch_inner) = body.get(&btype).and_then(Value::as_object) {
                            if let Some(existing) = blk[&btype].as_object_mut() {
                                for (k, v) in patch_inner {
                                    let mut val = v.clone();
                                    if k == "rich_text" {
                                        if let Some(arr) = val.as_array_mut() {
                                            for span in arr {
                                                if span.get("plain_text").is_none() {
                                                    if let Some(c) = span.pointer("/text/content").and_then(Value::as_str).map(str::to_owned) {
                                                        span["plain_text"] = json!(c);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    existing.insert(k.clone(), val);
                                }
                            }
                        }
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .delete(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "DELETE".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: Value::Null,
                });
                for list in st.blocks_by_parent.lock().unwrap().values_mut() {
                    list.retain(|b| b["id"] != id);
                }
                Json(json!({ "id": id, "archived": true }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Edit ONLY the toggle title -> 1 PATCH on TOGGLE_ID; all 4 children keep IDs, order, and content.
    #[cfg(windows)]
    let toggle_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('Toggle Title', 'Toggle Title v2')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let toggle_script = "#!/bin/sh\nsed -i 's/Toggle Title/Toggle Title v2/g' \"$1\"\n";
    let editor_toggle = write_editor_script(tmp.path(), "edit_toggle", toggle_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_toggle)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "toggle title edit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "expected exactly 1 PATCH on toggle, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{TOGGLE_ID}"));

    {
        let map = state.blocks_by_parent.lock().unwrap();
        let root_ids: Vec<&str> = map[PAGE].iter().filter_map(|b| b["id"].as_str()).collect();
        assert_eq!(root_ids, vec![TOGGLE_ID, BULLET_ID, EMPTY_PARA_ID, ROOT_MULTI_ID, TAIL_PARA_ID]);
        let t_children = &map[TOGGLE_ID];
        let child_ids: Vec<&str> = t_children.iter().filter_map(|b| b["id"].as_str()).collect();
        assert_eq!(child_ids, vec![CHILD_MULTI_ID, CHILD_CODE1_ID, CHILD_CODE3_ID, CHILD_FILE_ID]);
        assert_eq!(t_children[0].pointer("/paragraph/rich_text/0/plain_text"), Some(&json!("Line A\nLine B")));
        assert_eq!(t_children[1].pointer("/code/rich_text/0/plain_text"), Some(&json!("x = 1")));
        assert_eq!(t_children[2].pointer("/code/rich_text/0/plain_text"), Some(&json!("def f():\n    return 1\n# end")));
        assert_eq!(t_children[3].pointer("/file/name"), Some(&json!("spec.pdf")));
    }

    // 2. Edit ONLY the bullet parent title -> 1 PATCH on BULLET_ID; multiline child intact.
    #[cfg(windows)]
    let bullet_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('Bullet Title', 'Bullet Title v2')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let bullet_script = "#!/bin/sh\nsed -i 's/Bullet Title/Bullet Title v2/g' \"$1\"\n";
    let editor_bullet = write_editor_script(tmp.path(), "edit_bullet", bullet_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_bullet)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "bullet title edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "expected 1 PATCH on bullet, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{BULLET_ID}"));
    {
        let map = state.blocks_by_parent.lock().unwrap();
        assert_eq!(map[BULLET_ID].len(), 1);
        assert_eq!(map[BULLET_ID][0]["id"], BULLET_MULTI_ID);
        assert_eq!(map[BULLET_ID][0].pointer("/paragraph/rich_text/0/plain_text"), Some(&json!("Bullet A\nBullet B")));
    }

    // 3. Edit one line of multiline paragraph after empty paragraph -> 1 PATCH on ROOT_MULTI_ID, 0 inserts, 0 archives.
    #[cfg(windows)]
    let multi_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('Root Line 2', 'Root Line 2 edited')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let multi_script = "#!/bin/sh\nsed -i 's/Root Line 2/Root Line 2 edited/g' \"$1\"\n";
    let editor_multi = write_editor_script(tmp.path(), "edit_multi", multi_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_multi)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "multiline edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "expected 1 PATCH on ROOT_MULTI_ID, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{ROOT_MULTI_ID}"));
    let patched_text: String = writes[0]
        .body
        .pointer("/paragraph/rich_text")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|s| s.pointer("/text/content").and_then(Value::as_str))
        .collect();
    assert_eq!(patched_text, "Root Line 1\nRoot Line 2 edited");

    // 4. Control A: Edit unrelated root tail paragraph only -> 1 PATCH on TAIL_PARA_ID.
    #[cfg(windows)]
    let tail_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path)
$text = $text.Replace('Tail Stable', 'Tail Revised')
[System.IO.File]::WriteAllText($Path, $text)
"#;
    #[cfg(not(windows))]
    let tail_script = "#!/bin/sh\nsed -i 's/Tail Stable/Tail Revised/g' \"$1\"\n";
    let editor_tail = write_editor_script(tmp.path(), "edit_tail", tail_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_tail)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "tail edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{TAIL_PARA_ID}"));

    // 5. Control B: Moving or modifying/partially deleting a protected file block fails BEFORE writes
    //    and saves recovery draft; completely omitting it never deletes the file block from Notion.
    #[cfg(windows)]
    let move_file_script = r#"param([string]$Path)
$clip = [char]::ConvertFromUtf32(0x1F4CE)
$line = "  [$clip spec.pdf](https://files.example.com/spec.pdf)"
$root = "[$clip spec.pdf](https://files.example.com/spec.pdf)"
$text = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
if (-not $text.Contains($line)) { exit 9 }
$text = $text.Replace($line, "") + "`n" + $root + "`n"
[System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
"#;
    #[cfg(not(windows))]
    let move_file_script = "#!/bin/sh\npython3 -c 'import sys; p=sys.argv[1]; t=open(p).read().replace(\"  [📎 spec.pdf](https://files.example.com/spec.pdf)\",\"\")+\"\\n[📎 spec.pdf](https://files.example.com/spec.pdf)\\n\"; open(p,\"w\").write(t)' \"$1\"\n";
    let editor_move = write_editor_script(tmp.path(), "move_file", move_file_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_move)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success(), "moving protected file block must fail");
    assert!(state.take_writes().is_empty(), "moving protected block must send 0 writes");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let rec = extract_recovery_path(&stderr).unwrap_or_else(|| panic!("expected recovery path: {stderr}"));
    assert!(std::fs::read_to_string(&rec).unwrap().contains("[📎 spec.pdf]"));

    #[cfg(windows)]
    let edit_file_script = r#"param([string]$Path)
$clip = [char]::ConvertFromUtf32(0x1F4CE)
$line = "  [$clip spec.pdf](https://files.example.com/spec.pdf)"
$edited = "  [$clip spec.pdf]"
$text = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
if (-not $text.Contains($line)) { exit 9 }
$text = $text.Replace($line, $edited)
[System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
"#;
    #[cfg(not(windows))]
    let edit_file_script = "#!/bin/sh\npython3 -c 'import sys; p=sys.argv[1]; t=open(p).read().replace(\"  [📎 spec.pdf](https://files.example.com/spec.pdf)\",\"  [📎 spec.pdf]\"); open(p,\"w\").write(t)' \"$1\"\n";
    let editor_edit_file = write_editor_script(tmp.path(), "edit_file", edit_file_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_edit_file)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(!out.status.success(), "editing protected file block display must fail");
    assert!(state.take_writes().is_empty(), "editing protected block must send 0 writes");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let rec = extract_recovery_path(&stderr).unwrap_or_else(|| panic!("expected recovery path: {stderr}"));
    assert!(std::fs::read_to_string(&rec).unwrap().contains("Toggle Title v2"));

    #[cfg(windows)]
    let omit_file_script = r#"param([string]$Path)
$clip = [char]::ConvertFromUtf32(0x1F4CE)
$line = "  [$clip spec.pdf](https://files.example.com/spec.pdf)"
$text = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
if (-not $text.Contains($line)) { exit 9 }
$text = $text.Replace($line, "")
[System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
"#;
    #[cfg(not(windows))]
    let omit_file_script = "#!/bin/sh\npython3 -c 'import sys; p=sys.argv[1]; t=open(p).read().replace(\"  [📎 spec.pdf](https://files.example.com/spec.pdf)\",\"\"); open(p,\"w\").write(t)' \"$1\"\n";
    let editor_omit = write_editor_script(tmp.path(), "omit_file", omit_file_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_omit)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "omitting protected file anchor keeps it untouched: {}", String::from_utf8_lossy(&out.stderr));
    assert!(state.take_writes().is_empty(), "omitting protected file must issue 0 writes (no DELETE)");
    assert!(
        state.blocks_by_parent.lock().unwrap()[TOGGLE_ID]
            .iter()
            .any(|b| b["id"] == CHILD_FILE_ID),
        "protected file block must stay preserved in Notion"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_04_children_of_headings_quote_paragraph_render_and_edit() {
    const PAGE: &str = "24000000-0000-0000-0000-000000000001";
    const H1_ID: &str = "24000000-0000-0000-0000-000000000010";
    const H1_CH: &str = "24000000-0000-0000-0000-000000000011";
    const H2_ID: &str = "24000000-0000-0000-0000-000000000020";
    const H2_CH: &str = "24000000-0000-0000-0000-000000000021";
    const H3_ID: &str = "24000000-0000-0000-0000-000000000030";
    const H3_CH: &str = "24000000-0000-0000-0000-000000000031";
    const QUOTE_ID: &str = "24000000-0000-0000-0000-000000000040";
    const QUOTE_CH: &str = "24000000-0000-0000-0000-000000000041";
    const PARA_ID: &str = "24000000-0000-0000-0000-000000000050";
    const PARA_CH: &str = "24000000-0000-0000-0000-000000000051";
    const H2_PLAIN_ID: &str = "24000000-0000-0000-0000-000000000060";
    const TOGGLE_ID: &str = "24000000-0000-0000-0000-000000000070";
    const TOGGLE_CH: &str = "24000000-0000-0000-0000-000000000071";
    const BULLET_ID: &str = "24000000-0000-0000-0000-000000000080";
    const BULLET_CH: &str = "24000000-0000-0000-0000-000000000081";

    let mut map: HashMap<String, Vec<Value>> = HashMap::new();
    map.insert(
        PAGE.to_string(),
        vec![
            json!({
                "object": "block", "id": H1_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "heading_1", "has_children": true, "archived": false,
                "heading_1": { "is_toggleable": true, "rich_text": [{ "type": "text", "text": { "content": "H1 Toggle" }, "plain_text": "H1 Toggle" }] }
            }),
            json!({
                "object": "block", "id": H2_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "heading_2", "has_children": true, "archived": false,
                "heading_2": { "is_toggleable": true, "rich_text": [{ "type": "text", "text": { "content": "H2 Toggle" }, "plain_text": "H2 Toggle" }] }
            }),
            json!({
                "object": "block", "id": H3_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "heading_3", "has_children": true, "archived": false,
                "heading_3": { "is_toggleable": true, "rich_text": [{ "type": "text", "text": { "content": "H3 Toggle" }, "plain_text": "H3 Toggle" }] }
            }),
            json!({
                "object": "block", "id": QUOTE_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "quote", "has_children": true, "archived": false,
                "quote": { "rich_text": [{ "type": "text", "text": { "content": "Quote Parent" }, "plain_text": "Quote Parent" }] }
            }),
            json!({
                "object": "block", "id": PARA_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "paragraph", "has_children": true, "archived": false,
                "paragraph": { "rich_text": [{ "type": "text", "text": { "content": "Para Parent" }, "plain_text": "Para Parent" }] }
            }),
            json!({
                "object": "block", "id": H2_PLAIN_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "heading_2", "has_children": false, "archived": false,
                "heading_2": { "is_toggleable": false, "rich_text": [{ "type": "text", "text": { "content": "Plain Heading" }, "plain_text": "Plain Heading" }] }
            }),
            json!({
                "object": "block", "id": TOGGLE_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "toggle", "has_children": true, "archived": false,
                "toggle": { "rich_text": [{ "type": "text", "text": { "content": "Plain Toggle" }, "plain_text": "Plain Toggle" }] }
            }),
            json!({
                "object": "block", "id": BULLET_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "bulleted_list_item", "has_children": true, "archived": false,
                "bulleted_list_item": { "rich_text": [{ "type": "text", "text": { "content": "Plain Bullet" }, "plain_text": "Plain Bullet" }] }
            }),
        ],
    );
    map.insert(H1_ID.into(), vec![paragraph_json(H1_CH, H1_ID, "MARKER_IN_H1")]);
    map.insert(H2_ID.into(), vec![paragraph_json(H2_CH, H2_ID, "MARKER_IN_H2")]);
    map.insert(H3_ID.into(), vec![paragraph_json(H3_CH, H3_ID, "MARKER_IN_H3")]);
    map.insert(QUOTE_ID.into(), vec![paragraph_json(QUOTE_CH, QUOTE_ID, "MARKER_IN_QUOTE")]);
    map.insert(PARA_ID.into(), vec![paragraph_json(PARA_CH, PARA_ID, "MARKER_IN_PARA")]);
    map.insert(TOGGLE_ID.into(), vec![paragraph_json(TOGGLE_CH, TOGGLE_ID, "MARKER_IN_TOGGLE")]);
    map.insert(BULLET_ID.into(), vec![paragraph_json(BULLET_CH, BULLET_ID, "MARKER_IN_BULLET")]);

    let state = ColumnMockState {
        blocks_by_parent: Arc::new(Mutex::new(map)),
        writes: Arc::new(Mutex::new(Vec::new())),
        drift_on_next_root_get: Arc::new(AtomicBool::new(false)),
    };
    let app = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "page", "id": id, "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false, "last_edited_time": "2026-11-21T03:00:00.000Z",
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": "Nested Readers" }] } }
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
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                let list = st.blocks_by_parent.lock().unwrap().get(&id).cloned().unwrap_or_default();
                Json(json!({ "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null }))
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
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                for list in st.blocks_by_parent.lock().unwrap().values() {
                    if let Some(blk) = list.iter().find(|b| b["id"] == id) {
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: body.clone(),
                });
                let mut map = st.blocks_by_parent.lock().unwrap();
                for list in map.values_mut() {
                    if let Some(blk) = list.iter_mut().find(|b| b["id"] == id) {
                        let btype = blk["type"].as_str().unwrap_or("paragraph").to_string();
                        if let Some(patch_inner) = body.get(&btype).and_then(Value::as_object) {
                            if let Some(existing) = blk[&btype].as_object_mut() {
                                for (k, v) in patch_inner {
                                    let mut val = v.clone();
                                    if k == "rich_text" {
                                        if let Some(arr) = val.as_array_mut() {
                                            for span in arr {
                                                if span.get("plain_text").is_none() {
                                                    if let Some(c) = span.pointer("/text/content").and_then(Value::as_str).map(str::to_owned) {
                                                        span["plain_text"] = json!(c);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    existing.insert(k.clone(), val);
                                }
                            }
                        }
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .delete(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "DELETE".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: Value::Null,
                });
                Json(json!({ "id": id, "archived": true }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let markers = [
        "MARKER_IN_H1",
        "MARKER_IN_H2",
        "MARKER_IN_H3",
        "MARKER_IN_QUOTE",
        "MARKER_IN_PARA",
        "MARKER_IN_TOGGLE",
        "MARKER_IN_BULLET",
    ];
    let assert_all_markers_once_in_order = |label: &str, text: &str| {
        let mut last_pos = 0usize;
        for marker in markers {
            let count = text.matches(marker).count();
            assert_eq!(count, 1, "{label}: expected {marker} exactly once, got {count} in:\n{text}");
            let pos = text.find(marker).unwrap();
            assert!(pos >= last_pos, "{label}: marker {marker} appeared out of order in:\n{text}");
            last_pos = pos;
        }
    };

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. pages get --format md
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", PAGE, "--format", "md", "--refresh"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let md_out = String::from_utf8(out.stdout).unwrap();
    assert_all_markers_once_in_order("pages get --format md", &md_out);
    assert!(md_out.contains("## Plain Heading\n\n"));
    assert!(md_out.contains("> ▸ Plain Toggle\n  MARKER_IN_TOGGLE"));
    assert!(md_out.contains("- Plain Bullet\n  MARKER_IN_BULLET"));

    // 2. pages get --format plain (control: unchanged traversal)
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", PAGE, "--format", "plain"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert_all_markers_once_in_order("pages get --format plain", &String::from_utf8(out.stdout).unwrap());

    // 3. pages get --json --compact
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", PAGE, "--json", "--compact"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let compact_val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_all_markers_once_in_order("pages get --compact", compact_val["markdown"].as_str().unwrap());

    // 4. pages export
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", PAGE])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert_all_markers_once_in_order("pages export", &String::from_utf8(out.stdout).unwrap());

    // 5. blocks list (Markdown)
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", PAGE])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    assert_all_markers_once_in_order("blocks list", &String::from_utf8(out.stdout).unwrap());

    // 6. MCP get_page(compact: true)
    let mut cfg = AppConfig::default();
    cfg.api_base_url = format!("{base_url}/v1");
    cfg.upsert_workspace(
        WorkspaceProfile {
            name: "bug04".into(),
            workspace_id: "bug04".into(),
            workspace_name: "Bug04 WS".into(),
            auth_mode: "integration".into(),
            api_base_url: Some(format!("{base_url}/v1")),
            ..Default::default()
        },
        true,
    );
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let mcp_resp = notion_cli::mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "get_page", "arguments": { "page": PAGE, "compact": true } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], json!(false), "{mcp_resp}");
    let mcp_md = mcp_resp["result"]["structuredContent"]["markdown"].as_str().unwrap();
    assert_all_markers_once_in_order("MCP get_page compact", mcp_md);

    // Control: reads & export issued 0 write requests
    assert!(state.take_writes().is_empty(), "reads/export must issue 0 writes");

    // 7. Editor edit: edit ONLY H2 heading text -> 1 PATCH on H2_ID only; all children stay intact
    #[cfg(windows)]
    let h2_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
if (-not $text.Contains('MARKER_IN_H2')) { exit 9 }
$text = $text.Replace('## H2 Toggle', '## H2 Toggle v2')
[System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
"#;
    #[cfg(not(windows))]
    let h2_script = "#!/bin/sh\nsed -i 's/## H2 Toggle/## H2 Toggle v2/g' \"$1\"\n";
    let editor_h2 = write_editor_script(tmp.path(), "edit_h2", h2_script);

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &editor_h2)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "H2 edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "expected 1 PATCH on H2_ID, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{H2_ID}"));
    {
        let map = state.blocks_by_parent.lock().unwrap();
        assert_eq!(map[H2_ID].len(), 1);
        assert_eq!(map[H2_ID][0]["id"], H2_CH);
        assert_eq!(map[H2_ID][0].pointer("/paragraph/rich_text/0/plain_text"), Some(&json!("MARKER_IN_H2")));
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_03_nested_columns_in_toggle_callout_and_synced_block() {
    const PAGE: &str = "23000000-0000-0000-0000-000000000001";
    const INTRO_ID: &str = "23000000-0000-0000-0000-000000000010";
    const WRAP_ID: &str = "23000000-0000-0000-0000-000000000020";
    const CL_ID: &str = "23000000-0000-0000-0000-000000000030";
    const COL_L_ID: &str = "23000000-0000-0000-0000-000000000031";
    const COL_R_ID: &str = "23000000-0000-0000-0000-000000000032";
    const LEFT_P_ID: &str = "23000000-0000-0000-0000-000000000034";
    const RIGHT_P_ID: &str = "23000000-0000-0000-0000-000000000035";

    let build_state = |wrap_block: Value| -> ColumnMockState {
        let mut map = HashMap::new();
        map.insert(
            PAGE.to_string(),
            vec![paragraph_json(INTRO_ID, PAGE, "INTRO OLD"), wrap_block],
        );
        map.insert(
            WRAP_ID.to_string(),
            vec![json!({
                "object": "block", "id": CL_ID,
                "parent": { "type": "block_id", "block_id": WRAP_ID },
                "type": "column_list", "has_children": true, "archived": false,
                "column_list": {}
            })],
        );
        map.insert(
            CL_ID.to_string(),
            vec![
                json!({
                    "object": "block", "id": COL_L_ID,
                    "parent": { "type": "block_id", "block_id": CL_ID },
                    "type": "column", "has_children": true, "archived": false,
                    "column": {}
                }),
                json!({
                    "object": "block", "id": COL_R_ID,
                    "parent": { "type": "block_id", "block_id": CL_ID },
                    "type": "column", "has_children": true, "archived": false,
                    "column": {}
                }),
            ],
        );
        map.insert(COL_L_ID.into(), vec![paragraph_json(LEFT_P_ID, COL_L_ID, "LEFT OLD")]);
        map.insert(COL_R_ID.into(), vec![paragraph_json(RIGHT_P_ID, COL_R_ID, "RIGHT OLD")]);
        ColumnMockState {
            blocks_by_parent: Arc::new(Mutex::new(map)),
            writes: Arc::new(Mutex::new(Vec::new())),
            drift_on_next_root_get: Arc::new(AtomicBool::new(false)),
        }
    };

    let spawn_mock = |state: ColumnMockState| async move {
        let app = Router::new()
            .route(
                "/v1/pages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    Json(json!({
                        "object": "page", "id": id, "url": format!("https://www.notion.so/{id}"),
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false, "last_edited_time": "2026-11-21T04:00:00.000Z",
                        "properties": { "title": { "type": "title", "title": [{ "plain_text": "Nested Columns" }] } }
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
                get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                    let list = st.blocks_by_parent.lock().unwrap().get(&id).cloned().unwrap_or_default();
                    Json(json!({ "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null }))
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
                get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                    for list in st.blocks_by_parent.lock().unwrap().values() {
                        if let Some(blk) = list.iter().find(|b| b["id"] == id) {
                            return (StatusCode::OK, Json(blk.clone())).into_response();
                        }
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
                })
                .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                    st.writes.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}"),
                        body: body.clone(),
                    });
                    let mut map = st.blocks_by_parent.lock().unwrap();
                    for list in map.values_mut() {
                        if let Some(blk) = list.iter_mut().find(|b| b["id"] == id) {
                            let btype = blk["type"].as_str().unwrap_or("paragraph").to_string();
                            if let Some(patch_inner) = body.get(&btype).and_then(Value::as_object) {
                                if let Some(existing) = blk[&btype].as_object_mut() {
                                    for (k, v) in patch_inner {
                                        let mut val = v.clone();
                                        if k == "rich_text" {
                                            if let Some(arr) = val.as_array_mut() {
                                                for span in arr {
                                                    if span.get("plain_text").is_none() {
                                                        if let Some(c) = span.pointer("/text/content").and_then(Value::as_str).map(str::to_owned) {
                                                            span["plain_text"] = json!(c);
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        existing.insert(k.clone(), val);
                                    }
                                }
                            }
                            return (StatusCode::OK, Json(blk.clone())).into_response();
                        }
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
                })
                .delete(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                    st.writes.lock().unwrap().push(RecordedWrite {
                        method: "DELETE".into(),
                        path: format!("/v1/blocks/{id}"),
                        body: Value::Null,
                    });
                    Json(json!({ "id": id, "archived": true }))
                }),
            )
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), server)
    };

    let containers = vec![
        (
            "toggle",
            json!({
                "object": "block", "id": WRAP_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "toggle", "has_children": true, "archived": false,
                "toggle": { "rich_text": [{ "type": "text", "text": { "content": "Wrap" }, "plain_text": "Wrap" }] }
            }),
        ),
        (
            "callout",
            json!({
                "object": "block", "id": WRAP_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "callout", "has_children": true, "archived": false,
                "callout": {
                    "icon": { "type": "emoji", "emoji": "💡" },
                    "rich_text": [{ "type": "text", "text": { "content": "Wrap" }, "plain_text": "Wrap" }]
                }
            }),
        ),
        (
            "synced_block",
            json!({
                "object": "block", "id": WRAP_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "synced_block", "has_children": true, "archived": false,
                "synced_block": { "synced_from": Value::Null }
            }),
        ),
    ];

    let tmp = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let intro_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
$text = $text.Replace('INTRO OLD', 'INTRO NEW')
[System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
"#;
    #[cfg(not(windows))]
    let intro_script = "#!/bin/sh\nsed -i 's/INTRO OLD/INTRO NEW/g' \"$1\"\n";
    let editor_intro = write_editor_script(tmp.path(), "bug03_intro", intro_script);

    #[cfg(windows)]
    let left_script = r#"param([string]$Path)
$text = [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
$text = $text.Replace('LEFT OLD', 'LEFT NEW')
[System.IO.File]::WriteAllText($Path, $text, [System.Text.UTF8Encoding]::new($false))
"#;
    #[cfg(not(windows))]
    let left_script = "#!/bin/sh\nsed -i 's/LEFT OLD/LEFT NEW/g' \"$1\"\n";
    let editor_left = write_editor_script(tmp.path(), "bug03_left", left_script);

    for (label, wrap_block) in containers {
        let state = build_state(wrap_block);
        let (base_url, server) = spawn_mock(state.clone()).await;
        let cfg_dir = tmp.path().join(format!("cfg_{label}"));
        let data_dir = tmp.path().join(format!("data_{label}"));

        // Export keeps clean Markdown without column markers
        let exp = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .args(["pages", "export", PAGE])
            .output()
            .await
            .unwrap();
        assert!(exp.status.success(), "{label}: export failed");
        let exp_str = String::from_utf8(exp.stdout).unwrap();
        assert!(exp_str.contains("LEFT OLD") && exp_str.contains("RIGHT OLD"), "{label}: {exp_str}");
        assert!(!exp_str.contains("<!-- notion:column"), "{label}: export leaked markers");

        // Case 1: Edit INTRO -> 1 PATCH on INTRO_ID only; right & layout IDs preserved
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .env("EDITOR", &editor_intro)
            .args(["pages", "edit", PAGE, "--editor"])
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{label} edit INTRO failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let writes = state.take_writes();
        assert_eq!(writes.len(), 1, "{label} INTRO: expected 1 PATCH, got {writes:?}");
        assert_eq!(writes[0].method, "PATCH");
        assert_eq!(writes[0].path, format!("/v1/blocks/{INTRO_ID}"));

        // Case 2: Edit LEFT -> 1 PATCH on LEFT_P_ID only; right & layout IDs preserved
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .env("EDITOR", &editor_left)
            .args(["pages", "edit", PAGE, "--editor"])
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{label} edit LEFT failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let writes = state.take_writes();
        assert_eq!(writes.len(), 1, "{label} LEFT: expected 1 PATCH, got {writes:?}");
        assert_eq!(writes[0].method, "PATCH");
        assert_eq!(writes[0].path, format!("/v1/blocks/{LEFT_P_ID}"));

        {
            let map = state.blocks_by_parent.lock().unwrap();
            assert_eq!(map[WRAP_ID].len(), 1);
            assert_eq!(map[WRAP_ID][0]["id"], CL_ID);
            assert_eq!(map[CL_ID].len(), 2);
            assert_eq!(map[CL_ID][0]["id"], COL_L_ID);
            assert_eq!(map[CL_ID][1]["id"], COL_R_ID);
            assert_eq!(map[COL_L_ID][0]["id"], LEFT_P_ID);
            assert_eq!(map[COL_L_ID][0].pointer("/paragraph/rich_text/0/plain_text"), Some(&json!("LEFT NEW")));
            assert_eq!(map[COL_R_ID][0]["id"], RIGHT_P_ID);
            assert_eq!(map[COL_R_ID][0].pointer("/paragraph/rich_text/0/plain_text"), Some(&json!("RIGHT OLD")));
        }

        // Control: missing marker, mismatched ID, and swapped column order fail before any write & save recovery draft
        #[cfg(windows)]
        let missing_script = format!(
            "param([string]$Path)\n$t=[System.IO.File]::ReadAllText($Path,[System.Text.Encoding]::UTF8)\n$t=$t.Replace('<!-- notion:column id={COL_R_ID} -->','')\n[System.IO.File]::WriteAllText($Path,$t,[System.Text.UTF8Encoding]::new($false))\n"
        );
        #[cfg(not(windows))]
        let missing_script = format!("#!/bin/sh\nsed -i 's/<!-- notion:column id={COL_R_ID} -->//g' \"$1\"\n");
        let ed_missing = write_editor_script(tmp.path(), &format!("bug03_miss_{label}"), &missing_script);

        #[cfg(windows)]
        let bad_id_script = format!(
            "param([string]$Path)\n$t=[System.IO.File]::ReadAllText($Path,[System.Text.Encoding]::UTF8)\n$t=$t.Replace('{COL_R_ID}','23000000-0000-0000-0000-000000000099')\n[System.IO.File]::WriteAllText($Path,$t,[System.Text.UTF8Encoding]::new($false))\n"
        );
        #[cfg(not(windows))]
        let bad_id_script = format!("#!/bin/sh\nsed -i 's/{COL_R_ID}/23000000-0000-0000-0000-000000000099/g' \"$1\"\n");
        let ed_bad_id = write_editor_script(tmp.path(), &format!("bug03_badid_{label}"), &bad_id_script);

        #[cfg(windows)]
        let swap_script = format!(
            "param([string]$Path)\n$t=[System.IO.File]::ReadAllText($Path,[System.Text.Encoding]::UTF8)\n$t=$t.Replace('{COL_L_ID}','__TMP__').Replace('{COL_R_ID}','{COL_L_ID}').Replace('__TMP__','{COL_R_ID}')\n[System.IO.File]::WriteAllText($Path,$t,[System.Text.UTF8Encoding]::new($false))\n"
        );
        #[cfg(not(windows))]
        let swap_script = format!("#!/bin/sh\nsed -i 's/{COL_L_ID}/__TMP__/g; s/{COL_R_ID}/{COL_L_ID}/g; s/__TMP__/{COL_R_ID}/g' \"$1\"\n");
        let ed_swap = write_editor_script(tmp.path(), &format!("bug03_swap_{label}"), &swap_script);

        for (case_name, ed) in [("missing", ed_missing), ("bad_id", ed_bad_id), ("swap", ed_swap)] {
            let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
                .env("EDITOR", &ed)
                .args(["pages", "edit", PAGE, "--editor"])
                .output()
                .await
                .unwrap();
            assert!(!out.status.success(), "{label} {case_name}: expected failure");
            assert!(state.take_writes().is_empty(), "{label} {case_name}: expected 0 writes");
            let stderr = String::from_utf8_lossy(&out.stderr);
            let rec = extract_recovery_path(&stderr)
                .unwrap_or_else(|| panic!("{label} {case_name}: missing recovery file: {stderr}"));
            assert!(std::fs::read_to_string(&rec).unwrap().contains("INTRO NEW"));
        }

        server.abort();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_09_callout_quote_type_and_grapheme_emoji_preservation() {
    const PAGE: &str = "29000000-0000-0000-0000-000000000001";
    const STAR_CALLOUT_ID: &str = "29000000-0000-0000-0000-000000000010";
    const FLAG_CALLOUT_ID: &str = "29000000-0000-0000-0000-000000000011";
    const FIRE_QUOTE_ID: &str = "29000000-0000-0000-0000-000000000012";
    const CHILD_CALLOUT_ID: &str = "29000000-0000-0000-0000-000000000013";
    const CHILD_CALLOUT_P_ID: &str = "29000000-0000-0000-0000-000000000014";
    const PLAIN_QUOTE_ID: &str = "29000000-0000-0000-0000-000000000015";
    const BULB_CALLOUT_ID: &str = "29000000-0000-0000-0000-000000000016";
    const TOGGLE_ID: &str = "29000000-0000-0000-0000-000000000017";

    let mut map: HashMap<String, Vec<Value>> = HashMap::new();
    map.insert(
        PAGE.to_string(),
        vec![
            json!({
                "object": "block", "id": STAR_CALLOUT_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "callout", "has_children": false, "archived": false,
                "callout": {
                    "icon": { "type": "emoji", "emoji": "⭐" },
                    "rich_text": [{ "type": "text", "text": { "content": "Star note" }, "plain_text": "Star note" }]
                }
            }),
            json!({
                "object": "block", "id": FLAG_CALLOUT_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "callout", "has_children": false, "archived": false,
                "callout": {
                    "icon": { "type": "emoji", "emoji": "🇰🇷" },
                    "rich_text": [{ "type": "text", "text": { "content": "Flag note" }, "plain_text": "Flag note" }]
                }
            }),
            json!({
                "object": "block", "id": FIRE_QUOTE_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "quote", "has_children": false, "archived": false,
                "quote": {
                    "rich_text": [{ "type": "text", "text": { "content": "🔥 Hot take old" }, "plain_text": "🔥 Hot take old" }]
                }
            }),
            json!({
                "object": "block", "id": CHILD_CALLOUT_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "callout", "has_children": true, "archived": false,
                "callout": {
                    "icon": { "type": "emoji", "emoji": "⭐" },
                    "rich_text": [{ "type": "text", "text": { "content": "Star parent" }, "plain_text": "Star parent" }]
                }
            }),
            json!({
                "object": "block", "id": PLAIN_QUOTE_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "quote", "has_children": false, "archived": false,
                "quote": {
                    "rich_text": [{ "type": "text", "text": { "content": "Plain quote old" }, "plain_text": "Plain quote old" }]
                }
            }),
            json!({
                "object": "block", "id": BULB_CALLOUT_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "callout", "has_children": false, "archived": false,
                "callout": {
                    "icon": { "type": "emoji", "emoji": "💡" },
                    "rich_text": [{ "type": "text", "text": { "content": "Bulb note old" }, "plain_text": "Bulb note old" }]
                }
            }),
            json!({
                "object": "block", "id": TOGGLE_ID,
                "parent": { "type": "page_id", "page_id": PAGE },
                "type": "toggle", "has_children": false, "archived": false,
                "toggle": {
                    "rich_text": [{ "type": "text", "text": { "content": "Toggle note old" }, "plain_text": "Toggle note old" }]
                }
            }),
        ],
    );
    map.insert(
        CHILD_CALLOUT_ID.to_string(),
        vec![paragraph_json(CHILD_CALLOUT_P_ID, CHILD_CALLOUT_ID, "Child inside star callout")],
    );

    let state = ColumnMockState {
        blocks_by_parent: Arc::new(Mutex::new(map)),
        writes: Arc::new(Mutex::new(Vec::new())),
        drift_on_next_root_get: Arc::new(AtomicBool::new(false)),
    };
    let app = Router::new()
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "page", "id": id, "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false, "last_edited_time": "2026-11-21T05:00:00.000Z",
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": "Callout Quote Page" }] } }
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
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                let list = st.blocks_by_parent.lock().unwrap().get(&id).cloned().unwrap_or_default();
                Json(json!({ "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null }))
            })
            .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}/children"),
                    body: body.clone(),
                });
                let mut map = st.blocks_by_parent.lock().unwrap();
                let entry = map.entry(id).or_default();
                let mut created = Vec::new();
                if let Some(children) = body.get("children").and_then(Value::as_array) {
                    for (i, ch) in children.iter().enumerate() {
                        let mut blk = ch.clone();
                        blk["id"] = json!(format!("blk-appended-{}-{}", entry.len(), i));
                        blk["has_children"] = json!(false);
                        entry.push(blk.clone());
                        created.push(blk);
                    }
                }
                Json(json!({ "object": "list", "results": created }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            get(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                for list in st.blocks_by_parent.lock().unwrap().values() {
                    if let Some(blk) = list.iter().find(|b| b["id"] == id) {
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .patch(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: body.clone(),
                });
                let mut map = st.blocks_by_parent.lock().unwrap();
                for list in map.values_mut() {
                    if let Some(blk) = list.iter_mut().find(|b| b["id"] == id) {
                        let btype = blk["type"].as_str().unwrap_or("paragraph").to_string();
                        if let Some(patch_inner) = body.get(&btype).and_then(Value::as_object) {
                            if let Some(existing) = blk[&btype].as_object_mut() {
                                for (k, v) in patch_inner {
                                    let mut val = v.clone();
                                    if k == "rich_text" {
                                        if let Some(arr) = val.as_array_mut() {
                                            for span in arr {
                                                if span.get("plain_text").is_none() {
                                                    if let Some(c) = span.pointer("/text/content").and_then(Value::as_str).map(str::to_owned) {
                                                        span["plain_text"] = json!(c);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    existing.insert(k.clone(), val);
                                }
                            }
                        }
                        return (StatusCode::OK, Json(blk.clone())).into_response();
                    }
                }
                (StatusCode::NOT_FOUND, Json(json!({ "code": "object_not_found" }))).into_response()
            })
            .delete(|State(st): State<ColumnMockState>, AxumPath(id): AxumPath<String>| async move {
                st.writes.lock().unwrap().push(RecordedWrite {
                    method: "DELETE".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: Value::Null,
                });
                for list in st.blocks_by_parent.lock().unwrap().values_mut() {
                    list.retain(|b| b["id"] != id);
                }
                Json(json!({ "id": id, "archived": true }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let run_replace_edit = |name: &str, from: &str, to: &str| {
        #[cfg(windows)]
        let script = format!(
            "param([string]$Path)\n$t=[System.IO.File]::ReadAllText($Path,[System.Text.Encoding]::UTF8)\n$t=$t.Replace('{from}','{to}')\n[System.IO.File]::WriteAllText($Path,$t,[System.Text.UTF8Encoding]::new($false))\n"
        );
        #[cfg(not(windows))]
        let script = format!("#!/bin/sh\nsed -i 's/{from}/{to}/g' \"$1\"\n");
        write_editor_script(tmp.path(), name, &script)
    };

    let patch_plain = |w: &RecordedWrite, btype: &str| -> String {
        w.body
            .pointer(&format!("/{btype}/rich_text"))
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("missing /{btype}/rich_text in {}", w.body))
            .iter()
            .filter_map(|s| s.pointer("/text/content").and_then(Value::as_str))
            .collect()
    };

    // 1. ⭐ callout body edit -> 1 PATCH on STAR_CALLOUT_ID as callout, 0 deletes/inserts
    let ed = run_replace_edit("b09_star", "Star note", "Star note v2");
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &ed)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "star callout edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "star callout: expected 1 PATCH, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{STAR_CALLOUT_ID}"));
    assert_eq!(patch_plain(&writes[0], "callout"), "Star note v2");
    assert!(writes[0].body.pointer("/callout/icon").is_none());

    // 2. 🇰🇷 callout body edit -> 1 PATCH on FLAG_CALLOUT_ID as callout, 0 deletes/inserts
    let ed = run_replace_edit("b09_flag", "Flag note", "Flag note v2");
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &ed)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "flag callout edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "flag callout: expected 1 PATCH, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{FLAG_CALLOUT_ID}"));
    assert_eq!(patch_plain(&writes[0], "callout"), "Flag note v2");

    // 3. 🔥 quote body edit -> 1 PATCH on FIRE_QUOTE_ID as quote (NOT callout), 0 deletes/inserts
    let ed = run_replace_edit("b09_fire", "Hot take old", "Hot take new");
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &ed)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "fire quote edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "fire quote: expected 1 PATCH, got {writes:?}");
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/blocks/{FIRE_QUOTE_ID}"));
    assert_eq!(patch_plain(&writes[0], "quote"), "🔥 Hot take new");

    // 4. Control: ⭐ callout WITH child -> 1 PATCH on CHILD_CALLOUT_ID, child preserved
    let ed = run_replace_edit("b09_child", "Star parent", "Star parent v2");
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .env("EDITOR", &ed)
        .args(["pages", "edit", PAGE, "--editor"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "star parent callout edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = state.take_writes();
    assert_eq!(writes.len(), 1, "star parent callout: expected 1 PATCH, got {writes:?}");
    assert_eq!(writes[0].path, format!("/v1/blocks/{CHILD_CALLOUT_ID}"));
    assert_eq!(patch_plain(&writes[0], "callout"), "Star parent v2");
    {
        let map = state.blocks_by_parent.lock().unwrap();
        assert_eq!(map[CHILD_CALLOUT_ID].len(), 1);
        assert_eq!(map[CHILD_CALLOUT_ID][0]["id"], CHILD_CALLOUT_P_ID);
    }

    // 5. Controls: plain quote, 💡 callout, explicit toggle
    for (name, from, to, id, btype, expected) in [
        ("b09_plain_q", "Plain quote old", "Plain quote new", PLAIN_QUOTE_ID, "quote", "Plain quote new"),
        ("b09_bulb", "Bulb note old", "Bulb note new", BULB_CALLOUT_ID, "callout", "Bulb note new"),
        ("b09_toggle", "Toggle note old", "Toggle note new", TOGGLE_ID, "toggle", "Toggle note new"),
    ] {
        let ed = run_replace_edit(name, from, to);
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .env("EDITOR", &ed)
            .args(["pages", "edit", PAGE, "--editor"])
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "{name} failed: {}", String::from_utf8_lossy(&out.stderr));
        let writes = state.take_writes();
        assert_eq!(writes.len(), 1, "{name}: expected 1 PATCH, got {writes:?}");
        assert_eq!(writes[0].path, format!("/v1/blocks/{id}"));
        assert_eq!(patch_plain(&writes[0], btype), expected);
    }

    // 6. New /callout slash commands: ZWJ (👨‍💻), skin tone (👍🏽), star (⭐), flag (🇰🇷), and plain non-emoji text
    for (input, exp_icon, exp_text) in [
        ("/callout 👨‍💻 Dev", "👨‍💻", "Dev"),
        ("/callout 👍🏽 OK", "👍🏽", "OK"),
        ("/callout ⭐ Star", "⭐", "Star"),
        ("/callout 🇰🇷 Korea", "🇰🇷", "Korea"),
        ("/callout Plain text", "💡", "Plain text"),
        ("/callout 한글 노트", "💡", "한글 노트"),
    ] {
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .args(["pages", "append", PAGE, input, "--json"])
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "append `{input}` failed: {}", String::from_utf8_lossy(&out.stderr));
        let writes = state.take_writes();
        assert_eq!(writes.len(), 1);
        let blk = &writes[0].body["children"][0];
        assert_eq!(blk["type"], "callout", "input={input}");
        assert_eq!(blk.pointer("/callout/icon/emoji"), Some(&json!(exp_icon)), "input={input}: {blk}");
        let plain: String = blk
            .pointer("/callout/rich_text")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(|s| s.pointer("/text/content").and_then(Value::as_str))
            .collect();
        assert_eq!(plain, exp_text, "input={input}: {blk}");
    }

    // Explicit /toggle still produces toggle
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", PAGE, "/toggle Explicit toggle", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let writes = state.take_writes();
    assert_eq!(writes[0].body["children"][0]["type"], "toggle");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_08_hyphen_w_text_and_status_values() {
    let state = CliMockState::default();
    let app = Router::new()
        .route(
            "/v1/pages",
            post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/pages".into(),
                    body: body.clone(),
                });
                Json(json!({
                    "object": "page", "id": "page-created-b08",
                    "url": "https://www.notion.so/page-created-b08",
                    "parent": body.get("parent").cloned().unwrap_or_else(|| json!({"type":"workspace"})),
                    "archived": false,
                    "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                }))
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "page", "id": id, "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "database_id", "database_id": "28000000-0000-0000-0000-000000000003" },
                    "archived": false,
                    "properties": {
                        "Name": { "type": "title", "title": [{ "plain_text": "Row 1" }] },
                        "Status": { "type": "status", "status": { "name": "Todo" } }
                    }
                }))
            })
            .patch(|State(st): State<CliMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/pages/{id}"),
                    body: body.clone(),
                });
                Json(json!({
                    "object": "page", "id": id, "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "database_id", "database_id": "28000000-0000-0000-0000-000000000003" },
                    "archived": false,
                    "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                }))
            }),
        )
        .route(
            "/v1/databases/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "database", "id": id, "url": format!("https://www.notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "title": [{ "plain_text": "Tasks" }], "description": [],
                    "properties": {
                        "Name": { "id": "title", "type": "title", "title": {} },
                        "Status": { "id": "st", "type": "status", "status": { "options": [] } }
                    }
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "block", "id": id, "type": "paragraph", "has_children": false,
                    "paragraph": { "rich_text": [{ "type": "text", "text": { "content": "Old" }, "plain_text": "Old" }] }
                }))
            })
            .patch(|State(st): State<CliMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}"),
                    body: body.clone(),
                });
                Json(json!({
                    "object": "block", "id": id, "type": "paragraph", "has_children": false,
                    "paragraph": body.get("paragraph").cloned().unwrap_or_else(|| json!({ "rich_text": [] }))
                }))
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(|| async {
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            })
            .patch(|State(st): State<CliMockState>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "PATCH".into(),
                    path: format!("/v1/blocks/{id}/children"),
                    body: body.clone(),
                });
                let mut created = Vec::new();
                if let Some(children) = body.get("children").and_then(Value::as_array) {
                    for (i, ch) in children.iter().enumerate() {
                        let mut blk = ch.clone();
                        blk["id"] = json!(format!("blk-b08-{i}"));
                        blk["has_children"] = json!(false);
                        created.push(blk);
                    }
                }
                Json(json!({ "object": "list", "results": created }))
            }),
        )
        .route(
            "/v1/comments",
            get(|| async {
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            })
            .post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/comments".into(),
                    body: body.clone(),
                });
                Json(json!({
                    "id": "cmt-b08", "discussion_id": "disc-b08",
                    "created_time": "2026-11-21T06:00:00Z",
                    "created_by": { "id": "bot-1", "name": "Bot" },
                    "rich_text": body.get("rich_text").cloned().unwrap_or_else(|| json!([]))
                }))
            }),
        )
        .route(
            "/v1/search",
            post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/search".into(),
                    body,
                });
                Json(json!({ "object": "list", "results": [], "has_more": false, "next_cursor": Value::Null }))
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let page_uuid = "28000000-0000-0000-0000-000000000001";
    let blk_uuid = "28000000-0000-0000-0000-000000000002";
    let db_uuid = "28000000-0000-0000-0000-000000000003";
    let row_uuid = "28000000-0000-0000-0000-000000000004";

    // 1. search -weekly
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["search", "-weekly", "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "search -weekly failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/search");
    assert_eq!(reqs[0].body["query"], "-weekly");

    // 2. pages append "-work item"
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", page_uuid, "-work item", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "append '-work item' failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body.pointer("/children/0/paragraph/rich_text/0/text/content"),
        Some(&json!("-work item"))
    );

    // 3. blocks edit --text "-wip notes"
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "edit", blk_uuid, "--text", "-wip notes", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "blocks edit --text '-wip notes' failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body.pointer("/paragraph/rich_text/0/text/content"),
        Some(&json!("-wip notes"))
    );

    // 4. comments add "-waiting"
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "add", page_uuid, "-waiting", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "comments add '-waiting' failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].body.pointer("/rich_text/0/text/content"),
        Some(&json!("-waiting"))
    );

    // 5. db create-item --title "-weekly report" --status "- Blocked"
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "create-item", db_uuid, "--title", "-weekly report", "--status", "- Blocked", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "db create-item failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body.pointer("/properties/Name/title/0/text/content"), Some(&json!("-weekly report")));
    assert_eq!(reqs[0].body.pointer("/properties/Status/status/name"), Some(&json!("- Blocked")));

    // 6. db update-item --status "- Blocked"
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "update-item", row_uuid, "--status", "- Blocked", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "db update-item failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].body.pointer("/properties/Status/status/name"), Some(&json!("- Blocked")));

    // 7. pages append "-work" --file <f> -> keeps BOTH "-work" and file contents, and NEVER creates an "ork" workspace dir
    let body_file = tmp.path().join("extra.md");
    std::fs::write(&body_file, "from file").unwrap();
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", page_uuid, "-work", "--file", body_file.to_str().unwrap(), "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "append '-work' --file failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 1);
    let children = reqs[0].body["children"].as_array().unwrap();
    let texts: Vec<&str> = children
        .iter()
        .filter_map(|c| c.pointer("/paragraph/rich_text/0/text/content").and_then(Value::as_str))
        .collect();
    assert_eq!(texts, vec!["-work", "from file"], "both -work and file contents must remain: {children:?}");
    assert!(!data_dir.join("ork").exists(), "spurious 'ork' workspace directory must not be created");
    assert!(!data_dir.join("ork_item").exists(), "spurious 'ork_item' directory must not be created");
    assert!(!data_dir.join("eekly").exists(), "spurious 'eekly' directory must not be created");

    // 8. Controls: --text=val, -- separator, global -w NAME, attached -wNAME in non-text position, --help, unknown option
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "edit", blk_uuid, "--text=-wip notes", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let reqs = state.take();
    assert_eq!(reqs[0].body.pointer("/paragraph/rich_text/0/text/content"), Some(&json!("-wip notes")));

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["search", "--remote", "--json", "--", "-weekly"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "search -- -weekly failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs[0].body["query"], "-weekly");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["-w", "teamws", "search", "-weekly", "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "global -w NAME failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs[0].body["query"], "-weekly");
    assert!(data_dir.join("teamws").exists(), "explicit -w teamws must use teamws workspace");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["-waltws", "search", "hello", "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "non-text attached -wNAME failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs[0].body["query"], "hello");
    assert!(data_dir.join("altws").exists(), "non-text -waltws must still select altws workspace");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["search", "hello", "-wpostws", "--remote", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "post-text attached -wNAME failed: {}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs[0].body["query"], "hello");
    assert!(data_dir.join("postws").exists(), "post-text -wpostws must still select postws workspace");

    let help_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["search", "-weekly", "--help"])
        .output()
        .await
        .unwrap();
    assert!(help_out.status.success());
    assert!(String::from_utf8_lossy(&help_out.stdout).contains("Usage:"));
    assert!(state.take().is_empty());

    let bad_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["search", "--unknown-flag"])
        .output()
        .await
        .unwrap();
    assert!(!bad_out.status.success());
    assert!(state.take().is_empty());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_pages_create_chunks_over_100_blocks_and_single_post_under_100() {
    let state = CliMockState::default();
    let app = Router::new()
        .route(
            "/v1/pages",
            post(|State(st): State<CliMockState>, Json(body): Json<Value>| async move {
                let count = body
                    .get("children")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0);
                if count > 100 {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "object": "error",
                            "status": 400,
                            "code": "validation_error",
                            "message": "body.children.length should be <= 100"
                        })),
                    )
                        .into_response();
                }
                st.requests.lock().unwrap().push(RecordedWrite {
                    method: "POST".into(),
                    path: "/v1/pages".into(),
                    body: body.clone(),
                });
                Json(json!({
                    "object": "page",
                    "id": "50000000-0000-0000-0000-000000000105",
                    "url": "https://www.notion.so/50000000000000000000000000000105",
                    "parent": body.get("parent").cloned().unwrap_or_else(|| json!({"type":"workspace"})),
                    "archived": false,
                    "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            patch(
                |State(st): State<CliMockState>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.requests.lock().unwrap().push(RecordedWrite {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}/children"),
                        body: body.clone(),
                    });
                    let mut created = Vec::new();
                    if let Some(children) = body.get("children").and_then(Value::as_array) {
                        for (i, ch) in children.iter().enumerate() {
                            let mut blk = ch.clone();
                            blk["id"] = json!(format!("blk-overflow-{i}"));
                            blk["has_children"] = json!(false);
                            created.push(blk);
                        }
                    }
                    Json(json!({ "object": "list", "results": created }))
                },
            ),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let parent_uuid = "50000000-0000-0000-0000-000000000001";

    // 105-block file -> POST(100) + PATCH(5)
    let file_105 = tmp.path().join("105_blocks.md");
    let content_105: String = (1..=105)
        .map(|i| format!("Line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&file_105, &content_105).unwrap();

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "create",
            "--parent",
            parent_uuid,
            "--title",
            "105 Blocks Page",
            "--file",
            file_105.to_str().unwrap(),
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let reqs = state.take();
    assert_eq!(reqs.len(), 2, "expected POST(100) + PATCH(5), got: {reqs:?}");
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path, "/v1/pages");
    assert_eq!(reqs[0].body["children"].as_array().unwrap().len(), 100);
    assert_eq!(reqs[1].method, "PATCH");
    assert_eq!(
        reqs[1].path,
        "/v1/blocks/50000000-0000-0000-0000-000000000105/children"
    );
    assert_eq!(reqs[1].body["children"].as_array().unwrap().len(), 5);

    // Control: 13-block file -> single POST(13)
    let file_13 = tmp.path().join("13_blocks.md");
    let content_13: String = (1..=13)
        .map(|i| format!("Short line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&file_13, &content_13).unwrap();

    let out13 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "create",
            "--parent",
            parent_uuid,
            "--title",
            "13 Blocks Page",
            "--file",
            file_13.to_str().unwrap(),
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(out13.status.success(), "{}", String::from_utf8_lossy(&out13.stderr));
    let reqs13 = state.take();
    assert_eq!(reqs13.len(), 1);
    assert_eq!(reqs13[0].method, "POST");
    assert_eq!(reqs13[0].body["children"].as_array().unwrap().len(), 13);

    server.abort();
}
