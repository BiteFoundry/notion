//! End-to-end integration test suite against a local Mock Axum Notion REST API v1 server.
//!
//! Verifies:
//! 1. Notion REST API client (users/me, search with pagination, pages CRUD, recursive blocks,
//!    to_do toggle PATCH, database query + item create/update, comments, 429 Retry-After, media download)
//! 2. SQLite WAL + FTS5 cache + Korean `<3` char LIKE fallback
//! 3. Local HTTP API server (`src/serve.rs`, `:8791` contract + API key auth)
//! 4. Stdio MCP JSON-RPC 2.0 server (`src/mcp.rs`, tool catalog and `archive_page` confirm guard)
//! 5. Notion-Native TUI rendering (`TestBackend`: Breadcrumb bar, Sidebar, Page Canvas, Callout,
//!    Toggle expand/collapse, To-do, Code, Unicode Table, Comments Drawer, Database Table/Board/List).

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
use ratatui::{backend::TestBackend, Terminal};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    config::{AppConfig, PageWidthMode, WorkspaceProfile},
    mcp,
    models::DatabaseViewMode,
    notion::{media, NotionClient},
    serve,
    storage::Storage,
    sync,
    ui::{
        self, action,
        action::Action,
        app::{AppState, CanvasView, Focus, UiState},
    },
};

#[derive(Clone)]
struct MockNotionState {
    base_url: String,
    expected_token: String,
    rate_limit_hits: Arc<AtomicUsize>,
    pages: Arc<Mutex<Vec<Value>>>,
    databases: Arc<Mutex<Vec<Value>>>,
    blocks_by_parent: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    comments_by_page: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    todo_checked: Arc<Mutex<bool>>,
}

