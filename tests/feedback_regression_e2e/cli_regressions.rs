use std::{
    collections::HashMap,
    sync::{atomic::AtomicBool, Arc, Mutex},
};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::harness::{
    extract_recovery_path, notion_cli_cmd, paragraph_json, write_editor_script, CliMockState,
    ColumnMockState, RecordedWrite,
};

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

