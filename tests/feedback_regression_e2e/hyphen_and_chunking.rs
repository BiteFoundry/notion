use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, patch, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::harness::{notion_cli_cmd, CliMockState, RecordedWrite};

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
