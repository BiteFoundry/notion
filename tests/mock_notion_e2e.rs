//! End-to-end integration test suite against a local Mock Axum Notion REST API v1 server.
//!
//! Verifies:
//! 1. Notion REST API client (users/me, search with pagination, pages CRUD, recursive blocks,
//!    to_do toggle PATCH, database query + item create/update, comments, 429 Retry-After, media download)
//! 2. SQLite WAL + FTS5 cache + Korean `<3` char LIKE fallback
//! 3. Local HTTP API server (`src/serve.rs`, `:8791` contract + API key auth)
//! 4. Stdio MCP JSON-RPC 2.0 server (`src/mcp.rs`, all 18 tools + `archive_page` confirm guard)
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
                        "rich_text": [{ "plain_text": "Ship @shinick/notion v0.1.0" }]
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
                                "rich_text": [{ "plain_text": "Ship @shinick/notion v0.1.0" }]
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
                            .unwrap_or("Ship @shinick/notion v0.1.0");
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
    let rows = notion_cli::cli::op_query_database(&client, &storage, "db-tasks", None, None, 20, false)
        .await
        .unwrap();
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
async fn e2e_http_api_server_and_all_18_mcp_tools() {
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

    // --- Part B: MCP JSON-RPC 2.0 Server (18 Tools) ---
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
    assert_eq!(list_tools["result"]["tools"].as_array().unwrap().len(), 18);

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
