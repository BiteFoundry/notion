use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::config::{AppConfig, WorkspaceProfile};

use crate::audit_mock::{audit_error, audit_limit_violation};

#[derive(Clone)]
pub(crate) struct MockNotionState {
    pub(crate) base_url: String,
    pub(crate) expected_token: String,
    pub(crate) rate_limit_hits: Arc<AtomicUsize>,
    pub(crate) pages: Arc<Mutex<Vec<Value>>>,
    pub(crate) databases: Arc<Mutex<Vec<Value>>>,
    pub(crate) blocks_by_parent: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    pub(crate) comments_by_page: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    pub(crate) todo_checked: Arc<Mutex<bool>>,
}

impl MockNotionState {
    pub(crate) fn seeded(base_url: String) -> Self {
        let mut blocks_map = std::collections::HashMap::new();
        blocks_map.insert(
            "page-roadmap".to_string(),
            vec![
                json!({
                    "id": "blk-h1",
                    "type": "heading_1",
                    "has_children": false,
                    "heading_1": { "rich_text": [{ "plain_text": "Q4 Launch Milestones" }] }
                }),
                json!({
                    "id": "blk-callout",
                    "type": "callout",
                    "has_children": false,
                    "callout": {
                        "icon": { "type": "emoji", "emoji": "💡" },
                        "color": "blue_background",
                        "rich_text": [{ "plain_text": "Keyboard-first Notion in the terminal!" }]
                    }
                }),
                json!({
                    "id": "blk-todo",
                    "type": "to_do",
                    "has_children": false,
                    "to_do": {
                        "checked": false,
                        "rich_text": [{ "plain_text": "Ship @bitefoundry/notion v0.1.0" }]
                    }
                }),
                json!({
                    "id": "blk-toggle",
                    "type": "toggle",
                    "has_children": true,
                    "toggle": {
                        "rich_text": [{ "plain_text": "Architecture Details" }]
                    }
                }),
                json!({
                    "id": "blk-code",
                    "type": "code",
                    "has_children": false,
                    "code": {
                        "language": "rust",
                        "rich_text": [{ "plain_text": "fn main() { println!(\"notion\"); }" }]
                    }
                }),
                json!({
                    "id": "blk-table",
                    "type": "table",
                    "has_children": true,
                    "table": { "table_width": 2, "has_column_header": true, "has_row_header": false }
                }),
                json!({
                    "id": "blk-img",
                    "type": "image",
                    "has_children": false,
                    "image": {
                        "type": "external",
                        "external": { "url": format!("{base_url}/media/sample.png") },
                        "caption": [{ "plain_text": "Architecture Diagram" }]
                    }
                }),
            ],
        );
        blocks_map.insert(
            "blk-toggle".to_string(),
            vec![json!({
                "id": "blk-toggle-child",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "SQLite WAL + FTS5 with Korean fallback" }] }
            })],
        );
        blocks_map.insert(
            "blk-table".to_string(),
            vec![
                json!({
                    "id": "blk-tr-0",
                    "type": "table_row",
                    "has_children": false,
                    "table_row": { "cells": [[{ "plain_text": "Feature" }], [{ "plain_text": "Status" }]] }
                }),
                json!({
                    "id": "blk-tr-1",
                    "type": "table_row",
                    "has_children": false,
                    "table_row": { "cells": [[{ "plain_text": "Kanban Board" }], [{ "plain_text": "Ready" }]] }
                }),
            ],
        );

        let mut comments_map = std::collections::HashMap::new();
        comments_map.insert(
            "page-roadmap".to_string(),
            vec![json!({
                "id": "cmt-1",
                "discussion_id": "disc-1",
                "created_time": "2026-11-20T01:00:00Z",
                "created_by": { "id": "u-1", "name": "Alice" },
                "rich_text": [{ "plain_text": "Looks awesome!" }]
            })],
        );

        Self {
            base_url,
            expected_token: "ntn_mock_token_123".to_string(),
            rate_limit_hits: Arc::new(AtomicUsize::new(0)),
            pages: Arc::new(Mutex::new(vec![
                json!({
                    "object": "page",
                    "id": "page-roadmap",
                    "url": "https://www.notion.so/page-roadmap",
                    "icon": { "type": "emoji", "emoji": "🚀" },
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "last_edited_time": "2026-11-20T02:00:00Z",
                    "properties": {
                        "title": { "type": "title", "title": [{ "plain_text": "Product Roadmap" }] },
                        "Status": { "type": "status", "status": { "name": "In Progress", "color": "blue" } }
                    }
                }),
                json!({
                    "object": "page",
                    "id": "page-ko",
                    "url": "https://www.notion.so/page-ko",
                    "icon": { "type": "emoji", "emoji": "📝" },
                    "parent": { "type": "page_id", "page_id": "page-roadmap" },
                    "archived": false,
                    "last_edited_time": "2026-11-20T02:05:00Z",
                    "properties": {
                        "title": { "type": "title", "title": [{ "plain_text": "주간 회의록" }] },
                        "Tags": { "type": "multi_select", "multi_select": [{ "name": "기획", "color": "purple" }] }
                    }
                }),
                json!({
                    "object": "page",
                    "id": "row-1",
                    "url": "https://www.notion.so/row-1",
                    "icon": { "type": "emoji", "emoji": "✅" },
                    "parent": { "type": "database_id", "database_id": "db-tasks" },
                    "archived": false,
                    "last_edited_time": "2026-11-20T02:10:00Z",
                    "properties": {
                        "Name": { "type": "title", "title": [{ "plain_text": "Build Kanban View" }] },
                        "Status": { "type": "status", "status": { "name": "In Progress", "color": "blue" } },
                        "Done": { "type": "checkbox", "checkbox": false }
                    }
                }),
                json!({
                    "object": "page",
                    "id": "row-2",
                    "url": "https://www.notion.so/row-2",
                    "icon": { "type": "emoji", "emoji": "📦" },
                    "parent": { "type": "database_id", "database_id": "db-tasks" },
                    "archived": false,
                    "last_edited_time": "2026-11-20T02:15:00Z",
                    "properties": {
                        "Name": { "type": "title", "title": [{ "plain_text": "Publish npm binaries" }] },
                        "Status": { "type": "status", "status": { "name": "Done", "color": "green" } },
                        "Done": { "type": "checkbox", "checkbox": true }
                    }
                }),
            ])),
            databases: Arc::new(Mutex::new(vec![json!({
                "object": "database",
                "id": "db-tasks",
                "url": "https://www.notion.so/db-tasks",
                "icon": { "type": "emoji", "emoji": "🗂" },
                "parent": { "type": "workspace", "workspace": true },
                "title": [{ "plain_text": "Sprint Tasks" }],
                "description": [{ "plain_text": "Engineering task tracker" }],
                "last_edited_time": "2026-11-20T02:15:00Z",
                "properties": {
                    "Name": { "id": "title", "type": "title", "title": {} },
                    "Status": {
                        "id": "st",
                        "type": "status",
                        "status": {
                            "options": [
                                { "id": "1", "name": "Todo", "color": "gray" },
                                { "id": "2", "name": "In Progress", "color": "blue" },
                                { "id": "3", "name": "Done", "color": "green" }
                            ]
                        }
                    },
                    "Done": { "id": "dn", "type": "checkbox", "checkbox": {} }
                }
            })])),
            blocks_by_parent: Arc::new(Mutex::new(blocks_map)),
            comments_by_page: Arc::new(Mutex::new(comments_map)),
            todo_checked: Arc::new(Mutex::new(false)),
        }
    }
}