impl MockNotionState {
    fn seeded(base_url: String) -> Self {
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

fn check_notion_headers(headers: &HeaderMap, expected_token: &str) -> Result<(), (StatusCode, Json<Value>)> {
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
struct CommentQuery {
    block_id: String,
}

async fn start_mock_notion_server() -> (String, MockNotionState, tokio::task::JoinHandle<()>) {
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
        .with_state(state);

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (base_url, state_ret, handle)
}

fn test_config(base_url: &str) -> AppConfig {
    let mut cfg = AppConfig::default();
    cfg.api_base_url = format!("{base_url}/v1");
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

#[tokio::test]
async fn e2e_notion_client_search_recursive_blocks_todo_db_comments_and_media() {
    let (base_url, state, server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    // 1. GET /v1/users/me (automatically retries HTTP 429!)
    let me = client.get_me().await.unwrap();
    assert_eq!(me.name, "Notion Mock Bot");
    assert_eq!(state.rate_limit_hits.load(Ordering::SeqCst), 2);

    // 2. Full workspace sync (cursor pagination across pages + databases)
    let (ws_id, pages, dbs) = sync::sync_workspace_tree(&client, &storage).await.unwrap();
    assert_eq!(pages.len(), 4);
    assert_eq!(dbs.len(), 1);

    // 3. Recursive block tree fetch (toggle children + table rows + comments)
    let detail = sync::sync_page_detail(&client, &storage, "page-roadmap")
        .await
        .unwrap();
    assert_eq!(detail.blocks.len(), 7);
    let toggle = detail.blocks.iter().find(|b| b.id == "blk-toggle").unwrap();
    assert_eq!(toggle.children.len(), 1);
    assert!(toggle.children[0].plain_text().contains("Korean fallback"));
    let table = detail.blocks.iter().find(|b| b.id == "blk-table").unwrap();
    assert_eq!(table.children.len(), 2);
    assert_eq!(detail.comments.len(), 1);
    assert_eq!(detail.media.len(), 1);

    // 4. SQLite FTS5 + Korean <3 char LIKE fallback
    let fts_en = storage.search_local(&ws_id, "Milestones", 10).unwrap();
    assert_eq!(fts_en.len(), 1);
    assert_eq!(fts_en[0].id, "page-roadmap");

    let ko_short = storage.search_local(&ws_id, "회의", 10).unwrap();
    assert_eq!(ko_short.len(), 1);
    assert_eq!(ko_short[0].id, "page-ko");

    let ko_1 = storage.search_local(&ws_id, "기", 10).unwrap();
    assert_eq!(ko_1.len(), 1);
    assert_eq!(ko_1[0].id, "page-ko");

    // 5. Toggle to_do checkbox via PATCH /v1/blocks/{id}
    let toggled = notion_cli::cli::op_toggle_todo(&client, &storage, "blk-todo", None)
        .await
        .unwrap();
    assert!(matches!(
        toggled.kind,
        notion_cli::models::NotionBlockKind::ToDo { checked: true, .. }
    ));

    // 6. Database query + item create + update
    let rows = notion_cli::cli::op_query_database(
        &client, &storage, "db-tasks", None, None, 20, None, false,
    )
    .await
    .unwrap()
    .items;
    assert_eq!(rows.len(), 2);

    let created = notion_cli::cli::op_create_database_item(
        &client,
        &storage,
        "db-tasks",
        "Write E2E Tests",
        Some("Todo"),
        None,
        Some("- [ ] Cover all endpoints"),
    )
    .await
    .unwrap();
    assert_eq!(created.title, "Write E2E Tests");

    // 7. Media download helper (by block ID)
    let tmp = tempfile::tempdir().unwrap();
    let dl = media::download_media_target(&cfg, &client, "blk-img", Some(tmp.path()))
        .await
        .unwrap();
    assert!(dl.ok);
    assert!(dl.is_image);
    assert_eq!(dl.mime_type, "image/png");
    assert!(dl.size_bytes > 0);
    assert!(std::path::Path::new(&dl.path).exists());

    server.abort();
}

#[tokio::test]
async fn e2e_http_api_server_and_mcp_tools() {
    let (base_url, _state, mock_server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    // --- Part A: HTTP API Server (`serve.rs`) ---
    let api_state = Arc::new(serve::ApiState {
        config: cfg.clone(),
        auth: auth.clone(),
        client: client.clone(),
        storage: storage.clone(),
        api_key: Some("local-secret".into()),
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_base = format!("http://{api_addr}");
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });

    let http = reqwest::Client::new();
    // Health endpoint requires no auth
    let health: Value = http
        .get(format!("{api_base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["service"], "notion-cli");

    // Protected route rejects missing API key
    let unauth = http
        .get(format!("{api_base}/api/v1/whoami"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Whoami returns API workspace_name rather than placeholder profile name (W9)
    let whoami_resp: Value = http
        .get(format!("{api_base}/api/v1/whoami"))
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(whoami_resp["workspace_name"], "Acme Workspace");
    assert_eq!(whoami_resp["user"]["workspace_name"], "Acme Workspace");

    // Upstream 401 from Notion API propagates as HTTP 401 from `serve` (W3 F2e)
    let bad_auth = AuthManager::from_token(cfg.clone(), "ntn_wrong_token");
    let bad_client = NotionClient::new(&cfg, bad_auth.clone()).unwrap();
    let bad_api_state = Arc::new(serve::ApiState {
        config: cfg.clone(),
        auth: bad_auth,
        client: bad_client,
        storage: Storage::open_in_memory().unwrap(),
        api_key: None,
    });
    let bad_api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad_api_addr = bad_api_listener.local_addr().unwrap();
    let bad_api_server = tokio::spawn(async move {
        let _ = axum::serve(bad_api_listener, serve::build_router(bad_api_state)).await;
    });
    let upstream_401 = http
        .get(format!("http://{bad_api_addr}/api/v1/whoami"))
        .send()
        .await
        .unwrap();
    assert_eq!(upstream_401.status(), StatusCode::UNAUTHORIZED);
    bad_api_server.abort();

    // Sync via HTTP API
    let sync_resp: Value = http
        .post(format!("{api_base}/api/v1/sync"))
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sync_resp["ok"], true);
    assert_eq!(sync_resp["pages_count"], 4);

    // Korean <3 char search via HTTP API
    let search_ko: Value = http
        .get(format!("{api_base}/api/v1/search?q=회의"))
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search_ko["count"], 1);

    // Comments POST + GET via HTTP API
    let add_cmt: Value = http
        .post(format!("{api_base}/api/v1/pages/page-roadmap/comments"))
        .header("x-api-key", "local-secret")
        .json(&json!({ "text": "HTTP comment works!" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(add_cmt["ok"], true);

    // Database query & item creation via HTTP API
    let db_q: Value = http
        .post(format!("{api_base}/api/v1/databases/db-tasks/query"))
        .header("x-api-key", "local-secret")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(db_q["count"], 2);

    api_server.abort();

    // --- Part B: MCP JSON-RPC 2.0 server ---
    let init = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }),
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "notion-cli");

    let list_tools = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await
    .unwrap();
    assert_eq!(list_tools["result"]["tools"].as_array().unwrap().len(), 20);

    // Call archive_page WITHOUT confirm -> must fail!
    let archive_denied = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "archive_page", "arguments": { "page": "page-ko", "confirm": false } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(archive_denied["result"]["isError"], true);

    // Call archive_page WITH confirm: true -> succeeds!
    let archive_ok = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "archive_page", "arguments": { "page": "page-ko", "confirm": true } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(archive_ok["result"]["isError"], false);

    // Call append_blocks + toggle_todo + list_databases + add_comment via MCP
    let append_ok = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "append_blocks", "arguments": { "page": "page-roadmap", "markdown": "/callout 🔥 Shipped!" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(append_ok["result"]["isError"], false);
    assert_eq!(append_ok["result"]["structuredContent"]["appended"], 1);
    assert!(append_ok["result"]["structuredContent"].get("blocks").is_none());
    assert_eq!(
        append_ok["result"]["structuredContent"]["block_ids"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    mock_server.abort();
}

#[tokio::test]
async fn e2e_tui_renders_notion_page_canvas_and_database_table_board_list() {
    let (base_url, _state, mock_server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    sync::sync_page_detail(&client, &storage, "page-roadmap")
        .await
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui_state = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    app.load_cached_initial();
    app.open_page("page-roadmap", &mut ui_state, false);
    ui_state.focus = Focus::Canvas;
    ui_state.comments_drawer_open = true;

    let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

    // 1. Render Page Canvas (Centered + Comments Drawer + Callout + Toggle + Todo + Code + Table)
    assert_eq!(ui_state.page_width, PageWidthMode::Centered);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();
    let screen1: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen1.contains("Product Roadmap"));
    assert!(screen1.contains("Width: Centered"));
    assert!(screen1.contains("In Progress"));
    assert!(screen1.contains("Q4 Launch Milestones"));
    assert!(screen1.contains("Architecture Details"));
    assert!(screen1.contains("Comments"));
    assert!(screen1.contains("Looks awesome!"));

    // 2. Expand Toggle block live and verify nested child appears!
    ui_state.selected_block = 3; // blk-toggle
    action::dispatch(Action::ToggleActiveBlock, &mut app, &mut ui_state);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();
    let screen_toggle: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(screen_toggle.contains("SQLite WAL + FTS5"));

    // 3. Switch to Database View -> Table, Board (Kanban), and List
    app.open_database("db-tasks", &mut ui_state, true);
    assert_eq!(app.active_view, CanvasView::Database);
    ui_state.comments_drawer_open = false;

    for mode in [
        DatabaseViewMode::Table,
        DatabaseViewMode::Board,
        DatabaseViewMode::List,
    ] {
        ui_state.db_view_mode = mode;
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui_state);
            })
            .unwrap();
        let db_screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(db_screen.contains("Sprint Tasks"));
        assert!(db_screen.contains("[1 Table]"));
        assert!(db_screen.contains("[2 Board]"));
        assert!(db_screen.contains("[3 List]"));
        assert!(db_screen.contains("Build Kanban View"));
    }

    mock_server.abort();
}

#[tokio::test]
async fn e2e_tui_mouse_scroll_clamping_hitmap_and_sidebar_toggle() {
    let (base_url, _state, mock_server) = start_mock_notion_server().await;
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    sync::sync_page_detail(&client, &storage, "page-roadmap")
        .await
        .unwrap();

    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(sync::FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = sync::SyncController::from_channels(focus_tx, cmd_tx);

    let mut ui_state = UiState::from_config(&cfg);
    let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
    app.load_cached_initial();
    app.open_page("page-roadmap", &mut ui_state, false);
    ui_state.focus = Focus::Canvas;
    ui_state.comments_drawer_open = true;

    let mut terminal = Terminal::new(TestBackend::new(140, 20)).unwrap();
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();

    // 1. Clicking sidebar arrow on collapsible `page-roadmap` (parent of `page-ko`) collapses it
    // without moving focus away from Sidebar.
    let items = app.build_sidebar_items(&ui_state);
    let roadmap_idx = items
        .iter()
        .position(|i| i.id == "page-roadmap")
        .expect("page-roadmap should be in sidebar");
    let toggle_rect = ui_state
        .hitmap
        .region_of(ui::hitmap::HitTarget::SidebarToggle(roadmap_idx))
        .expect("page-roadmap should have a clickable SidebarToggle region");
    let toggle_hit = ui_state.hitmap.hit(toggle_rect.y, toggle_rect.x + 1).unwrap();
    assert_eq!(
        toggle_hit,
        ui::hitmap::HitTarget::SidebarToggle(roadmap_idx)
    );
    action::dispatch(
        Action::MouseClick {
            target: toggle_hit,
            col: toggle_rect.x + 1,
            row: toggle_rect.y,
        },
        &mut app,
        &mut ui_state,
    );
    assert_eq!(ui_state.focus, Focus::Sidebar);
    assert!(ui_state.collapsed_nodes.contains("page-roadmap"));

    // 2. ScrollBottom clamps on draw so MouseScroll up immediately responds
    action::dispatch(Action::ScrollBottom, &mut app, &mut ui_state);
    terminal
        .draw(|f| {
            ui::draw(f, &mut app, &mut ui_state);
        })
        .unwrap();
    let max_scroll = ui_state.canvas_scroll;
    assert!(max_scroll < 100);
    if max_scroll > 0 {
        action::dispatch(
            Action::MouseScroll {
                delta: -1,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui_state,
        );
        assert_eq!(ui_state.canvas_scroll, max_scroll - 1);
    }

    // 3. Clicking Comments Drawer close hint closes the drawer
    let close_rect = ui_state
        .hitmap
        .region_of(ui::hitmap::HitTarget::Button(
            ui::hitmap::ButtonId::ToggleComments,
        ))
        .expect("comments close hint should be mapped");
    action::dispatch(
        Action::MouseClick {
            target: ui::hitmap::HitTarget::Button(ui::hitmap::ButtonId::ToggleComments),
            col: close_rect.x,
            row: close_rect.y,
        },
        &mut app,
        &mut ui_state,
    );
    assert!(!ui_state.comments_drawer_open);

    mock_server.abort();
}

#[tokio::test]
async fn e2e_bidirectional_sync_versions_forced_refresh_block_writes_and_section4_followups() {
    use std::sync::{Arc, Mutex};
    use axum::{
        extract::{Path as AxumPath, Query as AxumQuery, State},
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use notion_cli::cli;
    use notion_cli::models::{CoreEvent, NotionBlockKind};
    use notion_cli::sync::{DetailSyncMode, FocusTarget, SyncCommand, SyncController};
    use notion_cli::ui::action::{self, Action};
    use notion_cli::ui::app::{AppState, UiMsg, UiState};
    use serde_json::{json, Value};

    #[derive(Default)]
    struct SyncMockState {
        page_ts: String,
        para_text: String,
        toggle_child_text: String,
        todo_checked: bool,
        del_archived: bool,
        comments: Vec<Value>,
        beta_archived: bool,
        search_omit_valid_page: bool,
        deleted_404_missing_from_search: bool,
        ambiguous_500_missing_from_search: bool,
        row2_archived: bool,
        fail_toggle_children: bool,
        search_two_pages_same_ts: bool,
        hits: Vec<String>,
    }

    let state = Arc::new(Mutex::new(SyncMockState {
        page_ts: "2026-11-20T02:00:00.000Z".into(),
        para_text: "Body V2".into(),
        toggle_child_text: "Nested V2".into(),
        todo_checked: false,
        del_archived: false,
        comments: vec![json!({
            "id": "cmt-1",
            "discussion_id": "disc-1",
            "created_time": "2026-11-20T02:00:00.000Z",
            "created_by": { "id": "u-1", "name": "Alice" },
            "rich_text": [{ "plain_text": "Comment V2" }]
        })],
        beta_archived: false,
        search_omit_valid_page: false,
        deleted_404_missing_from_search: false,
        ambiguous_500_missing_from_search: false,
        row2_archived: false,
        fail_toggle_children: false,
        search_two_pages_same_ts: false,
        hits: Vec::new(),
    }));

    let mk_page = |id: &str, title: &str, ts: &str, parent: Value| {
        json!({
            "object": "page",
            "id": id,
            "url": format!("https://notion.so/{id}"),
            "parent": parent,
            "archived": false,
            "last_edited_time": ts,
            "properties": {
                "title": { "type": "title", "title": [{ "plain_text": title }] }
            }
        })
    };

    let router = Router::new()
        .route(
            "/v1/users/me",
            get(|State(st): State<Arc<Mutex<SyncMockState>>>| async move {
                st.lock().unwrap().hits.push("GET /users/me".into());
                Json(json!({
                    "object": "user",
                    "id": "bot-1",
                    "name": "Sync Bot",
                    "type": "bot",
                    "bot": { "workspace_name": "Sync WS" }
                }))
            }),
        )
        .route(
            "/v1/search",
            post(
                move |State(st): State<Arc<Mutex<SyncMockState>>>, Json(body): Json<Value>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push("POST /search".into());
                    let cursor = body.get("start_cursor").and_then(Value::as_str);
                    if g.search_two_pages_same_ts {
                        let ts = g.page_ts.clone();
                        if cursor.is_none() {
                            return Json(json!({
                                "object": "list",
                                "results": [
                                    mk_page("page-a", "Page A", &ts, json!({"type":"workspace","workspace":true}))
                                ],
                                "has_more": true,
                                "next_cursor": "cur-2"
                            }));
                        }
                        return Json(json!({
                            "object": "list",
                            "results": [
                                mk_page("page-same-min-2", "Same Minute Second Page", &ts, json!({"type":"workspace","workspace":true}))
                            ],
                            "has_more": false,
                            "next_cursor": null
                        }));
                    }
                    let mut results = vec![mk_page(
                        "page-a",
                        "Page A",
                        &g.page_ts,
                        json!({"type":"workspace","workspace":true}),
                    )];
                    if !g.beta_archived {
                        results.push(mk_page(
                            "page-beta",
                            "Page Beta",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    if !g.search_omit_valid_page {
                        results.push(mk_page(
                            "page-valid-omitted",
                            "Valid Omitted Spec",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    if !g.deleted_404_missing_from_search {
                        results.push(mk_page(
                            "page-gone-404",
                            "Gone 404 Spec",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    if !g.ambiguous_500_missing_from_search {
                        results.push(mk_page(
                            "page-flaky-500",
                            "Flaky 500 Spec",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        ));
                    }
                    results.push(mk_page(
                        "row-1",
                        "Row One",
                        "2026-11-20T01:00:00.000Z",
                        json!({"type":"database_id","database_id":"db-1"}),
                    ));
                    if !g.row2_archived {
                        results.push(mk_page(
                            "row-2",
                            "Row Two",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"database_id","database_id":"db-1"}),
                        ));
                    }
                    results.push(json!({
                        "object": "database",
                        "id": "db-1",
                        "url": "https://notion.so/db-1",
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false,
                        "title": [{ "plain_text": "Tasks DB" }],
                        "description": [],
                        "last_edited_time": "2026-11-20T01:00:00.000Z",
                        "properties": { "Name": { "id": "title", "type": "title", "title": {} } }
                    }));
                    Json(json!({
                        "object": "list",
                        "results": results,
                        "has_more": false,
                        "next_cursor": null
                    }))
                },
            ),
        )
        .route(
            "/v1/pages/{id}",
            get(
                move |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /pages/{id}"));
                    if id == "page-beta" {
                        let mut p = mk_page(
                            "page-beta",
                            "Page Beta",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"workspace","workspace":true}),
                        );
                        if g.beta_archived {
                            // Verify `in_trash: true` with `archived: false` is treated as archived
                            p["archived"] = json!(false);
                            p["in_trash"] = json!(true);
                        }
                        return (StatusCode::OK, Json(p)).into_response();
                    }
                    if id == "page-valid-omitted" {
                        return (
                            StatusCode::OK,
                            Json(mk_page(
                                "page-valid-omitted",
                                "Valid Omitted Spec",
                                "2026-11-20T01:00:00.000Z",
                                json!({"type":"workspace","workspace":true}),
                            )),
                        )
                            .into_response();
                    }
                    if id == "page-gone-404" && g.deleted_404_missing_from_search {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "code": "object_not_found",
                                "message": "Could not find page with ID"
                            })),
                        )
                            .into_response();
                    }
                    if id == "page-flaky-500" && g.ambiguous_500_missing_from_search {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({
                                "code": "internal_server_error",
                                "message": "transient upstream failure"
                            })),
                        )
                            .into_response();
                    }
                    if id == "row-2" {
                        let mut p = mk_page(
                            "row-2",
                            "Row Two",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"database_id","database_id":"db-1"}),
                        );
                        if g.row2_archived {
                            p["archived"] = json!(true);
                        }
                        return (StatusCode::OK, Json(p)).into_response();
                    }
                    (
                        StatusCode::OK,
                        Json(mk_page(
                            &id,
                            "Page A",
                            &g.page_ts,
                            json!({"type":"workspace","workspace":true}),
                        )),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(
                |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /blocks/{id}/children"));
                    if id == "blk-toggle" {
                        if g.fail_toggle_children {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({ "code": "internal_server_error", "message": "injected failure" })),
                            )
                                .into_response();
                        }
                        return (
                            StatusCode::OK,
                            Json(json!({
                                "object": "list",
                                "results": [{
                                    "id": "blk-toggle-child",
                                    "type": "paragraph",
                                    "has_children": false,
                                    "archived": false,
                                    "paragraph": { "rich_text": [{ "plain_text": g.toggle_child_text }] }
                                }],
                                "has_more": false,
                                "next_cursor": null
                            })),
                        )
                            .into_response();
                    }
                    let mut results = vec![
                        json!({
                            "id": "blk-para",
                            "type": "paragraph",
                            "has_children": false,
                            "archived": false,
                            "paragraph": { "rich_text": [{ "plain_text": g.para_text }] }
                        }),
                        json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "has_children": false,
                            "archived": false,
                            "to_do": { "checked": g.todo_checked, "rich_text": [{ "plain_text": "Check item" }] }
                        }),
                        json!({
                            "id": "blk-toggle",
                            "type": "toggle",
                            "has_children": true,
                            "archived": false,
                            "toggle": { "rich_text": [{ "plain_text": "Details" }] }
                        }),
                    ];
                    if !g.del_archived {
                        results.push(json!({
                            "id": "blk-del",
                            "type": "paragraph",
                            "has_children": false,
                            "archived": false,
                            "paragraph": { "rich_text": [{ "plain_text": "Removable" }] }
                        }));
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "object": "list",
                            "results": results,
                            "has_more": false,
                            "next_cursor": null
                        })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}",
            get(
                |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let g = st.lock().unwrap();
                    if id == "blk-todo" {
                        return Json(json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "parent": { "type": "page_id", "page_id": "page-a" },
                            "has_children": false,
                            "archived": false,
                            "to_do": { "checked": g.todo_checked, "rich_text": [{ "plain_text": "Check item" }] }
                        }));
                    }
                    Json(json!({
                        "id": id,
                        "type": "paragraph",
                        "parent": { "type": "page_id", "page_id": "page-a" },
                        "has_children": false,
                        "archived": false,
                        "paragraph": { "rich_text": [{ "plain_text": g.para_text }] }
                    }))
                },
            )
            .patch(
                |State(st): State<Arc<Mutex<SyncMockState>>>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("PATCH /blocks/{id}"));
                    if id == "blk-todo" {
                        if let Some(c) = body.pointer("/to_do/checked").and_then(Value::as_bool) {
                            g.todo_checked = c;
                        }
                        return Json(json!({
                            "id": "blk-todo",
                            "type": "to_do",
                            "parent": { "type": "page_id", "page_id": "page-a" },
                            "has_children": false,
                            "archived": false,
                            "to_do": { "checked": g.todo_checked, "rich_text": [{ "plain_text": "Check item" }] }
                        }));
                    }
                    if let Some(txt) = body
                        .pointer("/paragraph/rich_text/0/text/content")
                        .and_then(Value::as_str)
                    {
                        g.para_text = txt.to_string();
                    }
                    Json(json!({
                        "id": id,
                        "type": "paragraph",
                        "parent": { "type": "page_id", "page_id": "page-a" },
                        "has_children": false,
                        "archived": false,
                        "paragraph": { "rich_text": [{ "plain_text": g.para_text }] }
                    }))
                },
            )
            .delete(
                |State(st): State<Arc<Mutex<SyncMockState>>>, AxumPath(id): AxumPath<String>| async move {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("DELETE /blocks/{id}"));
                    if id == "blk-del" {
                        g.del_archived = true;
                    }
                    Json(json!({
                        "id": id,
                        "archived": true,
                        "parent": { "type": "page_id", "page_id": "page-a" }
                    }))
                },
            ),
        )
        .route(
            "/v1/comments",
            get(|State(st): State<Arc<Mutex<SyncMockState>>>| async move {
                let mut g = st.lock().unwrap();
                g.hits.push("GET /comments".into());
                Json(json!({
                    "object": "list",
                    "results": g.comments.clone(),
                    "has_more": false,
                    "next_cursor": null
                }))
            }),
        )
        .route(
            "/v1/databases/{id}",
            get(|AxumPath(id): AxumPath<String>| async move {
                Json(json!({
                    "object": "database",
                    "id": id,
                    "url": format!("https://notion.so/{id}"),
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false,
                    "title": [{ "plain_text": "Tasks DB" }],
                    "description": [],
                    "last_edited_time": "2026-11-20T01:00:00.000Z",
                    "properties": { "Name": { "id": "title", "type": "title", "title": {} } }
                }))
            }),
        )
        .route(
            "/v1/databases/{id}/query",
            post(
                move |State(st): State<Arc<Mutex<SyncMockState>>>,
                      AxumQuery(_q): AxumQuery<std::collections::HashMap<String, String>>,
                      Json(body): Json<Value>| async move {
                    let g = st.lock().unwrap();
                    let page_size = body.get("page_size").and_then(Value::as_u64).unwrap_or(100) as usize;
                    let mut rows = vec![mk_page(
                        "row-1",
                        "Row One",
                        "2026-11-20T01:00:00.000Z",
                        json!({"type":"database_id","database_id":"db-1"}),
                    )];
                    if !g.row2_archived && body.get("filter").is_none() {
                        rows.push(mk_page(
                            "row-2",
                            "Row Two",
                            "2026-11-20T01:00:00.000Z",
                            json!({"type":"database_id","database_id":"db-1"}),
                        ));
                    }
                    let has_more = rows.len() > page_size;
                    let next_cursor = has_more.then_some("db-cur-2");
                    rows.truncate(page_size);
                    Json(json!({
                        "object": "list",
                        "results": rows,
                        "has_more": has_more,
                        "next_cursor": next_cursor
                    }))
                },
            ),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("sync_e2e.db");
    let cfg = test_config(&format!("http://{addr}"));
    let auth = AuthManager::from_token(cfg.clone(), "ntn_sync_e2e");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open(&db_path).unwrap();

    // Cache V2
    sync::sync_workspace_tree_with_options(&client, &storage, true, |_, _, _| {})
        .await
        .unwrap();
    let d_v2 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v2.blocks[0].plain_text(), "Body V2");

    // Idle conditional read reuses cached body (0 block GETs!)
    state.lock().unwrap().hits.clear();
    let d_idle = sync::sync_page_detail_with_mode(
        &client,
        &storage,
        "page-a",
        DetailSyncMode::Conditional,
    )
    .await
    .unwrap();
    assert_eq!(d_idle.blocks[0].plain_text(), "Body V2");
    assert!(!state
        .lock()
        .unwrap()
        .hits
        .iter()
        .any(|h| h.contains("/blocks/")));

    // ① List -> Detail with V2 -> V3: MUST fetch V3!
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V3 list-before-detail".into();
        g.page_ts = "2026-11-20T03:00:00.000Z".into();
    }
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    let d_v3 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v3.blocks[0].plain_text(), "Body V3 list-before-detail");

    // ② Detail -> List with V3 -> V4: MUST see V4!
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V4 detail-before-list".into();
        g.page_ts = "2026-11-20T04:00:00.000Z".into();
    }
    let d_v4 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v4.blocks[0].plain_text(), "Body V4 detail-before-list");
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    let d_v4_after_list = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(
        d_v4_after_list.blocks[0].plain_text(),
        "Body V4 detail-before-list"
    );

    // ③ Restart after list sync (reopen SQLite) -> online read MUST see V5!
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V5 after-restart".into();
        g.page_ts = "2026-11-20T05:00:00.000Z".into();
    }
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    drop(storage);
    let storage = Storage::open(&db_path).unwrap();
    let d_v5 = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();
    assert_eq!(d_v5.blocks[0].plain_text(), "Body V5 after-restart");

    // ③b F4c: same-minute edit with current `last_edited_time` is not frozen in cache.
    let now_ts = chrono::Utc::now().to_rfc3339();
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body SameMinute V1".into();
        g.page_ts = now_ts.clone();
    }
    let d_sm1 = sync::sync_page_body(&client, &storage, "page-a", false)
        .await
        .unwrap();
    assert_eq!(d_sm1.blocks[0].plain_text(), "Body SameMinute V1");
    {
        let mut g = state.lock().unwrap();
        // Same timestamp (minute granularity), updated block content
        g.para_text = "Body SameMinute V2".into();
    }
    let d_sm2 = sync::sync_page_body(&client, &storage, "page-a", false)
        .await
        .unwrap();
    assert_eq!(d_sm2.blocks[0].plain_text(), "Body SameMinute V2");
    let d_sm_detail1 = sync::sync_page_detail(&client, &storage, "page-a")
        .await
        .unwrap();
    assert_eq!(d_sm_detail1.blocks[0].plain_text(), "Body SameMinute V2");
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body SameMinute V3".into();
    }
    let d_sm_detail2 = sync::sync_page_detail(&client, &storage, "page-a")
        .await
        .unwrap();
    assert_eq!(d_sm_detail2.blocks[0].plain_text(), "Body SameMinute V3");
    // Restore historical timestamp for subsequent assertions in this test
    {
        let mut g = state.lock().unwrap();
        g.para_text = "Body V5 after-restart".into();
        g.page_ts = "2026-11-20T05:00:00.000Z".into();
    }
    let _ = sync::sync_page_detail(&client, &storage, "page-a").await.unwrap();

    // Frozen parent timestamp + nested toggle child edit + new comment:
    // RefreshAll on spawn_sync_engine MUST see both AND fetch block tree ONLY ONCE!
    {
        let mut g = state.lock().unwrap();
        g.toggle_child_text = "Nested V5 frozen-ts".into();
        g.comments.push(json!({
            "id": "cmt-2",
            "discussion_id": "disc-2",
            "created_time": "2026-11-20T05:00:30.000Z",
            "created_by": { "id": "u-2", "name": "Bob" },
            "rich_text": [{ "plain_text": "Comment V5 frozen-ts" }]
        }));
        g.hits.clear();
    }
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<CoreEvent>();
    let ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), ev_tx);
    ctrl.set_focus(FocusTarget {
        page_id: Some("page-a".into()),
        database_id: None,
    });
    ctrl.request(SyncCommand::RefreshAll);
    let mut refreshed_detail = None;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(ev)) =
            tokio::time::timeout(std::time::Duration::from_millis(500), ev_rx.recv()).await
        {
            if let CoreEvent::PageDetailUpdated(d) = ev {
                refreshed_detail = Some(*d);
                break;
            }
        }
    }
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    ctrl.request(SyncCommand::Shutdown);
    let refreshed_detail = refreshed_detail.expect("RefreshAll must emit PageDetailUpdated");
    assert_eq!(
        refreshed_detail.blocks[2].children[0].plain_text(),
        "Nested V5 frozen-ts"
    );
    assert!(refreshed_detail
        .comments
        .iter()
        .any(|c| c.body == "Comment V5 frozen-ts"));
    let root_block_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        root_block_gets, 1,
        "RefreshAll must not fetch block tree twice for the same page"
    );

    // Block writes (Patch 2): toggle todo, update text, delete block -> immediate online + offline agree!
    let toggled = cli::op_toggle_todo(&client, &storage, "blk-todo", Some(true))
        .await
        .unwrap();
    assert!(matches!(
        toggled.kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));
    let after_toggle_online = cli::op_get_page(&client, &storage, "page-a", false)
        .await
        .unwrap();
    let after_toggle_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert!(matches!(
        after_toggle_online.blocks[1].kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));
    assert!(matches!(
        after_toggle_offline.blocks[1].kind,
        NotionBlockKind::ToDo { checked: true, .. }
    ));

    cli::op_update_block_text(
        &client,
        &storage,
        "blk-para",
        Some("paragraph"),
        "Patched paragraph text",
        None,
    )
    .await
    .unwrap();
    let after_patch_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert_eq!(
        after_patch_offline.blocks[0].plain_text(),
        "Patched paragraph text"
    );

    cli::op_delete_block(&client, &storage, "blk-del")
        .await
        .unwrap();
    let after_del_online = cli::op_get_page(&client, &storage, "page-a", false)
        .await
        .unwrap();
    let after_del_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert!(!after_del_online.blocks.iter().any(|b| b.id == "blk-del"));
    assert!(!after_del_offline.blocks.iter().any(|b| b.id == "blk-del"));

    // Section 4A: Notion Search limitation safety & authoritative reconciliation + DB query cap
    // 1. Database query hitting its limit with has_more=true MUST NOT prune unseen cached rows!
    let capped_rows =
        cli::op_query_database(&client, &storage, "db-1", None, None, 1, None, false)
            .await
            .unwrap()
            .items;
    assert_eq!(capped_rows.len(), 1);
    let offline_after_cap =
        cli::op_query_database(&client, &storage, "db-1", None, None, 50, None, true)
            .await
            .unwrap()
            .items;
    assert_eq!(
        offline_after_cap.len(),
        2,
        "DB query at cap with has_more=true must preserve unseen row-2"
    );

    // 2. Simulate Search omitting 4 pages even with has_more=false:
    //    - page-valid-omitted: valid on GET /v1/pages -> MUST retain cache & FTS!
    //    - page-flaky-500: HTTP 500 on GET /v1/pages -> MUST retain cache & FTS!
    //    - page-beta: in_trash=true on GET /v1/pages -> MUST hide & prune FTS!
    //    - page-gone-404: HTTP 404 object_not_found on GET /v1/pages -> MUST hide & prune FTS!
    {
        let mut g = state.lock().unwrap();
        g.beta_archived = true;
        g.search_omit_valid_page = true;
        g.deleted_404_missing_from_search = true;
        g.ambiguous_500_missing_from_search = true;
        g.hits.clear();
    }
    // Ordinary incremental poll (full_rescan=false) stays cheap: 0 missing-candidate GET probes!
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    assert!(
        !state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.starts_with("GET /pages/")),
        "ordinary incremental sync must not probe missing candidates"
    );

    // Explicit full refresh confirms missing candidates individually via GET by ID:
    sync::sync_workspace_tree_with_options(&client, &storage, true, |_, _, _| {})
        .await
        .unwrap();
    let ws_id = client.auth().active_workspace_id().await;
    let active_pages = cli::op_list_pages(&client, &storage, false, false, true, usize::MAX, None)
        .await
        .unwrap()
        .items;
    assert!(
        active_pages.iter().any(|p| p.id == "page-valid-omitted"),
        "valid page missing from Search must remain cached"
    );
    assert!(
        !storage
            .search_local(&ws_id, "Valid Omitted", 10)
            .unwrap()
            .is_empty(),
        "valid page missing from Search must retain FTS entry"
    );
    assert!(
        active_pages.iter().any(|p| p.id == "page-flaky-500"),
        "candidate with ambiguous HTTP 500 must remain cached"
    );
    assert!(
        !storage
            .search_local(&ws_id, "Flaky 500", 10)
            .unwrap()
            .is_empty(),
        "candidate with ambiguous HTTP 500 must retain FTS entry"
    );
    assert!(
        !active_pages.iter().any(|p| p.id == "page-beta"),
        "authoritatively in_trash page must be hidden"
    );
    assert!(
        storage
            .search_local(&ws_id, "Page Beta", 10)
            .unwrap()
            .is_empty(),
        "authoritatively in_trash page must be removed from FTS"
    );
    assert!(
        !active_pages.iter().any(|p| p.id == "page-gone-404"),
        "authoritatively 404 object_not_found page must be hidden"
    );
    assert!(
        storage
            .search_local(&ws_id, "Gone 404", 10)
            .unwrap()
            .is_empty(),
        "authoritatively 404 object_not_found page must be removed from FTS"
    );

    // 3. Complete unfiltered DB query (has_more=false) after archiving row-2 prunes row-2:
    state.lock().unwrap().row2_archived = true;
    let db_rows_live =
        cli::op_query_database(&client, &storage, "db-1", None, None, 50, None, false)
            .await
            .unwrap()
            .items;
    assert_eq!(db_rows_live.len(), 1);
    let db_rows_offline =
        cli::op_query_database(&client, &storage, "db-1", None, None, 50, None, true)
            .await
            .unwrap()
            .items;
    assert_eq!(db_rows_offline.len(), 1);
    assert_eq!(db_rows_offline[0].id, "row-1");

    // Section 4B: Pagination boundaries — same-minute watermark continues to page 2,
    // and nested child fetch failure propagates Err without deleting cached children!
    state.lock().unwrap().search_two_pages_same_ts = true;
    let inc = client
        .search_incremental(
            None,
            notion_cli::notion::SearchFilterObject::All,
            100,
            Some("2026-11-20T05:00:00.000Z"),
            |_, _, _| {},
        )
        .await
        .unwrap();
    assert!(inc.pages.iter().any(|p| p.id == "page-same-min-2"));
    state.lock().unwrap().search_two_pages_same_ts = false;

    state.lock().unwrap().fail_toggle_children = true;
    assert!(sync::sync_page_detail_force(&client, &storage, "page-a")
        .await
        .is_err());
    let preserved_offline = cli::op_get_page(&client, &storage, "page-a", true)
        .await
        .unwrap();
    assert_eq!(
        preserved_offline.blocks[2].children[0].plain_text(),
        "Nested V5 frozen-ts"
    );
    state.lock().unwrap().fail_toggle_children = false;

    // Section 4C & 4D: TUI restart focus restoration + inline editor external edit conflict guard
    let (focus_tx, focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let mut ui = UiState::from_config(&cfg);
    let mut app = AppState::new(
        cfg.clone(),
        auth.clone(),
        client.clone(),
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    app.workspace_id = client.auth().active_workspace_id().await;
    app.load_cached_initial();
    assert_eq!(
        focus_rx.borrow().page_id.as_deref(),
        app.active_page_id.as_deref(),
        "load_cached_initial must restore SyncController focus on restart"
    );

    // Open page-a, enter inline editor, type draft, receive external edit -> draft preserved + conflict armed
    app.open_page("page-a", &mut ui, false);
    assert!(app.open_inline_editor(&mut ui));
    action::dispatch(Action::Input('X'), &mut app, &mut ui);
    let user_draft = ui.inline_editor.as_ref().unwrap().composer.text().to_string();
    assert!(user_draft.starts_with('X'));

    let mut remote_detail = app.active_page_detail.clone().unwrap();
    remote_detail.page.last_edited_time = Some("2026-11-20T06:00:00.000Z".into());
    remote_detail.markdown = "Remote concurrent change".into();
    app.handle_core_event(&mut ui, CoreEvent::PageDetailUpdated(Box::new(remote_detail)));

    let ed = ui.inline_editor.as_ref().unwrap();
    assert_eq!(ed.composer.text(), user_draft, "user editing buffer must be preserved");
    assert!(ed.remote_conflict.is_some());

    // First Ctrl+S warns and does NOT dispatch overwrite or write __optimistic_* to SQLite
    let dispatched_first = app.flush_inline_editor_sync(&mut ui, true);
    assert!(!dispatched_first);
    assert!(ui.inline_editor.as_ref().unwrap().confirm_overwrite);
    let cached_blocks = storage.load_page_blocks(&app.workspace_id, "page-a").unwrap();
    assert!(cached_blocks.iter().all(|b| !b.id.starts_with("__optimistic_")));

    // TUI toggle_todo_block applies server block directly without reverting checked state
    app.discard_inline_editor(&mut ui);
    app.toggle_todo_block("blk-todo", false, &mut ui);
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg, UiMsg::PageRefreshed(_)));
    app.handle_ui_msg(&mut ui, msg);
    assert!(matches!(
        app.active_page_detail.as_ref().unwrap().blocks[1].kind,
        NotionBlockKind::ToDo { checked: false, .. }
    ));

    server_handle.abort();
}

