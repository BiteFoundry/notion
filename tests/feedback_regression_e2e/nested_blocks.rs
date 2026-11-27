use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    config::{AppConfig, WorkspaceProfile},
    notion::NotionClient,
    storage::Storage,
};

use crate::harness::{
    extract_recovery_path, notion_cli_cmd, paragraph_json, write_editor_script, ColumnMockState,
    RecordedWrite,
};

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
    let mut cfg = AppConfig {
        api_base_url: format!("{base_url}/v1"),
        ..Default::default()
    };
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