pub(crate) fn check_notion_headers(headers: &HeaderMap, expected_token: &str) -> Result<(), (StatusCode, Json<Value>)> {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ver = headers
        .get("Notion-Version")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if auth != format!("Bearer {expected_token}") || ver != "2022-06-28" {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "unauthorized", "message": "Invalid Notion credentials or version" })),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct CommentQuery {
    pub(crate) block_id: String,
}

pub(crate) async fn start_mock_notion_server() -> (String, MockNotionState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let state = MockNotionState::seeded(base_url.clone());
    let state_ret = state.clone();

    // 1x1 red PNG
    let sample_png: Vec<u8> = {
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 64, 64, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    };

    let app = Router::new()
        .route(
            "/v1/users/me",
            get(|State(st): State<MockNotionState>, headers: HeaderMap| async move {
                if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                    return e.into_response();
                }
                // Exercise HTTP 429 Retry-After on very first call!
                if st.rate_limit_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "0")],
                        Json(json!({ "code": "rate_limited", "message": "Slow down" })),
                    )
                        .into_response();
                }
                Json(json!({
                    "object": "user",
                    "id": "bot-mock-1",
                    "name": "Notion Mock Bot",
                    "type": "bot",
                    "bot": { "workspace_name": "Acme Workspace" }
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/search",
            post(
                |State(st): State<MockNotionState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let cursor = body.get("start_cursor").and_then(Value::as_str);
                    if cursor == Some("page-2") {
                        let dbs = st.databases.lock().unwrap().clone();
                        return Json(json!({
                            "object": "list",
                            "results": dbs,
                            "has_more": false,
                            "next_cursor": Value::Null
                        }))
                        .into_response();
                    }
                    let pages = st.pages.lock().unwrap().clone();
                    Json(json!({
                        "object": "list",
                        "results": pages,
                        "has_more": true,
                        "next_cursor": "page-2"
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1/pages",
            post(
                |State(st): State<MockNotionState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let mut pages = st.pages.lock().unwrap();
                    let new_id = format!("page-created-{}", pages.len() + 1);
                    let page = json!({
                        "object": "page",
                        "id": new_id,
                        "url": format!("https://www.notion.so/{new_id}"),
                        "parent": body.get("parent").cloned().unwrap_or_else(|| json!({"type":"workspace"})),
                        "icon": body.get("icon").cloned().unwrap_or(Value::Null),
                        "archived": false,
                        "last_edited_time": "2026-11-20T02:20:00Z",
                        "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                    });
                    pages.push(page.clone());
                    Json(page).into_response()
                },
            ),
        )
        .route(
            "/v1/pages/{id}",
            get(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let pages = st.pages.lock().unwrap();
                    if let Some(p) = pages.iter().find(|p| p["id"] == id) {
                        return Json(p.clone()).into_response();
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
                },
            )
            .patch(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let mut pages = st.pages.lock().unwrap();
                    if let Some(p) = pages.iter_mut().find(|p| p["id"] == id) {
                        if let Some(arch) = body.get("archived").and_then(Value::as_bool) {
                            p["archived"] = json!(arch);
                        }
                        if let Some(props) = body.get("properties").and_then(Value::as_object) {
                            for (k, v) in props {
                                p["properties"][k] = v.clone();
                            }
                        }
                        return Json(p.clone()).into_response();
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}",
            get(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    if id == "blk-todo" {
                        let checked = *st.todo_checked.lock().unwrap();
                        return Json(json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "has_children": false,
                            "to_do": {
                                "checked": checked,
                                "rich_text": [{ "plain_text": "Ship @bitefoundry/notion v0.1.0" }]
                            }
                        }))
                        .into_response();
                    }
                    if id == "blk-img" {
                        return Json(json!({
                            "id": "blk-img",
                            "type": "image",
                            "has_children": false,
                            "image": {
                                "type": "external",
                                "external": { "url": format!("{}/media/sample.png", st.base_url) },
                                "caption": [{ "plain_text": "Architecture Diagram" }]
                            }
                        }))
                        .into_response();
                    }
                    Json(json!({
                        "id": id,
                        "type": "paragraph",
                        "has_children": false,
                        "paragraph": { "rich_text": [{ "plain_text": "Paragraph text" }] }
                    }))
                    .into_response()
                },
            )
            .patch(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    if id == "blk-todo" {
                        if let Some(c) = body.pointer("/to_do/checked").and_then(Value::as_bool) {
                            *st.todo_checked.lock().unwrap() = c;
                        }
                        let checked = *st.todo_checked.lock().unwrap();
                        let text = body
                            .pointer("/to_do/rich_text/0/text/content")
                            .and_then(Value::as_str)
                            .unwrap_or("Ship @bitefoundry/notion v0.1.0");
                        return Json(json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "has_children": false,
                            "to_do": {
                                "checked": checked,
                                "rich_text": [{ "plain_text": text }]
                            }
                        }))
                        .into_response();
                    }
                    Json(json!({
                        "id": id,
                        "type": "paragraph",
                        "has_children": false,
                        "paragraph": body.get("paragraph").cloned().unwrap_or_else(|| json!({ "rich_text": [] }))
                    }))
                    .into_response()
                },
            )
            .delete(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    Json(json!({ "id": id, "archived": true })).into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
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
                    .into_response()
                },
            )
            .patch(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
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
                    Json(json!({ "object": "list", "results": created })).into_response()
                },
            ),
        )
        .route(
            "/v1/databases/{id}",
            get(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let dbs = st.databases.lock().unwrap();
                    if let Some(d) = dbs.iter().find(|d| d["id"] == id) {
                        return Json(d.clone()).into_response();
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
                },
            ),
        )
        .route(
            "/v1/databases/{id}/query",
            post(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 AxumPath(id): AxumPath<String>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let rows: Vec<Value> = st
                        .pages
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|p| p.pointer("/parent/database_id").and_then(Value::as_str) == Some(id.as_str()))
                        .cloned()
                        .collect();
                    Json(json!({
                        "object": "list",
                        "results": rows,
                        "has_more": false,
                        "next_cursor": Value::Null
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1/comments",
            get(
                |State(st): State<MockNotionState>,
                 headers: HeaderMap,
                 Query(q): Query<CommentQuery>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let list = st
                        .comments_by_page
                        .lock()
                        .unwrap()
                        .get(&q.block_id)
                        .cloned()
                        .unwrap_or_default();
                    Json(json!({
                        "object": "list",
                        "results": list,
                        "has_more": false,
                        "next_cursor": Value::Null
                    }))
                    .into_response()
                },
            )
            .post(
                |State(st): State<MockNotionState>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    if let Err(e) = check_notion_headers(&headers, &st.expected_token) {
                        return e.into_response();
                    }
                    let pid = body
                        .pointer("/parent/page_id")
                        .and_then(Value::as_str)
                        .unwrap_or("page-roadmap")
                        .to_string();
                    let mut map = st.comments_by_page.lock().unwrap();
                    let list = map.entry(pid).or_default();
                    let cmt = json!({
                        "id": format!("cmt-{}", list.len() + 1),
                        "discussion_id": body.get("discussion_id").and_then(Value::as_str).unwrap_or("disc-1"),
                        "created_time": "2026-11-20T02:25:00Z",
                        "created_by": { "id": "bot-mock-1", "name": "Notion Mock Bot" },
                        "rich_text": body.get("rich_text").cloned().unwrap_or_else(|| json!([]))
                    });
                    list.push(cmt.clone());
                    Json(cmt).into_response()
                },
            ),
        )
        .route(
            "/media/sample.png",
            get(move || {
                let bytes = sample_png.clone();
                async move {
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "image/png")],
                        bytes,
                    )
                }
            }),
        )
        .with_state(state)
        .layer(axum::middleware::from_fn(reject_oversized_writes));

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (base_url, state_ret, handle)
}

/// Rejects write bodies that break Notion's request limits, as the real API does.
pub(crate) async fn reject_oversized_writes(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path().to_string();
    let is_write = matches!(request.method().as_str(), "POST" | "PATCH")
        && !path.ends_with("/query")
        && path != "/v1/search";
    if !is_write {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if let Some(violation) = audit_limit_violation(&json) {
        return audit_error(
            StatusCode::BAD_REQUEST,
            "validation_error",
            format!("body failed validation: {violation}"),
        );
    }
    next.run(axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

pub(crate) fn test_config(base_url: &str) -> AppConfig {
    let mut cfg = AppConfig {
        api_base_url: format!("{base_url}/v1"),
        ..Default::default()
    };
    cfg.upsert_workspace(
        WorkspaceProfile {
            name: "acme".into(),
            workspace_id: "acme".into(),
            workspace_name: "Acme Workspace".into(),
            workspace_icon: Some("🚀".into()),
            bot_id: Some("bot-mock-1".into()),
            bot_name: Some("Notion Mock Bot".into()),
            auth_mode: "integration".into(),
            api_base_url: Some(format!("{base_url}/v1")),
        },
        true,
    );
    cfg
}