#[tokio::test]
async fn e2e_realtime_sync_dedup_churn_sidebar_merge_and_rate_limit() {
    use notion_cli::models::{CoreEvent, NotionPageSummary};
    use notion_cli::sync::{FocusTarget, SyncCommand, SyncController};
    use tokio::sync::oneshot;

    #[derive(Default)]
    struct RtMock {
        slow_delay_ms: u64,
        return_429_once: bool,
        return_529_once: bool,
        return_529_blocked: bool,
        para_by_page: std::collections::HashMap<String, String>,
        nested_text: String,
        title_alpha: String,
        icon_alpha: String,
        comments_alpha: Vec<Value>,
        hits: Vec<String>,
        block_response_gate: Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>,
    }

    let state = Arc::new(Mutex::new(RtMock {
        slow_delay_ms: 0,
        return_429_once: false,
        return_529_once: false,
        return_529_blocked: false,
        para_by_page: [
            ("page-a".to_string(), "Alpha V1".to_string()),
            ("page-b".to_string(), "Beta V1".to_string()),
            ("page-c".to_string(), "Gamma V1".to_string()),
            ("page-d".to_string(), "Delta V1".to_string()),
        ]
        .into_iter()
        .collect(),
        nested_text: "Nested V1".to_string(),
        title_alpha: "Alpha Spec".to_string(),
        icon_alpha: "🚀".to_string(),
        comments_alpha: vec![json!({
            "id": "c-1",
            "discussion_id": "d-1",
            "created_time": "2026-11-20T01:00:00.000Z",
            "created_by": { "id": "u-1", "name": "Alice" },
            "rich_text": [{ "plain_text": "Comment 1" }]
        })],
        hits: Vec::new(),
        block_response_gate: None,
    }));

    let app_router = Router::new()
        .route(
            "/v1/users/me",
            get(|State(st): State<Arc<Mutex<RtMock>>>| async move {
                st.lock().unwrap().hits.push("GET /users/me".into());
                Json(json!({
                    "object": "user", "id": "bot-rt", "name": "RT Bot",
                    "type": "bot", "bot": { "workspace_name": "RT WS" }
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/search",
            post(|State(st): State<Arc<Mutex<RtMock>>>| async move {
                let mut g = st.lock().unwrap();
                g.hits.push("POST /search".into());
                let title_a = g.title_alpha.clone();
                let icon_a = g.icon_alpha.clone();
                Json(json!({
                    "object": "list",
                    "results": [
                        {
                            "object": "page", "id": "page-a", "url": "https://notion.so/page-a",
                            "icon": { "type": "emoji", "emoji": icon_a },
                            "parent": { "type": "workspace", "workspace": true },
                            "archived": false, "last_edited_time": "2026-11-20T01:00:00.000Z",
                            "properties": { "title": { "type": "title", "title": [{ "plain_text": title_a }] } }
                        },
                        {
                            "object": "page", "id": "page-d", "url": "https://notion.so/page-d",
                            "icon": { "type": "emoji", "emoji": "📝" },
                            "parent": { "type": "workspace", "workspace": true },
                            "archived": false, "last_edited_time": "2026-11-20T01:00:00.000Z",
                            "properties": { "title": { "type": "title", "title": [{ "plain_text": "Delta" }] } }
                        }
                    ],
                    "has_more": false,
                    "next_cursor": Value::Null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(|State(st): State<Arc<Mutex<RtMock>>>, AxumPath(id): AxumPath<String>| async move {
                let (delay, do_429, do_529, do_529_blocked, title, icon) = {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /pages/{id}"));
                    let d = g.slow_delay_ms;
                    let r429 = std::mem::take(&mut g.return_429_once);
                    let r529 = std::mem::take(&mut g.return_529_once);
                    let r529b = std::mem::take(&mut g.return_529_blocked);
                    let t = if id == "page-a" { g.title_alpha.clone() } else { format!("Title {id}") };
                    let ic = if id == "page-a" { g.icon_alpha.clone() } else { "📝".to_string() };
                    (d, r429, r529, r529b, t, ic)
                };
                if do_429 {
                    return (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(header::RETRY_AFTER, "0.2")],
                        Json(json!({ "code": "rate_limited", "message": "Slow down" })),
                    )
                        .into_response();
                }
                if do_529 {
                    return (
                        StatusCode::from_u16(529).unwrap(),
                        [(header::RETRY_AFTER, "0.15")],
                        Json(json!({ "code": "service_overload", "message": "Overloaded" })),
                    )
                        .into_response();
                }
                if do_529_blocked {
                    return (
                        StatusCode::from_u16(529).unwrap(),
                        [(header::RETRY_AFTER, "1")],
                        Json(json!({ "code": "public_api_request_blocked", "message": "Blocked" })),
                    )
                        .into_response();
                }
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Json(json!({
                    "object": "page", "id": id, "url": format!("https://notion.so/{id}"),
                    "icon": { "type": "emoji", "emoji": icon },
                    "parent": { "type": "workspace", "workspace": true },
                    "archived": false, "last_edited_time": "2026-11-20T01:00:00.000Z",
                    "properties": { "title": { "type": "title", "title": [{ "plain_text": title }] } }
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(|State(st): State<Arc<Mutex<RtMock>>>, AxumPath(id): AxumPath<String>| async move {
                let (delay, results, gate) = {
                    let mut g = st.lock().unwrap();
                    g.hits.push(format!("GET /blocks/{id}/children"));
                    let d = g.slow_delay_ms;
                    let res = if id == "blk-nested-parent" {
                        vec![json!({
                            "id": "blk-nested-child",
                            "type": "paragraph",
                            "has_children": false,
                            "paragraph": { "rich_text": [{ "plain_text": g.nested_text.clone() }] }
                        })]
                    } else {
                        let txt = g
                            .para_by_page
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| format!("Body {id}"));
                        let mut v = vec![json!({
                            "id": format!("blk-p-{id}"),
                            "type": "paragraph",
                            "has_children": false,
                            "paragraph": { "rich_text": [{ "plain_text": txt }] }
                        })];
                        if id == "page-a" {
                            v.push(json!({
                                "id": "blk-nested-parent",
                                "type": "bulleted_list_item",
                                "has_children": true,
                                "bulleted_list_item": { "rich_text": [{ "plain_text": "Bullet" }] }
                            }));
                        }
                        v
                    };
                    let gate = if id == "page-a" {
                        g.block_response_gate.take()
                    } else {
                        None
                    };
                    (d, res, gate)
                };
                if let Some((received, resume)) = gate {
                    let _ = received.send(());
                    let _ = resume.await;
                }
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Json(json!({
                    "object": "list", "results": results, "has_more": false, "next_cursor": Value::Null
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/comments",
            get(|State(st): State<Arc<Mutex<RtMock>>>, Query(q): Query<CommentQuery>| async move {
                let mut g = st.lock().unwrap();
                g.hits.push(format!("GET /comments?block_id={}", q.block_id));
                let list = if q.block_id == "page-a" {
                    g.comments_alpha.clone()
                } else {
                    Vec::new()
                };
                Json(json!({
                    "object": "list", "results": list, "has_more": false, "next_cursor": Value::Null
                }))
                .into_response()
            }),
        )
        .with_state(state.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let srv_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let mut cfg = test_config(&base_url);
    cfg.active_poll_ms = 3_000;
    cfg.sync_poll_ms = 60_000;
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let storage = Storage::open(&cache_path).unwrap();

    // 1. 429 & 529 Retry-After handling (and 529 public_api_request_blocked does NOT retry)
    state.lock().unwrap().return_429_once = true;
    let t0 = std::time::Instant::now();
    let p = client.get_page("page-a").await.unwrap();
    assert_eq!(p.id, "page-a");
    assert!(
        t0.elapsed() >= std::time::Duration::from_millis(160),
        "429 Retry-After 0.2s must be honored"
    );

    state.lock().unwrap().return_529_once = true;
    let t1 = std::time::Instant::now();
    let p2 = client.get_page("page-a").await.unwrap();
    assert_eq!(p2.id, "page-a");
    assert!(
        t1.elapsed() >= std::time::Duration::from_millis(120),
        "529 service_overload Retry-After 0.15s must be honored"
    );

    state.lock().unwrap().hits.clear();
    state.lock().unwrap().return_529_blocked = true;
    let blocked_err = client.get_page("page-a").await.unwrap_err();
    assert!(blocked_err.to_string().contains("public_api_request_blocked"));
    assert_eq!(
        state
            .lock()
            .unwrap()
            .hits
            .iter()
            .filter(|h| *h == "GET /pages/page-a")
            .count(),
        1,
        "public_api_request_blocked must not retry"
    );

    // Seed workspace tree & initial page detail
    sync::sync_workspace_tree(&client, &storage).await.unwrap();
    sync::sync_page_detail_force(&client, &storage, "page-a")
        .await
        .unwrap();

    // 2. Coalesced SetFocus + explicit RefreshPage on an already-cached unchanged page MUST:
    //    - fetch the block tree only once (deduped)
    //    - deliver PageDetailUpdated (explicit command result delivery)
    //    - not flash SyncStateChanged { busy: true }
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<CoreEvent>();
    let ctrl = sync::spawn_sync_engine(cfg.clone(), client.clone(), storage.clone(), ev_tx);
    state.lock().unwrap().slow_delay_ms = 180;
    state.lock().unwrap().hits.clear();
    ctrl.set_focus(FocusTarget {
        page_id: Some("page-a".into()),
        database_id: None,
    });
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    let root_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        root_gets, 1,
        "coalesced SetFocus + overlapping RefreshPage must share one in-flight fetch"
    );
    let mut saw_busy = false;
    let mut page_events = 0usize;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            CoreEvent::SyncStateChanged { busy: true, .. } => saw_busy = true,
            CoreEvent::PageDetailUpdated(_) => page_events += 1,
            _ => {}
        }
    }
    assert!(!saw_busy, "explicit RefreshPage must not flash busy=true");
    assert_eq!(
        page_events, 1,
        "explicit SetFocus+RefreshPage on unchanged cached page must still deliver PageDetailUpdated"
    );

    // 2b. Unchanged PERIODIC active poll (~3s) fetches body directly once and stays completely silent
    //     (0 PageDetailUpdated, 0 busy=true).
    state.lock().unwrap().slow_delay_ms = 0;
    state.lock().unwrap().hits.clear();
    let periodic_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < periodic_deadline {
        if state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.as_str() == "GET /blocks/page-a/children")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let periodic_block_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        periodic_block_gets, 1,
        "periodic active poll must directly check open page body once"
    );
    let mut periodic_busy = false;
    let mut periodic_page_events = 0usize;
    while let Ok(ev) = ev_rx.try_recv() {
        match ev {
            CoreEvent::SyncStateChanged { busy: true, .. } => periodic_busy = true,
            CoreEvent::PageDetailUpdated(_) => periodic_page_events += 1,
            _ => {}
        }
    }
    assert!(!periodic_busy, "unchanged periodic poll must not flash busy");
    assert_eq!(
        periodic_page_events, 0,
        "unchanged periodic poll must emit 0 PageDetailUpdated events"
    );

    // 2c. Upgrading an already-running silent periodic fetch with an explicit RefreshPage:
    //     must share ONE network fetch (0 extra GETs) AND emit PageDetailUpdated even when unchanged!
    state.lock().unwrap().slow_delay_ms = 240;
    state.lock().unwrap().hits.clear();
    let upgrade_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while tokio::time::Instant::now() < upgrade_deadline {
        if state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h.as_str() == "GET /blocks/page-a/children")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(15)).await;
    }
    // Silent periodic fetch is now in flight (~240ms delay): send explicit RefreshPage while in flight!
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    let upgraded_ev = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(d)) = ev_rx.recv().await {
                break *d;
            }
        }
    })
    .await
    .expect("upgrading in-flight silent periodic fetch with RefreshPage must deliver PageDetailUpdated");
    assert_eq!(upgraded_ev.page.id, "page-a");
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let upgrade_block_gets = state
        .lock()
        .unwrap()
        .hits
        .iter()
        .filter(|h| h.as_str() == "GET /blocks/page-a/children")
        .count();
    assert_eq!(
        upgrade_block_gets, 1,
        "upgrading in-flight silent periodic fetch must not issue a second GET"
    );

    // 3. Same-timestamp nested block + comment + title/icon update picked up by quiet RefreshPage with 0 POST /search
    {
        let mut g = state.lock().unwrap();
        g.slow_delay_ms = 0;
        g.nested_text = "Nested V2 same-ts".into();
        g.title_alpha = "Alpha Renamed".into();
        g.icon_alpha = "🔥".into();
        g.comments_alpha.push(json!({
            "id": "c-2",
            "discussion_id": "d-2",
            "created_time": "2026-11-20T01:00:30.000Z",
            "created_by": { "id": "u-2", "name": "Bob" },
            "rich_text": [{ "plain_text": "Comment 2 same-ts" }]
        }));
        g.hits.clear();
    }
    ctrl.request(SyncCommand::RefreshPage {
        page_id: "page-a".into(),
    });
    let detail_v2 = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(d)) = ev_rx.recv().await {
                break *d;
            }
        }
    })
    .await
    .expect("same-ts nested/comment/title change must emit PageDetailUpdated");
    assert_eq!(detail_v2.page.title, "Alpha Renamed");
    assert_eq!(detail_v2.page.icon.as_deref(), Some("🔥"));
    assert_eq!(
        detail_v2.blocks[1].children[0].plain_text(),
        "Nested V2 same-ts"
    );
    assert_eq!(detail_v2.comments.len(), 2);
    assert!(
        !state
            .lock()
            .unwrap()
            .hits
            .iter()
            .any(|h| h == "POST /search"),
        "visible page refresh must execute 0 workspace searches"
    );

    // A separate forced refresh can commit while the quiet poll is still awaiting its response.
    let (received, request) = oneshot::channel();
    let (resume, release_response) = oneshot::channel();
    {
        let mut g = state.lock().unwrap();
        g.para_by_page.insert("page-a".into(), "Concurrent refresh".into());
        g.block_response_gate = Some((received, release_response));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), request)
        .await
        .expect("periodic poll must request page blocks")
        .unwrap();
    let other_storage = Storage::open(&cache_path).unwrap();
    let written = sync::sync_page_detail_force(&client, &other_storage, "page-a")
        .await
        .unwrap();
    assert_eq!(written.blocks[0].plain_text(), "Concurrent refresh");
    resume.send(()).unwrap();
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(detail)) = ev_rx.recv().await {
                break detail;
            }
        }
    })
    .await
    .expect("the periodic poll must deliver the edit already written by the other connection");
    assert_eq!(delivered.blocks[0].plain_text(), "Concurrent refresh");

    // 4. Rapid focus churn A -> B -> C -> D cannot lose latest target D!
    state.lock().unwrap().slow_delay_ms = 120;
    while ev_rx.try_recv().is_ok() {}
    for pid in ["page-a", "page-b", "page-c", "page-d"] {
        ctrl.set_focus(FocusTarget {
            page_id: Some(pid.into()),
            database_id: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let final_detail = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(CoreEvent::PageDetailUpdated(d)) = ev_rx.recv().await {
                if d.page.id == "page-d" {
                    break *d;
                }
            }
        }
    })
    .await
    .expect("rapid focus churn must converge on latest target page-d");
    assert_eq!(final_detail.blocks[0].plain_text(), "Delta V1");

    ctrl.request(SyncCommand::Shutdown);

    // 5. UI merge: PageDetailUpdated updates sidebar title/icon + Canvas in one update
    //    while preserving selection, scroll, and dirty inline editor draft.
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);
    let mut ui = UiState::from_config(&cfg);
    let mut app = AppState::new(
        cfg.clone(),
        auth.clone(),
        client.clone(),
        storage.clone(),
        sync_ctrl,
        ui_tx,
    );
    app.workspace_id = client.auth().active_workspace_id().await;
    app.pages = vec![NotionPageSummary {
        id: "page-a".into(),
        workspace_id: app.workspace_id.clone(),
        title: "Alpha Spec".into(),
        icon: Some("🚀".into()),
        is_favorite: true,
        ..Default::default()
    }];
    app.open_page("page-a", &mut ui, false);
    ui.selected_block = 1;
    ui.canvas_scroll = 7;
    ui.sidebar_selected = 2;

    let changed_redraw =
        app.handle_core_event(&mut ui, CoreEvent::PageDetailUpdated(Box::new(detail_v2.clone())));
    assert!(changed_redraw, "changed PageDetailUpdated must trigger redraw");
    let unchanged_redraw =
        app.handle_core_event(&mut ui, CoreEvent::PageDetailUpdated(Box::new(detail_v2)));
    assert!(
        !unchanged_redraw,
        "identical unchanged PageDetailUpdated must not trigger redundant TUI redraw"
    );
    assert_eq!(app.pages[0].title, "Alpha Renamed");
    assert_eq!(app.pages[0].icon.as_deref(), Some("🔥"));
    assert!(app.pages[0].is_favorite, "local favorite flag must be preserved");
    assert_eq!(
        app.active_page_detail.as_ref().unwrap().page.title,
        "Alpha Renamed"
    );
    assert_eq!(ui.selected_block, 1, "selected block must be preserved");
    assert_eq!(ui.canvas_scroll, 7, "canvas scroll must be preserved");
    assert_eq!(ui.sidebar_selected, 2, "sidebar selection must be preserved");

    // refresh_active_item dispatches RefreshPage for active page (FocusGained path)
    while cmd_rx.try_recv().is_ok() {}
    app.refresh_active_item();
    assert_eq!(
        cmd_rx.try_recv().ok(),
        Some(SyncCommand::RefreshPage {
            page_id: "page-a".into()
        })
    );

    srv_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_01_status_with_select_properties_and_controls() {
    use std::collections::HashMap;
    use tokio::process::Command;

    const DB_SP: &str = "11000000-0000-0000-0000-000000000001";
    const ROW_SP: &str = "11000000-0000-0000-0000-000000000011";
    const DB_SEL_ONLY: &str = "11000000-0000-0000-0000-000000000002";
    const ROW_SEL_ONLY: &str = "11000000-0000-0000-0000-000000000012";
    const DB_MULTI_ST: &str = "11000000-0000-0000-0000-000000000003";
    const ROW_MULTI_ST: &str = "11000000-0000-0000-0000-000000000013";
    const DB_MULTI_SEL: &str = "11000000-0000-0000-0000-000000000004";

    #[derive(Clone, Debug)]
    struct ReqLog {
        method: String,
        path: String,
        body: Value,
    }

    #[derive(Clone)]
    struct Bug01Mock {
        dbs: HashMap<String, Value>,
        pages: Arc<Mutex<HashMap<String, Value>>>,
        writes: Arc<Mutex<Vec<ReqLog>>>,
    }

    impl Bug01Mock {
        fn take_writes(&self) -> Vec<ReqLog> {
            std::mem::take(&mut *self.writes.lock().unwrap())
        }
    }

    let mut dbs = HashMap::new();
    dbs.insert(
        DB_SP.into(),
        json!({
            "object": "database",
            "id": DB_SP,
            "url": format!("https://www.notion.so/{DB_SP}"),
            "parent": { "type": "workspace", "workspace": true },
            "title": [{ "plain_text": "Status + Priority" }],
            "description": [],
            "properties": {
                "Name": { "id": "title", "type": "title", "title": {} },
                "Status": { "id": "st", "type": "status", "status": { "options": [] } },
                "Priority": { "id": "pr", "type": "select", "select": { "options": [] } }
            }
        }),
    );
    dbs.insert(
        DB_SEL_ONLY.into(),
        json!({
            "object": "database",
            "id": DB_SEL_ONLY,
            "url": format!("https://www.notion.so/{DB_SEL_ONLY}"),
            "parent": { "type": "workspace", "workspace": true },
            "title": [{ "plain_text": "Select Only" }],
            "description": [],
            "properties": {
                "Name": { "id": "title", "type": "title", "title": {} },
                "Stage": { "id": "sg", "type": "select", "select": { "options": [] } }
            }
        }),
    );
    dbs.insert(
        DB_MULTI_ST.into(),
        json!({
            "object": "database",
            "id": DB_MULTI_ST,
            "url": format!("https://www.notion.so/{DB_MULTI_ST}"),
            "parent": { "type": "workspace", "workspace": true },
            "title": [{ "plain_text": "Multi Status" }],
            "description": [],
            "properties": {
                "Name": { "id": "title", "type": "title", "title": {} },
                "Status": { "id": "st1", "type": "status", "status": { "options": [] } },
                "Review": { "id": "st2", "type": "status", "status": { "options": [] } },
                "Priority": { "id": "pr", "type": "select", "select": { "options": [] } }
            }
        }),
    );
    dbs.insert(
        DB_MULTI_SEL.into(),
        json!({
            "object": "database",
            "id": DB_MULTI_SEL,
            "url": format!("https://www.notion.so/{DB_MULTI_SEL}"),
            "parent": { "type": "workspace", "workspace": true },
            "title": [{ "plain_text": "Multi Select" }],
            "description": [],
            "properties": {
                "Name": { "id": "title", "type": "title", "title": {} },
                "Stage": { "id": "sg", "type": "select", "select": { "options": [] } },
                "Priority": { "id": "pr", "type": "select", "select": { "options": [] } }
            }
        }),
    );

    let mut pages = HashMap::new();
    pages.insert(
        ROW_SP.into(),
        json!({
            "object": "page",
            "id": ROW_SP,
            "url": format!("https://www.notion.so/{ROW_SP}"),
            "parent": { "type": "database_id", "database_id": DB_SP },
            "archived": false,
            "properties": {
                "Name": { "type": "title", "title": [{ "plain_text": "Existing Task" }] },
                "Status": { "type": "status", "status": { "name": "In Progress" } },
                "Priority": { "type": "select", "select": { "name": "P2" } }
            }
        }),
    );
    pages.insert(
        ROW_SEL_ONLY.into(),
        json!({
            "object": "page",
            "id": ROW_SEL_ONLY,
            "url": format!("https://www.notion.so/{ROW_SEL_ONLY}"),
            "parent": { "type": "database_id", "database_id": DB_SEL_ONLY },
            "archived": false,
            "properties": {
                "Name": { "type": "title", "title": [{ "plain_text": "Select Row" }] },
                "Stage": { "type": "select", "select": { "name": "Backlog" } }
            }
        }),
    );
    pages.insert(
        ROW_MULTI_ST.into(),
        json!({
            "object": "page",
            "id": ROW_MULTI_ST,
            "url": format!("https://www.notion.so/{ROW_MULTI_ST}"),
            "parent": { "type": "database_id", "database_id": DB_MULTI_ST },
            "archived": false,
            "properties": {
                "Name": { "type": "title", "title": [{ "plain_text": "Multi Status Row" }] },
                "Status": { "type": "status", "status": { "name": "Todo" } },
                "Review": { "type": "status", "status": { "name": "Pending" } },
                "Priority": { "type": "select", "select": { "name": "P1" } }
            }
        }),
    );

    let st = Bug01Mock {
        dbs,
        pages: Arc::new(Mutex::new(pages)),
        writes: Arc::new(Mutex::new(Vec::new())),
    };

    let app = Router::new()
        .route(
            "/v1/databases/{id}",
            get(|State(st): State<Bug01Mock>, AxumPath(id): AxumPath<String>| async move {
                if let Some(db) = st.dbs.get(&id) {
                    return Json(db.clone()).into_response();
                }
                (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
            }),
        )
        .route(
            "/v1/pages",
            post(|State(st): State<Bug01Mock>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(ReqLog {
                    method: "POST".into(),
                    path: "/v1/pages".into(),
                    body: body.clone(),
                });
                let mut pages = st.pages.lock().unwrap();
                let new_id = format!("11000000-0000-0000-0000-00000000{:04}", pages.len() + 100);
                let page = json!({
                    "object": "page",
                    "id": new_id,
                    "url": format!("https://www.notion.so/{new_id}"),
                    "parent": body.get("parent").cloned().unwrap_or_else(|| json!({"type":"workspace"})),
                    "archived": false,
                    "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                });
                pages.insert(new_id, page.clone());
                Json(page).into_response()
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(|State(st): State<Bug01Mock>, AxumPath(id): AxumPath<String>| async move {
                if let Some(p) = st.pages.lock().unwrap().get(&id).cloned() {
                    return Json(p).into_response();
                }
                (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
            })
            .patch(|State(st): State<Bug01Mock>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                st.writes.lock().unwrap().push(ReqLog {
                    method: "PATCH".into(),
                    path: format!("/v1/pages/{id}"),
                    body: body.clone(),
                });
                let mut pages = st.pages.lock().unwrap();
                if let Some(p) = pages.get_mut(&id) {
                    if let Some(props) = body.get("properties").and_then(Value::as_object) {
                        for (k, v) in props {
                            p["properties"][k] = v.clone();
                        }
                    }
                    return Json(p.clone()).into_response();
                }
                (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
            }),
        )
        .with_state(st.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let mock_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let cli = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_notion"));
        cmd.env("NOTION_API_BASE_URL", format!("{base_url}/v1"))
            .env("NOTION_TOKEN", "ntn_mock_token_123")
            .env("NOTION_CONFIG_DIR", &cfg_dir)
            .env("NOTION_DATA_DIR", &data_dir)
            .env("NOTION_DISABLE_KEYRING", "1")
            .env("NOTION_CLI_CACHE_SECS", "0")
            .args(args);
        cmd
    };

    // 1. CLI create-item: --status Done + Priority=P0 -> both Status.status.name=Done and Priority.select.name=P0
    let out = cli(&[
        "db",
        "create-item",
        DB_SP,
        "--title",
        "Launch Item",
        "--status",
        "Done",
        "--properties",
        r#"{"Priority":{"select":{"name":"P0"}}}"#,
        "--json",
    ])
    .output()
    .await
    .unwrap();
    assert!(out.status.success(), "cli create failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "POST");
    assert_eq!(writes[0].path, "/v1/pages");
    assert_eq!(
        writes[0].body.pointer("/properties/Status/status/name"),
        Some(&json!("Done")),
        "Status.status.name must be Done, got: {}",
        writes[0].body
    );
    assert_eq!(
        writes[0].body.pointer("/properties/Priority/select/name"),
        Some(&json!("P0")),
        "Priority.select.name must stay P0, got: {}",
        writes[0].body
    );
    assert_eq!(
        writes[0].body.pointer("/properties/Name/title/0/text/content"),
        Some(&json!("Launch Item"))
    );

    // 2. CLI update-item: --status Done + Priority=P0 -> both recorded, existing title NOT added
    let out = cli(&[
        "db",
        "update-item",
        ROW_SP,
        "--status",
        "Done",
        "--properties",
        r#"{"Priority":{"select":{"name":"P0"}}}"#,
        "--json",
    ])
    .output()
    .await
    .unwrap();
    assert!(out.status.success(), "cli update failed: {}", String::from_utf8_lossy(&out.stderr));
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/pages/{ROW_SP}"));
    assert_eq!(
        writes[0].body.pointer("/properties/Status/status/name"),
        Some(&json!("Done")),
        "update must set Status.status.name=Done, got: {}",
        writes[0].body
    );
    assert_eq!(
        writes[0].body.pointer("/properties/Priority/select/name"),
        Some(&json!("P0")),
        "update must keep Priority.select.name=P0, got: {}",
        writes[0].body
    );
    assert!(
        writes[0].body.pointer("/properties/Name").is_none()
            && writes[0].body.pointer("/properties/title").is_none(),
        "update without --title must not inject title property: {}",
        writes[0].body
    );

    // 3. MCP create_database_item & update_database_item
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    let mcp_create = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "create_database_item",
                "arguments": {
                    "database": DB_SP,
                    "title": "MCP Item",
                    "status": "Done",
                    "properties": { "Priority": { "select": { "name": "P0" } } }
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_create["result"]["isError"], json!(false), "{mcp_create}");
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Status/status/name"), Some(&json!("Done")));
    assert_eq!(writes[0].body.pointer("/properties/Priority/select/name"), Some(&json!("P0")));

    let mcp_update = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "update_database_item",
                "arguments": {
                    "page_id": ROW_SP,
                    "status": "Done",
                    "properties": { "Priority": { "select": { "name": "P0" } } }
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_update["result"]["isError"], json!(false), "{mcp_update}");
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Status/status/name"), Some(&json!("Done")));
    assert_eq!(writes[0].body.pointer("/properties/Priority/select/name"), Some(&json!("P0")));
    assert!(writes[0].body.pointer("/properties/Name").is_none());

    // 4. HTTP POST /api/v1/databases/{id}/items
    let api_state = Arc::new(serve::ApiState {
        config: cfg.clone(),
        auth: auth.clone(),
        client: client.clone(),
        storage: storage.clone(),
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("http://{api_addr}/api/v1/databases/{DB_SP}/items"))
        .json(&json!({
            "title": "HTTP Item",
            "status": "Done",
            "properties": { "Priority": { "select": { "name": "P0" } } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Status/status/name"), Some(&json!("Done")));
    assert_eq!(writes[0].body.pointer("/properties/Priority/select/name"), Some(&json!("P0")));

    // 5. Conflict check: explicit target property value differs from --status -> error & 0 writes
    let conflict = notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_SP,
        None,
        Some("Done"),
        Some(json!({
            "Status": { "status": { "name": "In Progress" } },
            "Priority": { "select": { "name": "P0" } }
        })),
    )
    .await;
    assert!(conflict.is_err(), "conflicting explicit Status and --status must error");
    assert!(st.take_writes().is_empty(), "conflict must send 0 writes");

    // Identical explicit value + --status succeeds cleanly
    notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_SP,
        None,
        Some("Done"),
        Some(json!({
            "Status": { "status": { "name": "Done" } },
            "Priority": { "select": { "name": "P0" } }
        })),
    )
    .await
    .unwrap();
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Status/status/name"), Some(&json!("Done")));
    assert_eq!(writes[0].body.pointer("/properties/Priority/select/name"), Some(&json!("P0")));

    // 6. Controls:
    // 6a. Status-only call (no extra properties)
    notion_cli::cli::op_update_database_item(&client, &storage, ROW_SP, None, Some("Done"), None)
        .await
        .unwrap();
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Status/status/name"), Some(&json!("Done")));
    assert!(writes[0].body.pointer("/properties/Name").is_none());

    // 6b. Select-only DB (no status column -> falls back to single select column)
    notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_SEL_ONLY,
        None,
        Some("Done"),
        None,
    )
    .await
    .unwrap();
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Stage/select/name"), Some(&json!("Done")));

    // Select-only DB conflict check
    let sel_conflict = notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_SEL_ONLY,
        None,
        Some("Done"),
        Some(json!({ "Stage": { "select": { "name": "Backlog" } } })),
    )
    .await;
    assert!(sel_conflict.is_err(), "conflicting explicit select value must error");
    assert!(st.take_writes().is_empty());

    // 6c. Multi-status DB: unrelated select Priority MUST NOT select a status column -> ambiguity error, 0 writes
    let multi_unrelated = notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_MULTI_ST,
        None,
        Some("Done"),
        Some(json!({ "Priority": { "select": { "name": "P0" } } })),
    )
    .await;
    assert!(
        multi_unrelated.is_err(),
        "unrelated select property must not disambiguate multiple status columns"
    );
    assert!(st.take_writes().is_empty());

    // Multi-status DB: multiple explicit status columns -> ambiguity error, 0 writes
    let multi_ambig = notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_MULTI_ST,
        None,
        Some("Done"),
        Some(json!({ "Status": {}, "Review": {} })),
    )
    .await;
    assert!(multi_ambig.is_err(), "multiple explicit status candidates must error");
    assert!(st.take_writes().is_empty());

    // Multi-status DB: single explicit status column disambiguates while keeping Priority
    notion_cli::cli::op_update_database_item(
        &client,
        &storage,
        ROW_MULTI_ST,
        None,
        Some("Done"),
        Some(json!({ "Review": {}, "Priority": { "select": { "name": "P0" } } })),
    )
    .await
    .unwrap();
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Review/status/name"), Some(&json!("Done")));
    assert_eq!(writes[0].body.pointer("/properties/Priority/select/name"), Some(&json!("P0")));
    assert!(writes[0].body.pointer("/properties/Status").is_none());

    // 6d. Multi-select DB without status column: single explicit select candidate selects it, multiple or none errors
    let multi_sel_none = notion_cli::cli::op_create_database_item(
        &client,
        &storage,
        DB_MULTI_SEL,
        "No Select Picked",
        Some("Done"),
        None,
        None,
    )
    .await;
    assert!(multi_sel_none.is_err());
    assert!(st.take_writes().is_empty());

    notion_cli::cli::op_create_database_item(
        &client,
        &storage,
        DB_MULTI_SEL,
        "Explicit Select Picked",
        Some("Done"),
        Some(json!({ "Stage": {} })),
        None,
    )
    .await
    .unwrap();
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].body.pointer("/properties/Stage/select/name"), Some(&json!("Done")));

    api_server.abort();
    mock_server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_w2_custom_db_title_column_and_pages_edit_guard() {
    use std::collections::HashMap;
    use tokio::process::Command;

    const DB_CUSTOM: &str = "22000000-0000-0000-0000-000000000001";
    const ROW_CUSTOM: &str = "22000000-0000-0000-0000-000000000011";
    const PAGE_PLAIN: &str = "22000000-0000-0000-0000-000000000022";

    #[derive(Clone, Debug)]
    struct ReqLog {
        method: String,
        path: String,
        body: Value,
    }

    #[derive(Clone)]
    struct W2Mock {
        dbs: HashMap<String, Value>,
        pages: Arc<Mutex<HashMap<String, Value>>>,
        writes: Arc<Mutex<Vec<ReqLog>>>,
    }

    impl W2Mock {
        fn take_writes(&self) -> Vec<ReqLog> {
            std::mem::take(&mut *self.writes.lock().unwrap())
        }
    }

    let mut dbs = HashMap::new();
    dbs.insert(
        DB_CUSTOM.into(),
        json!({
            "object": "database",
            "id": DB_CUSTOM,
            "url": format!("https://www.notion.so/{DB_CUSTOM}"),
            "parent": { "type": "workspace", "workspace": true },
            "title": [{ "plain_text": "Sprint Board" }],
            "description": [],
            "properties": {
                "Task Title": { "id": "title", "type": "title", "title": {} },
                "Status": { "id": "st", "type": "status", "status": { "options": [] } }
            }
        }),
    );
    let mut pages = HashMap::new();
    pages.insert(
        ROW_CUSTOM.into(),
        json!({
            "object": "page",
            "id": ROW_CUSTOM,
            "url": format!("https://www.notion.so/{ROW_CUSTOM}"),
            "parent": { "type": "database_id", "database_id": DB_CUSTOM },
            "archived": false,
            "properties": {
                "Task Title": { "type": "title", "title": [{ "plain_text": "Ship CLI Audit" }] },
                "Status": { "type": "status", "status": { "name": "Todo", "color": "gray" } }
            }
        }),
    );
    pages.insert(
        PAGE_PLAIN.into(),
        json!({
            "object": "page",
            "id": PAGE_PLAIN,
            "url": format!("https://www.notion.so/{PAGE_PLAIN}"),
            "parent": { "type": "workspace", "workspace": true },
            "archived": false,
            "properties": {
                "title": { "type": "title", "title": [{ "plain_text": "Regular Page" }] }
            }
        }),
    );

    let st = W2Mock {
        dbs,
        pages: Arc::new(Mutex::new(pages)),
        writes: Arc::new(Mutex::new(Vec::new())),
    };

    let router = Router::new()
        .route(
            "/v1/databases/{id}",
            get(|State(s): State<W2Mock>, AxumPath(id): AxumPath<String>| async move {
                match s.dbs.get(&id) {
                    Some(v) => Json(v.clone()).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({"code": "object_not_found", "message": "not found"}))).into_response(),
                }
            }),
        )
        .route(
            "/v1/pages",
            post(|State(s): State<W2Mock>, Json(body): Json<Value>| async move {
                let is_db = body.pointer("/parent/database_id").is_some();
                if is_db && body.pointer("/properties/Task Title").is_none() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "code": "validation_error",
                            "message": "Name is not a property that exists."
                        })),
                    )
                        .into_response();
                }
                s.writes.lock().unwrap().push(ReqLog {
                    method: "POST".into(),
                    path: "/v1/pages".into(),
                    body: body.clone(),
                });
                Json(json!({
                    "object": "page",
                    "id": "22000000-0000-0000-0000-000000000099",
                    "url": "https://www.notion.so/22000000000000000000000000000099",
                    "parent": body["parent"],
                    "archived": false,
                    "properties": body["properties"]
                }))
                .into_response()
            }),
        )
        .route(
            "/v1/pages/{id}",
            get(|State(s): State<W2Mock>, AxumPath(id): AxumPath<String>| async move {
                match s.pages.lock().unwrap().get(&id).cloned() {
                    Some(v) => Json(v).into_response(),
                    None => (StatusCode::NOT_FOUND, Json(json!({"code": "object_not_found", "message": "not found"}))).into_response(),
                }
            })
            .patch(|State(s): State<W2Mock>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                let current = s.pages.lock().unwrap().get(&id).cloned().unwrap();
                let is_db_row = current.pointer("/parent/database_id").is_some();
                if is_db_row && body.pointer("/properties/title").is_some() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "code": "validation_error",
                            "message": "title is not a property that exists."
                        })),
                    )
                        .into_response();
                }
                s.writes.lock().unwrap().push(ReqLog {
                    method: "PATCH".into(),
                    path: format!("/v1/pages/{id}"),
                    body: body.clone(),
                });
                let mut updated = current;
                if let Some(props) = body.get("properties").and_then(Value::as_object) {
                    if let Some(target) = updated.get_mut("properties").and_then(Value::as_object_mut) {
                        for (k, v) in props {
                            target.insert(k.clone(), v.clone());
                        }
                    }
                }
                Json(updated).into_response()
            }),
        )
        .with_state(st.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let mock_server = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let run = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_notion"));
        cmd.args(args)
            .env("NOTION_API_BASE_URL", format!("{base_url}/v1"))
            .env("NOTION_TOKEN", "ntn_mock_token")
            .env("NOTION_CONFIG_DIR", &cfg_dir)
            .env("NOTION_DATA_DIR", &data_dir)
            .env("NOTION_DISABLE_KEYRING", "1");
        cmd
    };

    // F5a: pages edit --title on custom-title DB row uses "Task Title"
    let out = run(&["pages", "edit", ROW_CUSTOM, "--title", "Renamed", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "PATCH");
    assert_eq!(writes[0].path, format!("/v1/pages/{ROW_CUSTOM}"));
    assert_eq!(
        writes[0].body.pointer("/properties/Task Title/title/0/text/content"),
        Some(&json!("Renamed"))
    );
    assert!(writes[0].body.pointer("/properties/title").is_none());

    // F5b: pages create --parent <db> --database uses "Task Title"
    let out = run(&[
        "pages", "create", "--parent", DB_CUSTOM, "--database", "--title", "New Row", "--json",
    ])
    .output()
    .await
    .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].method, "POST");
    assert_eq!(writes[0].path, "/v1/pages");
    assert_eq!(
        writes[0].body.pointer("/properties/Task Title/title/0/text/content"),
        Some(&json!("New Row"))
    );
    assert!(writes[0].body.pointer("/properties/Name").is_none());

    // Control: regular workspace page edit --title uses "title"
    let out = run(&["pages", "edit", PAGE_PLAIN, "--title", "Updated Regular", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].body.pointer("/properties/title/title/0/text/content"),
        Some(&json!("Updated Regular"))
    );

    // F8d: pages edit with no flags exits 1 with invalid_input JSON envelope and 0 writes
    let out = run(&["pages", "edit", PAGE_PLAIN, "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err_json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(err_json["ok"], false);
    assert_eq!(err_json["error"]["code"], "invalid_input");
    assert!(st.take_writes().is_empty());

    mock_server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_notion_bug_05_code_block_single_edit_preserves_literal_text() {
    use std::collections::HashMap;
    use tokio::process::Command;

    const BLK_CODE: &str = "15000000-0000-0000-0000-000000000001";
    const BLK_CODE_EMPTY: &str = "15000000-0000-0000-0000-000000000002";
    const BLK_PARA: &str = "15000000-0000-0000-0000-000000000003";

    #[derive(Clone, Debug)]
    struct ReqLog {
        method: String,
        path: String,
        body: Value,
    }

    #[derive(Clone)]
    struct Bug05Mock {
        blocks: Arc<Mutex<HashMap<String, Value>>>,
        writes: Arc<Mutex<Vec<ReqLog>>>,
    }

    impl Bug05Mock {
        fn take_writes(&self) -> Vec<ReqLog> {
            std::mem::take(&mut *self.writes.lock().unwrap())
        }
    }

    let mut blocks = HashMap::new();
    blocks.insert(
        BLK_CODE.into(),
        json!({
            "object": "block",
            "id": BLK_CODE,
            "type": "code",
            "has_children": false,
            "code": {
                "language": "python",
                "caption": [{ "type": "text", "text": { "content": "Formula" }, "plain_text": "Formula" }],
                "rich_text": [{ "type": "text", "text": { "content": "old_code()" }, "plain_text": "old_code()" }]
            }
        }),
    );
    blocks.insert(
        BLK_CODE_EMPTY.into(),
        json!({
            "object": "block",
            "id": BLK_CODE_EMPTY,
            "type": "code",
            "has_children": false,
            "code": {
                "language": "rust",
                "caption": [{ "type": "text", "text": { "content": "Empty Snippet" }, "plain_text": "Empty Snippet" }],
                "rich_text": []
            }
        }),
    );
    blocks.insert(
        BLK_PARA.into(),
        json!({
            "object": "block",
            "id": BLK_PARA,
            "type": "paragraph",
            "has_children": false,
            "paragraph": {
                "rich_text": [{ "type": "text", "text": { "content": "old paragraph" }, "plain_text": "old paragraph" }]
            }
        }),
    );

    let st = Bug05Mock {
        blocks: Arc::new(Mutex::new(blocks)),
        writes: Arc::new(Mutex::new(Vec::new())),
    };

    let app = Router::new()
        .route(
            "/v1/blocks/{id}",
            get(|State(st): State<Bug05Mock>, AxumPath(id): AxumPath<String>| async move {
                if let Some(b) = st.blocks.lock().unwrap().get(&id).cloned() {
                    return Json(b).into_response();
                }
                (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
            })
            .patch(
                |State(st): State<Bug05Mock>, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| async move {
                    st.writes.lock().unwrap().push(ReqLog {
                        method: "PATCH".into(),
                        path: format!("/v1/blocks/{id}"),
                        body: body.clone(),
                    });
                    let mut map = st.blocks.lock().unwrap();
                    if let Some(blk) = map.get_mut(&id) {
                        let btype = blk["type"].as_str().unwrap_or("paragraph").to_string();
                        if let Some(patch_inner) = body.get(&btype).and_then(Value::as_object) {
                            if let Some(existing_inner) = blk[&btype].as_object_mut() {
                                for (k, v) in patch_inner {
                                    let mut val = v.clone();
                                    if k == "rich_text" {
                                        if let Some(arr) = val.as_array_mut() {
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
                                    }
                                    existing_inner.insert(k.clone(), val);
                                }
                            }
                        }
                        return Json(blk.clone()).into_response();
                    }
                    (StatusCode::NOT_FOUND, Json(json!({ "message": "not found" }))).into_response()
                },
            ),
        )
        .with_state(st.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let mock_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let code_sample = "val = a * b * c + x ** 2 + y ** 2\ntpl = `hi` \\* path\\to\\file";

    let assert_literal_code_patch = |label: &str, writes: &[ReqLog], returned_blk: &Value, expected: &str| {
        assert_eq!(writes.len(), 1, "{label}: expected 1 PATCH, got {writes:?}");
        assert_eq!(writes[0].method, "PATCH");
        assert_eq!(writes[0].path, format!("/v1/blocks/{BLK_CODE}"));
        let sent_spans = writes[0]
            .body
            .pointer("/code/rich_text")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{label}: missing code.rich_text in {}", writes[0].body));
        let sent_plain: String = sent_spans
            .iter()
            .filter_map(|s| s.pointer("/text/content").and_then(Value::as_str))
            .collect();
        assert_eq!(sent_plain, expected, "{label}: sent code text was mutated");
        for span in sent_spans {
            if let Some(ann) = span.get("annotations") {
                assert_ne!(ann.get("bold"), Some(&json!(true)), "{label}: unexpected bold annotation");
                assert_ne!(ann.get("italic"), Some(&json!(true)), "{label}: unexpected italic annotation");
                assert_ne!(ann.get("code"), Some(&json!(true)), "{label}: unexpected code annotation");
                assert_ne!(ann.get("strikethrough"), Some(&json!(true)), "{label}: unexpected strike annotation");
            }
        }
        // Language & caption preserved (not overwritten in PATCH, still intact in returned block)
        assert!(writes[0].body.pointer("/code/language").is_none(), "{label}: language should not be overwritten");
        assert!(writes[0].body.pointer("/code/caption").is_none(), "{label}: caption should not be overwritten");
        let kind = &returned_blk["kind"];
        assert_eq!(kind["language"], json!("python"), "{label}: language lost");
        let ret_caption: String = kind["caption"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.get("plain_text").and_then(Value::as_str))
            .collect();
        assert_eq!(ret_caption, "Formula", "{label}: caption lost");
        let ret_plain: String = kind["rich_text"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.get("plain_text").and_then(Value::as_str))
            .collect();
        assert_eq!(ret_plain, expected, "{label}: returned plain text mismatch");
    };

    // 1. CLI `blocks edit`
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let out = Command::new(env!("CARGO_BIN_EXE_notion"))
        .env("NOTION_API_BASE_URL", format!("{base_url}/v1"))
        .env("NOTION_TOKEN", "ntn_mock_token_123")
        .env("NOTION_CONFIG_DIR", &cfg_dir)
        .env("NOTION_DATA_DIR", &data_dir)
        .env("NOTION_DISABLE_KEYRING", "1")
        .env("NOTION_CLI_CACHE_SECS", "0")
        .args(["blocks", "edit", BLK_CODE, "--text", code_sample, "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "CLI blocks edit failed: {}", String::from_utf8_lossy(&out.stderr));
    let cli_blk: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_literal_code_patch("CLI", &st.take_writes(), &cli_blk, code_sample);

    // Reset code block content so MCP and HTTP also trigger real PATCH requests
    let reset_code = || {
        let mut map = st.blocks.lock().unwrap();
        map.get_mut(BLK_CODE).unwrap()["code"]["rich_text"] =
            json!([{ "type": "text", "text": { "content": "old_code()" }, "plain_text": "old_code()" }]);
    };

    // 2. MCP `update_block`
    reset_code();
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token_123");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    let mcp_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "update_block",
                "arguments": {
                    "block_id": BLK_CODE,
                    "text": code_sample
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], json!(false), "{mcp_resp}");
    assert_literal_code_patch("MCP", &st.take_writes(), &mcp_resp["result"]["structuredContent"], code_sample);

    // 3. HTTP `PATCH /api/v1/blocks/{id}`
    reset_code();
    let api_state = Arc::new(serve::ApiState {
        config: cfg.clone(),
        auth: auth.clone(),
        client: client.clone(),
        storage: storage.clone(),
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });
    let http = reqwest::Client::new();
    let http_resp: Value = http
        .patch(format!("http://{api_addr}/api/v1/blocks/{BLK_CODE}"))
        .json(&json!({ "text": code_sample }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(http_resp["ok"], json!(true), "{http_resp}");
    assert_literal_code_patch("HTTP", &st.take_writes(), &http_resp["block"], code_sample);

    // 4. Control A: Paragraph edit with the SAME string still parses inline Markdown annotations!
    let para_updated = notion_cli::cli::op_update_block_text(
        &client,
        &storage,
        BLK_PARA,
        None,
        code_sample,
        None,
    )
    .await
    .unwrap();
    let writes = st.take_writes();
    assert_eq!(writes.len(), 1);
    let para_spans = writes[0]
        .body
        .pointer("/paragraph/rich_text")
        .and_then(Value::as_array)
        .unwrap();
    assert!(
        para_spans.iter().any(|s| s.pointer("/annotations/italic") == Some(&json!(true))),
        "paragraph edit must still parse *italic*: {para_spans:?}"
    );
    assert!(
        para_spans.iter().any(|s| s.pointer("/annotations/bold") == Some(&json!(true))),
        "paragraph edit must still parse **bold**: {para_spans:?}"
    );
    assert!(
        para_spans.iter().any(|s| s.pointer("/annotations/code") == Some(&json!(true))),
        "paragraph edit must still parse `code`: {para_spans:?}"
    );
    assert!(!para_updated.plain_text().is_empty());

    // 5. Control B: Editing an already-empty code block with "" creates no symbols and no bogus writes
    let empty_updated = notion_cli::cli::op_update_block_text(
        &client,
        &storage,
        BLK_CODE_EMPTY,
        None,
        "",
        None,
    )
    .await
    .unwrap();
    assert_eq!(empty_updated.plain_text(), "");
    assert!(st.take_writes().is_empty(), "empty code -> empty text must not emit spurious PATCH");
    let mcp_empty = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "update_block",
                "arguments": {
                    "block_id": BLK_CODE_EMPTY,
                    "text": ""
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_empty["result"]["isError"], json!(false), "{mcp_empty}");
    assert!(st.take_writes().is_empty());

    api_server.abort();
    mock_server.abort();
}

// ---------------------------------------------------------------------------------------------
// v0.4.1 audit regressions (S-01, N-01 .. N-19)
//
// AuditMock is a stateful Notion mock that records every request and enforces the request
// limits that matter for these regressions: at most 2,000 UTF-16 units per text span, at most
// 100 items per rich_text array and at most 100 children per parent in one request.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct AuditRequest {
    method: String,
    path: String,
    query: String,
    body: Value,
}

#[derive(Clone, Default)]
struct AuditMock {
    pages: Arc<Mutex<Vec<Value>>>,
    databases: Arc<Mutex<Vec<Value>>>,
    children: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    comments: Arc<Mutex<Vec<Value>>>,
    media: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<AuditRequest>>>,
    next_id: Arc<AtomicUsize>,
}

fn audit_text(text: &str) -> Value {
    json!([{ "type": "text", "text": { "content": text }, "plain_text": text }])
}

fn audit_page(id: &str, title: &str, parent: Value, last_edited: &str) -> Value {
    let title_key = if parent.get("database_id").is_some() { "Name" } else { "title" };
    json!({
        "object": "page",
        "id": id,
        "url": format!("https://www.notion.so/{}", id.replace('-', "")),
        "parent": parent,
        "archived": false,
        "last_edited_time": last_edited,
        "properties": { title_key: { "id": "title", "type": "title", "title": audit_text(title) } }
    })
}

fn audit_database(id: &str, title: &str, last_edited: &str) -> Value {
    json!({
        "object": "database",
        "id": id,
        "url": format!("https://www.notion.so/{}", id.replace('-', "")),
        "parent": { "type": "workspace", "workspace": true },
        "title": audit_text(title),
        "description": [],
        "last_edited_time": last_edited,
        "properties": {
            "Name": { "id": "title", "type": "title", "title": {} },
            "Notes": { "id": "nt", "type": "rich_text", "rich_text": {} },
            "Done": { "id": "dn", "type": "checkbox", "checkbox": {} }
        }
    })
}

fn audit_block(id: &str, kind: &str, text: &str) -> Value {
    json!({
        "object": "block",
        "id": id,
        "type": kind,
        "has_children": false,
        kind: { "rich_text": audit_text(text) }
    })
}

fn audit_error(status: StatusCode, code: &str, message: impl Into<String>) -> axum::response::Response {
    (
        status,
        Json(json!({
            "object": "error",
            "status": status.as_u16(),
            "code": code,
            "message": message.into()
        })),
    )
        .into_response()
}

fn audit_not_found(what: &str, id: &str) -> axum::response::Response {
    audit_error(
        StatusCode::NOT_FOUND,
        "object_not_found",
        format!("Could not find {what} with ID: {id}."),
    )
}

/// Returns the first Notion request-limit violation in a write body.
fn audit_limit_violation(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(content) = map.get("text").and_then(|t| t.get("content")).and_then(Value::as_str) {
                let units = content.encode_utf16().count();
                if units > 2000 {
                    return Some(format!(
                        "text.content.length should be <= 2000, instead was {units}."
                    ));
                }
            }
            for (key, child) in map {
                if let Some(items) = child.as_array() {
                    let cap_applies = matches!(key.as_str(), "rich_text" | "title" | "caption" | "children");
                    if cap_applies && items.len() > 100 {
                        return Some(format!(
                            "{key}.length should be <= 100, instead was {}.",
                            items.len()
                        ));
                    }
                }
                if let Some(found) = audit_limit_violation(child) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(audit_limit_violation),
        _ => None,
    }
}

/// Mirrors Notion echoing plain_text for every text span it stores.
fn audit_fill_plain_text(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let content = map
                .get("text")
                .and_then(|t| t.get("content"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(content) = content {
                map.entry("type").or_insert_with(|| json!("text"));
                map.insert("plain_text".into(), json!(content));
            }
            for child in map.values_mut() {
                audit_fill_plain_text(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(audit_fill_plain_text),
        _ => {}
    }
}

fn audit_title_of(object: &Value) -> String {
    let spans = if object["object"] == "database" {
        object.get("title").and_then(Value::as_array).cloned()
    } else {
        object["properties"].as_object().and_then(|props| {
            props
                .values()
                .find(|p| p["type"] == "title")
                .and_then(|p| p["title"].as_array().cloned())
        })
    };
    spans
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["plain_text"].as_str())
        .collect()
}

fn audit_paginate(items: Vec<Value>, page_size: usize, cursor: Option<&str>) -> Value {
    let start = cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0).min(items.len());
    let end = (start + page_size.clamp(1, 100)).min(items.len());
    let has_more = end < items.len();
    json!({
        "object": "list",
        "results": items[start..end],
        "has_more": has_more,
        "next_cursor": if has_more { json!(end.to_string()) } else { Value::Null }
    })
}

impl AuditMock {
    fn new_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        format!("{prefix}0000-0000-4000-8000-{n:012x}")
    }

    fn add_page(&self, page: Value) {
        self.pages.lock().unwrap().push(page);
    }

    fn set_children(&self, parent: &str, blocks: Vec<Value>) {
        self.children.lock().unwrap().insert(parent.to_string(), blocks);
    }

    fn children_of(&self, parent: &str) -> Vec<Value> {
        self.children.lock().unwrap().get(parent).cloned().unwrap_or_default()
    }

    fn page(&self, id: &str) -> Option<Value> {
        self.pages.lock().unwrap().iter().find(|p| p["id"] == id).cloned()
    }

    fn requests_matching(&self, method: &str, path_prefix: &str) -> Vec<AuditRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .cloned()
            .collect()
    }

    fn count(&self, method: &str, path_prefix: &str) -> usize {
        self.requests_matching(method, path_prefix).len()
    }

    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }

    fn block_exists(&self, id: &str) -> bool {
        self.children
            .lock()
            .unwrap()
            .values()
            .any(|list| list.iter().any(|b| b["id"] == id))
    }

    fn with_has_children(&self, mut block: Value) -> Value {
        let id = block["id"].as_str().unwrap_or_default().to_string();
        let has = self.children.lock().unwrap().get(&id).is_some_and(|c| !c.is_empty());
        block["has_children"] = json!(has);
        block
    }

    /// Stores one appended block (and its nested children) and returns the stored JSON.
    fn store_block(&self, mut block: Value) -> Value {
        let id = self.new_id("b10c");
        let kind = block["type"].as_str().map(str::to_owned).unwrap_or_else(|| {
            block
                .as_object()
                .and_then(|m| m.keys().find(|k| *k != "object" && *k != "children").cloned())
                .unwrap_or_else(|| "paragraph".into())
        });
        let mut nested = block
            .get_mut(&kind)
            .and_then(Value::as_object_mut)
            .and_then(|inner| inner.remove("children"));
        if nested.is_none() {
            nested = block.as_object_mut().and_then(|m| m.remove("children"));
        }
        block["object"] = json!("block");
        block["id"] = json!(id);
        block["type"] = json!(kind);
        audit_fill_plain_text(&mut block);
        let stored_children: Vec<Value> = nested
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .map(|child| self.store_block(child))
            .collect();
        block["has_children"] = json!(!stored_children.is_empty());
        if !stored_children.is_empty() {
            self.children.lock().unwrap().insert(id, stored_children);
        }
        block
    }

    async fn handle(
        State(mock): State<AuditMock>,
        method: axum::http::Method,
        uri: axum::http::Uri,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let path = uri.path().to_string();
        let query = uri.query().unwrap_or("").to_string();
        let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        mock.requests.lock().unwrap().push(AuditRequest {
            method: method.to_string(),
            path: path.clone(),
            query: query.clone(),
            body: body.clone(),
        });
        let params: std::collections::HashMap<String, String> =
            url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        let page_size = |fallback: usize| {
            params
                .get("page_size")
                .and_then(|v| v.parse::<usize>().ok())
                .or_else(|| body["page_size"].as_u64().map(|v| v as usize))
                .unwrap_or(fallback)
        };
        let cursor = params
            .get("start_cursor")
            .cloned()
            .or_else(|| body["start_cursor"].as_str().map(str::to_owned));

        if let Some(name) = path.strip_prefix("/media/") {
            let decoded = urlencoding::decode(name).map(|c| c.into_owned()).unwrap_or_default();
            return match mock.media.lock().unwrap().get(&decoded).cloned() {
                Some(bytes) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/pdf")],
                    bytes,
                )
                    .into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }

        let is_write = matches!(method.as_str(), "POST" | "PATCH")
            && !path.ends_with("/query")
            && path != "/v1/search";
        if is_write {
            if let Some(violation) = audit_limit_violation(&body) {
                return audit_error(
                    StatusCode::BAD_REQUEST,
                    "validation_error",
                    format!("body failed validation: {violation}"),
                );
            }
        }

        let segs: Vec<&str> = path.trim_start_matches("/v1/").split('/').collect();
        match (method.as_str(), segs.as_slice()) {
            ("GET", ["users", "me"]) => Json(json!({
                "object": "user",
                "id": "bot-audit-1",
                "name": "Audit Bot",
                "type": "bot",
                "bot": { "workspace_name": "Audit Workspace" }
            }))
            .into_response(),
            ("POST", ["search"]) => {
                let wanted = body.pointer("/filter/value").and_then(Value::as_str);
                let needle = body["query"].as_str().unwrap_or("").to_lowercase();
                let mut all: Vec<Value> = Vec::new();
                if wanted != Some("database") {
                    all.extend(mock.pages.lock().unwrap().iter().cloned());
                }
                if wanted != Some("page") {
                    all.extend(mock.databases.lock().unwrap().iter().cloned());
                }
                all.retain(|o| needle.is_empty() || audit_title_of(o).to_lowercase().contains(&needle));
                all.sort_by(|a, b| {
                    b["last_edited_time"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(a["last_edited_time"].as_str().unwrap_or(""))
                });
                Json(audit_paginate(all, page_size(100), cursor.as_deref())).into_response()
            }
            ("POST", ["pages"]) => {
                let id = mock.new_id("9a6e");
                let mut page = json!({
                    "object": "page",
                    "id": id,
                    "url": format!("https://www.notion.so/{}", id.replace('-', "")),
                    "parent": body.get("parent").cloned().unwrap_or_else(|| json!({ "type": "workspace" })),
                    "icon": body.get("icon").cloned().unwrap_or(Value::Null),
                    "archived": false,
                    "last_edited_time": "2026-11-22T10:00:00.000Z",
                    "properties": body.get("properties").cloned().unwrap_or_else(|| json!({}))
                });
                if let Some(props) = page["properties"].as_object_mut() {
                    for prop in props.values_mut() {
                        let kind = prop.as_object().and_then(|m| m.keys().next().cloned());
                        if let (Some(kind), None) = (kind, prop.get("type")) {
                            prop["type"] = json!(kind);
                        }
                    }
                }
                audit_fill_plain_text(&mut page);
                let stored: Vec<Value> = body["children"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|child| mock.store_block(child))
                    .collect();
                mock.children.lock().unwrap().insert(id, stored);
                mock.pages.lock().unwrap().push(page.clone());
                Json(page).into_response()
            }
            ("GET", ["pages", id]) => match mock.page(id) {
                Some(page) => Json(page).into_response(),
                None => audit_not_found("page", id),
            },
            ("PATCH", ["pages", id]) => {
                let mut pages = mock.pages.lock().unwrap();
                let Some(page) = pages.iter_mut().find(|p| p["id"] == *id) else {
                    return audit_not_found("page", id);
                };
                if let Some(archived) = body["archived"].as_bool() {
                    page["archived"] = json!(archived);
                }
                if let Some(icon) = body.get("icon") {
                    page["icon"] = icon.clone();
                }
                if let Some(props) = body["properties"].as_object() {
                    for (name, value) in props {
                        let mut value = value.clone();
                        audit_fill_plain_text(&mut value);
                        let kind = value.as_object().and_then(|m| m.keys().next().cloned());
                        let slot = &mut page["properties"][name];
                        if !slot.is_object() {
                            *slot = json!({});
                        }
                        if let Some(fields) = value.as_object() {
                            for (k, v) in fields {
                                slot[k] = v.clone();
                            }
                        }
                        if let (Some(kind), None) = (kind, slot.get("type")) {
                            slot["type"] = json!(kind);
                        }
                    }
                }
                Json(page.clone()).into_response()
            }
            ("GET", ["blocks", id]) => {
                let found = mock
                    .children
                    .lock()
                    .unwrap()
                    .values()
                    .flat_map(|list| list.iter())
                    .find(|b| b["id"] == *id)
                    .cloned();
                match found {
                    Some(block) => Json(mock.with_has_children(block)).into_response(),
                    None => audit_not_found("block", id),
                }
            }
            ("PATCH", ["blocks", id]) => {
                let mut map = mock.children.lock().unwrap();
                let Some(block) = map
                    .values_mut()
                    .flat_map(|list| list.iter_mut())
                    .find(|b| b["id"] == *id)
                else {
                    return audit_not_found("block", id);
                };
                let kind = block["type"].as_str().unwrap_or("paragraph").to_string();
                if let Some(fields) = body[&kind].as_object() {
                    for (k, v) in fields {
                        let mut v = v.clone();
                        audit_fill_plain_text(&mut v);
                        block[&kind][k] = v;
                    }
                }
                Json(block.clone()).into_response()
            }
            ("DELETE", ["blocks", id]) => {
                let mut map = mock.children.lock().unwrap();
                let mut removed = None;
                for list in map.values_mut() {
                    if let Some(pos) = list.iter().position(|b| b["id"] == *id) {
                        removed = Some(list.remove(pos));
                        break;
                    }
                }
                match removed {
                    Some(mut block) => {
                        map.remove(*id);
                        block["archived"] = json!(true);
                        Json(block).into_response()
                    }
                    None => audit_not_found("block", id),
                }
            }
            ("GET", ["blocks", id, "children"]) => {
                if mock.page(id).is_none() && !mock.block_exists(id) {
                    return audit_not_found("block", id);
                }
                let list: Vec<Value> = mock
                    .children_of(id)
                    .into_iter()
                    .map(|b| mock.with_has_children(b))
                    .collect();
                Json(audit_paginate(list, page_size(100), cursor.as_deref())).into_response()
            }
            ("PATCH", ["blocks", id, "children"]) => {
                if mock.page(id).is_none() && !mock.block_exists(id) {
                    return audit_not_found("block", id);
                }
                let after = body["after"].as_str();
                if let Some(anchor) = after {
                    if !mock.children_of(id).iter().any(|b| b["id"] == anchor) {
                        return audit_not_found("block", anchor);
                    }
                }
                let created: Vec<Value> = body["children"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|child| mock.store_block(child))
                    .collect();
                let mut map = mock.children.lock().unwrap();
                let list = map.entry(id.to_string()).or_default();
                let at = match after {
                    Some(anchor) => list.iter().position(|b| b["id"] == anchor).map(|p| p + 1),
                    None if body.pointer("/position/type") == Some(&json!("start")) => Some(0),
                    None => None,
                }
                .unwrap_or(list.len());
                for (offset, block) in created.iter().enumerate() {
                    list.insert(at + offset, block.clone());
                }
                Json(json!({ "object": "list", "results": created })).into_response()
            }
            ("GET", ["databases", id]) => {
                match mock.databases.lock().unwrap().iter().find(|d| d["id"] == *id).cloned() {
                    Some(db) => Json(db).into_response(),
                    None => audit_not_found("database", id),
                }
            }
            ("POST", ["databases", id, "query"]) => {
                let rows: Vec<Value> = mock
                    .pages
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|p| p.pointer("/parent/database_id").and_then(Value::as_str) == Some(*id))
                    .filter(|p| p["archived"] != json!(true))
                    .cloned()
                    .collect();
                Json(audit_paginate(rows, page_size(100), cursor.as_deref())).into_response()
            }
            ("GET", ["comments"]) => {
                let block_id = params.get("block_id").cloned().unwrap_or_default();
                let list: Vec<Value> = mock
                    .comments
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c.pointer("/parent/page_id").and_then(Value::as_str) == Some(block_id.as_str()))
                    .cloned()
                    .collect();
                Json(audit_paginate(list, page_size(100), cursor.as_deref())).into_response()
            }
            ("POST", ["comments"]) => {
                let mut rich_text = body.get("rich_text").cloned().unwrap_or_else(|| json!([]));
                audit_fill_plain_text(&mut rich_text);
                let comment = json!({
                    "object": "comment",
                    "id": mock.new_id("c033"),
                    "parent": body.get("parent").cloned().unwrap_or(Value::Null),
                    "discussion_id": body["discussion_id"].as_str().unwrap_or("disc-audit-1"),
                    "created_time": "2026-11-22T10:05:00.000Z",
                    "created_by": { "id": "bot-audit-1", "name": "Audit Bot" },
                    "rich_text": rich_text
                });
                mock.comments.lock().unwrap().push(comment.clone());
                Json(comment).into_response()
            }
            _ => audit_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_url",
                format!("AuditMock has no route for {method} {path}"),
            ),
        }
    }
}

struct AuditEnv {
    mock: AuditMock,
    cfg: AppConfig,
    auth: AuthManager,
    client: NotionClient,
    storage: Storage,
    server: tokio::task::JoinHandle<()>,
}

async fn start_audit_env() -> AuditEnv {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let mock = AuditMock::default();
    let app = Router::new().fallback(AuditMock::handle).with_state(mock.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let cfg = test_config(&base_url);
    let auth = AuthManager::from_token(cfg.clone(), "ntn_audit_token");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    AuditEnv {
        mock,
        cfg,
        auth,
        client,
        storage: Storage::open_in_memory().unwrap(),
        server,
    }
}

/// Serves the HTTP API for an AuditEnv on a random loopback port.
async fn start_audit_api(
    env: &AuditEnv,
    api_key: Option<&str>,
    options: serve::ServeOptions,
) -> (String, tokio::task::JoinHandle<()>) {
    let state = Arc::new(serve::ApiState {
        config: env.cfg.clone(),
        auth: env.auth.clone(),
        client: env.client.clone(),
        storage: env.storage.clone(),
        api_key: api_key.map(str::to_owned),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, serve::build_router_with(state, options)).await;
    });
    (base, server)
}

const AUDIT_PAGE_A: &str = "a11d0000-0000-4000-8000-00000000000a";
const AUDIT_PAGE_B: &str = "a11d0000-0000-4000-8000-00000000000b";
const AUDIT_DB: &str = "a11d0000-0000-4000-8000-0000000000db";

#[tokio::test]
async fn regression_s01_serve_rejects_cross_origin_and_foreign_host() {
    let env = start_audit_env().await;
    env.mock.add_page(audit_page(
        AUDIT_PAGE_A,
        "Launch Plan",
        json!({ "type": "workspace", "workspace": true }),
        "2026-11-20T02:00:00.000Z",
    ));
    let (api, api_server) = start_audit_api(&env, Some("local-secret"), serve::ServeOptions::default()).await;
    let http = reqwest::Client::new();
    let forbidden = |resp: &reqwest::Response| {
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "a rejected request must not carry CORS headers"
        );
    };

    // 1. Preflight from a foreign origin: rejected, and never answered with CORS headers.
    let preflight = http
        .request(reqwest::Method::OPTIONS, format!("{api}/api/v1/pages"))
        .header("Origin", "https://evil.example")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type,x-api-key")
        .send()
        .await
        .unwrap();
    forbidden(&preflight);
    assert!(preflight.headers().get("access-control-allow-methods").is_none());

    // 2. A simple cross-origin GET, even with the right key.
    let cross_get = http
        .get(format!("{api}/api/v1/search?q=x"))
        .header("Origin", "https://evil.example")
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap();
    forbidden(&cross_get);
    let body: Value = cross_get.json().await.unwrap();
    assert_eq!(body, json!({ "ok": false, "error": "Forbidden origin" }));

    // 3. DNS rebinding: the socket is loopback but the Host header names another site.
    for host in ["evil.example", "127.0.0.1.evil.example:8791", "localhost.evil.example"] {
        let rebound = http
            .get(format!("{api}/api/v1/whoami"))
            .header("Host", host)
            .header("x-api-key", "local-secret")
            .send()
            .await
            .unwrap();
        forbidden(&rebound);
        let body: Value = rebound.json().await.unwrap();
        assert_eq!(body, json!({ "ok": false, "error": "Forbidden host" }), "{host}");
    }
    let health_rebound = http
        .get(format!("{api}/health"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    forbidden(&health_rebound);

    // 4. A body-less cross-site POST needs no preflight; it must not reach Notion.
    let searches_before = env.mock.count("POST", "/v1/search");
    for site in ["cross-site", "same-site"] {
        let cross_post = http
            .post(format!("{api}/api/v1/sync"))
            .header("Sec-Fetch-Site", site)
            .header("x-api-key", "local-secret")
            .send()
            .await
            .unwrap();
        forbidden(&cross_post);
    }
    assert_eq!(env.mock.count("POST", "/v1/search"), searches_before);
    assert!(env.mock.requests.lock().unwrap().is_empty(), "no guarded request may reach Notion");

    // 5. The same calls from a local client with the key work, with no CORS headers at all.
    let health = http.get(format!("{api}/health")).send().await.unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let missing_key = http.get(format!("{api}/api/v1/whoami")).send().await.unwrap();
    assert_eq!(missing_key.status(), StatusCode::UNAUTHORIZED);
    let wrong_key = http
        .get(format!("{api}/api/v1/whoami"))
        .header("Authorization", "Bearer local-secreT")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_key.status(), StatusCode::UNAUTHORIZED);
    let whoami = http
        .get(format!("{api}/api/v1/whoami"))
        .header("Authorization", "Bearer local-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(whoami.status(), StatusCode::OK);
    assert!(whoami.headers().get("access-control-allow-origin").is_none());
    let sync = http
        .post(format!("{api}/api/v1/sync"))
        .header("x-api-key", "local-secret")
        .header("Sec-Fetch-Site", "none")
        .send()
        .await
        .unwrap();
    assert_eq!(sync.status(), StatusCode::OK);
    let sync_body: Value = sync.json().await.unwrap();
    assert_eq!(sync_body["pages_count"], 1);
    api_server.abort();

    // 6. --cors-origin opts in exactly one origin; --allowed-host one extra Host name.
    let (cors_api, cors_server) = start_audit_api(
        &env,
        Some("local-secret"),
        serve::ServeOptions {
            allowed_hosts: vec!["notion.internal".into()],
            cors_origins: vec!["https://app.example.com".into()],
            enforce_host: true,
        },
    )
    .await;
    let allowed_preflight = http
        .request(reqwest::Method::OPTIONS, format!("{cors_api}/api/v1/pages"))
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "content-type,x-api-key")
        .send()
        .await
        .unwrap();
    assert!(allowed_preflight.status().is_success());
    assert_eq!(
        allowed_preflight.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    let allowed_get = http
        .get(format!("{cors_api}/api/v1/whoami"))
        .header("Origin", "https://app.example.com")
        .header("Sec-Fetch-Site", "cross-site")
        .header("Host", "notion.internal:8791")
        .header("x-api-key", "local-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_get.status(), StatusCode::OK);
    assert_eq!(
        allowed_get.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    // The opt-in does not remove the key requirement.
    let allowed_origin_no_key = http
        .get(format!("{cors_api}/api/v1/whoami"))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_origin_no_key.status(), StatusCode::UNAUTHORIZED);
    for origin in ["https://evil.example", "https://app.example.com.evil.example", "null"] {
        let other = http
            .get(format!("{cors_api}/api/v1/whoami"))
            .header("Origin", origin)
            .header("x-api-key", "local-secret")
            .send()
            .await
            .unwrap();
        forbidden(&other);
    }

    cors_server.abort();
    env.server.abort();
}

#[tokio::test]
async fn regression_n14_peek_url_archives_row_not_parent() {
    let env = start_audit_env().await;
    env.mock.databases.lock().unwrap().push(audit_database(
        AUDIT_DB,
        "Sprint Tasks",
        "2026-11-20T02:00:00.000Z",
    ));
    for (id, title) in [(AUDIT_PAGE_A, "Row A"), (AUDIT_PAGE_B, "Row B")] {
        env.mock.add_page(audit_page(
            id,
            title,
            json!({ "type": "database_id", "database_id": AUDIT_DB }),
            "2026-11-20T02:00:00.000Z",
        ));
    }
    let compact = |id: &str| id.replace('-', "");
    let view = "00000000000000000000000000000099";

    // A side-peek URL: the path is the database, p= is the row that is open.
    let peek = format!(
        "https://www.notion.so/acme/{}?v={view}&p={}&pm=s",
        compact(AUDIT_DB),
        compact(AUDIT_PAGE_B)
    );
    let archived = notion_cli::cli::op_archive_page(&env.client, &env.storage, &peek, true)
        .await
        .unwrap();
    assert_eq!(archived.id, AUDIT_PAGE_B);
    let patches = env.mock.requests_matching("PATCH", "/v1/pages/");
    assert_eq!(patches.len(), 1, "{patches:?}");
    assert_eq!(patches[0].path, format!("/v1/pages/{AUDIT_PAGE_B}"));
    assert_eq!(patches[0].body, json!({ "archived": true }));
    assert_eq!(env.mock.page(AUDIT_PAGE_B).unwrap()["archived"], true);
    assert_eq!(env.mock.page(AUDIT_PAGE_A).unwrap()["archived"], false);
    assert_eq!(env.mock.count("POST", "/v1/search"), 0, "URL input needs no lookup");

    // Database commands given the same URL still target the database in the path.
    env.mock.clear_requests();
    let rows = notion_cli::cli::op_query_database(
        &env.client,
        &env.storage,
        &peek,
        None,
        None,
        20,
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.items.len(), 1, "the archived row is gone, the other remains");
    assert_eq!(env.mock.count("POST", &format!("/v1/databases/{AUDIT_DB}/query")), 1);

    // Trailing slash and hyphenated UUID forms resolve to the page in the path.
    env.mock.clear_requests();
    for url in [
        format!("https://www.notion.so/acme/Row-A-{}/", compact(AUDIT_PAGE_A)),
        format!("https://www.notion.so/acme/Row-A-{AUDIT_PAGE_A}?pvs=4#top"),
    ] {
        let page = notion_cli::cli::op_archive_page(&env.client, &env.storage, &url, true)
            .await
            .unwrap();
        assert_eq!(page.id, AUDIT_PAGE_A, "{url}");
    }
    assert_eq!(env.mock.count("PATCH", &format!("/v1/pages/{AUDIT_PAGE_A}")), 2);
    env.server.abort();
}
