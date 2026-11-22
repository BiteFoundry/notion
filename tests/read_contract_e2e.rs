use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    extract::{Path as AxumPath, Query as AxumQuery, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use notion_cli::{
    auth::AuthManager,
    config::{AppConfig, WorkspaceProfile},
    mcp,
    notion::NotionClient,
    serve,
    storage::Storage,
};
use serde_json::{json, Value};
use tokio::{net::TcpListener, process::Command};

fn notion_cli_cmd(base_url: &str, cfg_dir: &Path, data_dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_notion"));
    cmd.env("NOTION_API_BASE_URL", format!("{base_url}/v1"))
        .env("NOTION_TOKEN", "ntn_mock_token")
        .env("NOTION_CONFIG_DIR", cfg_dir)
        .env("NOTION_DATA_DIR", data_dir)
        .env("NOTION_DISABLE_KEYRING", "1")
        .env("NOTION_CLI_CACHE_SECS", "0");
    cmd
}

#[derive(Clone, Copy)]
enum CursorFault {
    None,
    MissingNextCursor,
    RepeatedCursor,
}

#[derive(Clone)]
struct ReadContractMock {
    pages: Arc<Mutex<HashMap<String, Value>>>,
    databases: Arc<Mutex<HashMap<String, Value>>>,
    blocks: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    comment_counts: Arc<Mutex<HashMap<String, usize>>>,
    db_rows: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    search_items: Arc<Mutex<Vec<Value>>>,
    users: Arc<Mutex<Vec<Value>>>,
    search_reqs: Arc<AtomicUsize>,
    page_get_reqs: Arc<AtomicUsize>,
    comment_reqs: Arc<AtomicUsize>,
    block_reqs: Arc<AtomicUsize>,
    block_patch_reqs: Arc<Mutex<Vec<(String, Value)>>>,
    page_patch_reqs: Arc<Mutex<Vec<(String, Value)>>>,
    /// Optional cap on `/v1/search` page size (0 = honor client `page_size`).
    search_page_limit: Arc<AtomicUsize>,
    /// When true, `/v1/search` filters by query substring/id and `filter.value`.
    filter_search: Arc<std::sync::atomic::AtomicBool>,
    /// 1-based request number on which POST /v1/search returns HTTP 400 once (0 = disabled).
    fail_search_on_req: Arc<AtomicUsize>,
    search_cursor_fault: Arc<Mutex<CursorFault>>,
}

impl Default for ReadContractMock {
    fn default() -> Self {
        Self {
            pages: Arc::new(Mutex::new(HashMap::new())),
            databases: Arc::new(Mutex::new(HashMap::new())),
            blocks: Arc::new(Mutex::new(HashMap::new())),
            comment_counts: Arc::new(Mutex::new(HashMap::new())),
            db_rows: Arc::new(Mutex::new(HashMap::new())),
            search_items: Arc::new(Mutex::new(Vec::new())),
            users: Arc::new(Mutex::new(vec![json!({
                "object": "user",
                "id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1",
                "type": "person",
                "name": "Alice Kim",
                "person": { "email": "alice@example.com" }
            })])),
            search_reqs: Arc::new(AtomicUsize::new(0)),
            page_get_reqs: Arc::new(AtomicUsize::new(0)),
            comment_reqs: Arc::new(AtomicUsize::new(0)),
            block_reqs: Arc::new(AtomicUsize::new(0)),
            block_patch_reqs: Arc::new(Mutex::new(Vec::new())),
            page_patch_reqs: Arc::new(Mutex::new(Vec::new())),
            search_page_limit: Arc::new(AtomicUsize::new(0)),
            filter_search: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_search_on_req: Arc::new(AtomicUsize::new(0)),
            search_cursor_fault: Arc::new(Mutex::new(CursorFault::None)),
        }
    }
}

fn page_obj(id: &str, title: &str, edited: &str) -> Value {
    json!({
        "object": "page",
        "id": id,
        "url": format!("https://www.notion.so/{id}"),
        "parent": { "type": "workspace", "workspace": true },
        "archived": false,
        "created_time": "2026-01-01T00:00:00.000Z",
        "last_edited_time": edited,
        "properties": {
            "title": { "type": "title", "title": [{ "plain_text": title }] }
        }
    })
}

fn db_obj(id: &str, title: &str, edited: &str) -> Value {
    json!({
        "object": "database",
        "id": id,
        "url": format!("https://www.notion.so/{id}"),
        "parent": { "type": "workspace", "workspace": true },
        "archived": false,
        "last_edited_time": edited,
        "title": [{ "plain_text": title }],
        "description": [{ "plain_text": "Database description" }],
        "properties": {
            "Name": { "id": "title", "type": "title", "title": {} },
            "Status": { "id": "status", "type": "status", "status": { "options": [{ "name": "Done", "color": "green" }, { "name": "Todo", "color": "gray" }] } }
        }
    })
}

fn db_row_obj(id: &str, db_id: &str, title: &str, status: &str, edited: &str) -> Value {
    json!({
        "object": "page",
        "id": id,
        "url": format!("https://www.notion.so/{id}"),
        "parent": { "type": "database_id", "database_id": db_id },
        "archived": false,
        "created_time": "2026-01-01T00:00:00.000Z",
        "last_edited_time": edited,
        "properties": {
            "Name": { "type": "title", "title": [{ "plain_text": title }] },
            "Status": { "type": "status", "status": { "name": status, "color": "green" } }
        }
    })
}

fn paragraph_block(id: &str, text: &str) -> Value {
    json!({
        "object": "block",
        "id": id,
        "type": "paragraph",
        "has_children": false,
        "paragraph": {
            "rich_text": [{ "type": "text", "text": { "content": text }, "plain_text": text }]
        }
    })
}

fn heading2_block(id: &str, text: &str) -> Value {
    json!({
        "object": "block",
        "id": id,
        "type": "heading_2",
        "has_children": false,
        "heading_2": {
            "rich_text": [{ "type": "text", "text": { "content": text }, "plain_text": text }]
        }
    })
}

fn toggle_block(id: &str, text: &str, has_children: bool) -> Value {
    json!({
        "object": "block",
        "id": id,
        "type": "toggle",
        "has_children": has_children,
        "toggle": {
            "rich_text": [{ "type": "text", "text": { "content": text }, "plain_text": text }]
        }
    })
}

fn comment_obj(page_id: &str, idx: usize) -> Value {
    let secs = idx % 60;
    let mins = (idx / 60) % 60;
    let hrs = (idx / 3600) % 24;
    json!({
        "object": "comment",
        "id": format!("mc-{idx}"),
        "parent": { "type": "page_id", "page_id": page_id },
        "discussion_id": format!("d-{idx}"),
        "created_by": { "object": "user", "id": "u-bot", "name": "Bot" },
        "created_time": format!("2026-01-01T{hrs:02}:{mins:02}:{secs:02}.000Z"),
        "rich_text": [{ "type": "text", "text": { "content": format!("c{idx}") }, "plain_text": format!("c{idx}") }]
    })
}

fn standard_mock() -> ReadContractMock {
    let mock = ReadContractMock::default();
    let roadmap = page_obj("page-roadmap", "Roadmap", "2026-11-01T00:00:00.000Z");
    let empty = page_obj("page-empty", "Empty Page", "2026-11-01T00:00:00.000Z");
    let cmt_err = page_obj(
        "page-cmt-err",
        "Restricted Comments",
        "2026-11-01T00:00:00.000Z",
    );
    let many = page_obj("page-manycmt", "Many Comments", "2026-11-01T00:00:00.000Z");
    let exact2000 = page_obj(
        "page-2000",
        "Exact 2000 Comments",
        "2026-11-01T00:00:00.000Z",
    );
    let exact2001 = page_obj(
        "page-2001",
        "Exact 2001 Comments",
        "2026-11-01T00:00:00.000Z",
    );
    let wide = page_obj("page-wide", "Wide Page", "2026-11-01T00:00:00.000Z");
    let deep = page_obj("page-deep", "Deep", "2026-11-01T00:00:00.000Z");
    let shallow6 = page_obj("page-shallow6", "Shallow6", "2026-11-01T00:00:00.000Z");
    let tasks_db = db_obj("db-tasks", "Tasks", "2026-11-01T00:00:00.000Z");
    let notes_db = db_obj("db-notes", "Notes", "2026-10-31T00:00:00.000Z");

    {
        let mut pages = mock.pages.lock().unwrap();
        for p in [
            &roadmap, &empty, &cmt_err, &many, &exact2000, &exact2001, &wide, &deep, &shallow6,
        ] {
            pages.insert(p["id"].as_str().unwrap().into(), p.clone());
        }
    }
    {
        let mut dbs = mock.databases.lock().unwrap();
        dbs.insert("db-tasks".into(), tasks_db.clone());
        dbs.insert("db-notes".into(), notes_db.clone());
    }
    {
        let mut blocks = mock.blocks.lock().unwrap();
        blocks.insert(
            "page-roadmap".into(),
            vec![
                heading2_block("blk-rm-1", "Goals"),
                paragraph_block("blk-rm-2", "Ship v1.0"),
            ],
        );
        blocks.insert("page-empty".into(), Vec::new());
        blocks.insert(
            "page-cmt-err".into(),
            vec![paragraph_block("blk-ce-1", "Restricted body")],
        );
        blocks.insert(
            "page-manycmt".into(),
            vec![paragraph_block("blk-mc-1", "Many comments body")],
        );
        blocks.insert(
            "page-2000".into(),
            vec![paragraph_block("blk-2k-1", "2000 comments body")],
        );
        blocks.insert(
            "page-2001".into(),
            vec![paragraph_block("blk-2k1-1", "2001 comments body")],
        );
        blocks.insert(
            "page-wide".into(),
            (0..120)
                .map(|i| paragraph_block(&format!("blk-wide-{i:03}"), &format!("Wide block {i}")))
                .collect(),
        );

        // 14 nested levels: page-deep -> blk-depth-1 .. blk-depth-13 (toggle) -> blk-depth-14 (HIDDEN)
        blocks.insert(
            "page-deep".into(),
            vec![toggle_block("blk-depth-1", "Level 1", true)],
        );
        for level in 1..=12 {
            blocks.insert(
                format!("blk-depth-{level}"),
                vec![toggle_block(
                    &format!("blk-depth-{}", level + 1),
                    &format!("Level {}", level + 1),
                    true,
                )],
            );
        }
        blocks.insert(
            "blk-depth-13".into(),
            vec![paragraph_block("blk-depth-14", "HIDDEN")],
        );

        // 6-level control: page-shallow6 -> s-1 .. s-5 (toggle) -> s-6 (LEVEL6_OK)
        blocks.insert(
            "page-shallow6".into(),
            vec![toggle_block("s-1", "S1", true)],
        );
        for level in 1..=4 {
            blocks.insert(
                format!("s-{level}"),
                vec![toggle_block(
                    &format!("s-{}", level + 1),
                    &format!("S{}", level + 1),
                    true,
                )],
            );
        }
        blocks.insert("s-5".into(), vec![paragraph_block("s-6", "LEVEL6_OK")]);
    }
    {
        let mut counts = mock.comment_counts.lock().unwrap();
        counts.insert("page-roadmap".into(), 2);
        counts.insert("page-empty".into(), 0);
        counts.insert("page-manycmt".into(), 2100);
        counts.insert("page-2000".into(), 2000);
        counts.insert("page-2001".into(), 2001);
        counts.insert("page-deep".into(), 0);
        counts.insert("page-shallow6".into(), 0);
    }
    {
        let mut rows = mock.db_rows.lock().unwrap();
        rows.insert(
            "db-tasks".into(),
            vec![
                db_row_obj(
                    "row-1",
                    "db-tasks",
                    "Task 1",
                    "Done",
                    "2026-11-03T00:00:00.000Z",
                ),
                db_row_obj(
                    "row-2",
                    "db-tasks",
                    "Task 2",
                    "Todo",
                    "2026-11-02T00:00:00.000Z",
                ),
                db_row_obj(
                    "row-3",
                    "db-tasks",
                    "Task 3",
                    "Todo",
                    "2026-11-01T00:00:00.000Z",
                ),
            ],
        );
    }
    {
        let mut items = mock.search_items.lock().unwrap();
        items.push(roadmap);
        items.push(empty);
        items.push(cmt_err);
        items.push(many);
        items.push(exact2000);
        items.push(exact2001);
        items.push(wide);
        items.push(deep);
        items.push(shallow6);
        items.push(tasks_db);
        items.push(notes_db);
    }
    mock
}

fn parse_cursor_offset(cursor: Option<&str>) -> usize {
    cursor
        .and_then(|c| c.strip_prefix("cur-").unwrap_or(c).parse::<usize>().ok())
        .unwrap_or(0)
}

async fn start_mock_server(state: ReadContractMock) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route(
            "/v1/users/me",
            get(|| async {
                Json(json!({
                    "object": "user",
                    "id": "bot-1",
                    "type": "bot",
                    "name": "Mock Bot",
                    "bot": { "workspace_name": "Acme" }
                }))
            }),
        )
        .route(
            "/v1/users",
            get(|State(st): State<ReadContractMock>| async move {
                let users = st.users.lock().unwrap().clone();
                Json(json!({
                    "object": "list",
                    "results": users,
                    "has_more": false,
                    "next_cursor": Value::Null
                }))
            }),
        )
        .route(
            "/v1/search",
            post(
                |State(st): State<ReadContractMock>, Json(body): Json<Value>| async move {
                    let req_no = st.search_reqs.fetch_add(1, Ordering::SeqCst) + 1;
                    let fail_on = st.fail_search_on_req.load(Ordering::SeqCst);
                    if fail_on != 0 && req_no == fail_on {
                        st.fail_search_on_req.store(0, Ordering::SeqCst);
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "object": "error",
                                "status": 400,
                                "code": "validation_error",
                                "message": "Injected search failure"
                            })),
                        )
                            .into_response();
                    }
                    let mut page_size = body
                        .get("page_size")
                        .and_then(Value::as_u64)
                        .unwrap_or(100)
                        .clamp(1, 100) as usize;
                    let cap = st.search_page_limit.load(Ordering::SeqCst);
                    if cap > 0 {
                        page_size = page_size.min(cap);
                    }
                    let start_cursor = body.get("start_cursor").and_then(Value::as_str);
                    let fault = *st.search_cursor_fault.lock().unwrap();
                    let raw_items = st.search_items.lock().unwrap().clone();
                    let items: Vec<Value> = if st.filter_search.load(Ordering::SeqCst) {
                        let q = body
                            .get("query")
                            .and_then(Value::as_str)
                            .map(|s| s.to_ascii_lowercase());
                        let obj_filter = body
                            .pointer("/filter/value")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        raw_items
                            .into_iter()
                            .filter(|item| {
                                if let Some(ref want) = obj_filter {
                                    if item.get("object").and_then(Value::as_str) != Some(want.as_str()) {
                                        return false;
                                    }
                                }
                                if let Some(ref q) = q {
                                    let id = item
                                        .get("id")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_ascii_lowercase();
                                    let title = item
                                        .pointer("/properties/title/title/0/plain_text")
                                        .or_else(|| item.pointer("/properties/Name/title/0/plain_text"))
                                        .or_else(|| item.pointer("/title/0/plain_text"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .to_ascii_lowercase();
                                    id == *q || title.contains(q)
                                } else {
                                    true
                                }
                            })
                            .collect()
                    } else {
                        raw_items
                    };
                    let offset = parse_cursor_offset(start_cursor);
                    let end = (offset + page_size).min(items.len());
                    let slice = if offset < items.len() {
                        items[offset..end].to_vec()
                    } else {
                        Vec::new()
                    };
                    let mut has_more = end < items.len();
                    let mut next_cursor = has_more.then(|| format!("cur-{end}"));
                    match fault {
                        CursorFault::None => {}
                        CursorFault::MissingNextCursor => {
                            has_more = true;
                            next_cursor = None;
                        }
                        CursorFault::RepeatedCursor => {
                            has_more = true;
                            next_cursor = Some(
                                start_cursor
                                    .map(str::to_string)
                                    .unwrap_or_else(|| "cur-stuck".into()),
                            );
                        }
                    }
                    Json(json!({
                        "object": "list",
                        "results": slice,
                        "has_more": has_more,
                        "next_cursor": next_cursor
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1/pages/{id}",
            get(
                |State(st): State<ReadContractMock>, AxumPath(id): AxumPath<String>| async move {
                    st.page_get_reqs.fetch_add(1, Ordering::SeqCst);
                    if let Some(p) = st.pages.lock().unwrap().get(&id).cloned() {
                        return Json(p).into_response();
                    }
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "code": "object_not_found", "message": "Not found" })),
                    )
                        .into_response()
                },
            )
            .patch(
                |State(st): State<ReadContractMock>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.page_patch_reqs.lock().unwrap().push((id.clone(), body.clone()));
                    let mut pages = st.pages.lock().unwrap();
                    if let Some(p) = pages.get_mut(&id) {
                        if let Some(arch) = body.get("archived").and_then(Value::as_bool) {
                            p["archived"] = json!(arch);
                        }
                        if let Some(props) = body.get("properties").and_then(Value::as_object) {
                            if let Some(target) = p.get_mut("properties").and_then(Value::as_object_mut) {
                                for (k, v) in props {
                                    target.insert(k.clone(), v.clone());
                                }
                            }
                        }
                        return Json(p.clone()).into_response();
                    }
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "code": "object_not_found", "message": "Not found" })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/databases/{id}",
            get(
                |State(st): State<ReadContractMock>, AxumPath(id): AxumPath<String>| async move {
                    if let Some(d) = st.databases.lock().unwrap().get(&id).cloned() {
                        return Json(d).into_response();
                    }
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "code": "object_not_found", "message": "Not found" })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/v1/blocks/{id}/children",
            get(
                |State(st): State<ReadContractMock>,
                 AxumPath(id): AxumPath<String>,
                 AxumQuery(q): AxumQuery<HashMap<String, String>>| async move {
                    st.block_reqs.fetch_add(1, Ordering::SeqCst);
                    let page_size = q
                        .get("page_size")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(100)
                        .clamp(1, 100);
                    let offset = parse_cursor_offset(q.get("start_cursor").map(String::as_str));
                    let all = st
                        .blocks
                        .lock()
                        .unwrap()
                        .get(&id)
                        .cloned()
                        .unwrap_or_default();
                    let end = (offset + page_size).min(all.len());
                    let slice = if offset < all.len() {
                        all[offset..end].to_vec()
                    } else {
                        Vec::new()
                    };
                    let has_more = end < all.len();
                    let next_cursor = has_more.then(|| format!("cur-{end}"));
                    Json(json!({
                        "object": "list",
                        "results": slice,
                        "has_more": has_more,
                        "next_cursor": next_cursor
                    }))
                    .into_response()
                },
            )
            .patch(
                |State(st): State<ReadContractMock>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    st.block_patch_reqs
                        .lock()
                        .unwrap()
                        .push((id.clone(), body.clone()));
                    let mut created = Vec::new();
                    if let Some(children) = body.get("children").and_then(Value::as_array) {
                        let mut blocks = st.blocks.lock().unwrap();
                        let entry = blocks.entry(id.clone()).or_default();
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
            "/v1/comments",
            get(
                |State(st): State<ReadContractMock>,
                 AxumQuery(q): AxumQuery<HashMap<String, String>>| async move {
                    st.comment_reqs.fetch_add(1, Ordering::SeqCst);
                    let block_id = q.get("block_id").cloned().unwrap_or_default();
                    if block_id == "page-cmt-err" {
                        return (
                            StatusCode::FORBIDDEN,
                            Json(json!({
                                "object": "error",
                                "status": 403,
                                "code": "restricted_resource",
                                "message": "Insufficient permissions for comments"
                            })),
                        )
                            .into_response();
                    }
                    let total = st
                        .comment_counts
                        .lock()
                        .unwrap()
                        .get(&block_id)
                        .copied()
                        .unwrap_or(0);
                    let page_size = q
                        .get("page_size")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(100)
                        .clamp(1, 100);
                    let offset = parse_cursor_offset(q.get("start_cursor").map(String::as_str));
                    let end = (offset + page_size).min(total);
                    let slice: Vec<Value> =
                        (offset..end).map(|i| comment_obj(&block_id, i)).collect();
                    let has_more = end < total;
                    let next_cursor = has_more.then(|| format!("cur-{end}"));
                    Json(json!({
                        "object": "list",
                        "results": slice,
                        "has_more": has_more,
                        "next_cursor": next_cursor
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1/databases/{id}/query",
            post(
                |State(st): State<ReadContractMock>,
                 AxumPath(id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    let all = st
                        .db_rows
                        .lock()
                        .unwrap()
                        .get(&id)
                        .cloned()
                        .unwrap_or_default();
                    let filtered: Vec<Value> = if body.get("filter").is_some() {
                        all.into_iter()
                            .filter(|r| {
                                r.pointer("/properties/Status/status/name")
                                    .and_then(Value::as_str)
                                    == Some("Done")
                            })
                            .collect()
                    } else {
                        all
                    };
                    let page_size = body
                        .get("page_size")
                        .and_then(Value::as_u64)
                        .unwrap_or(100)
                        .clamp(1, 100) as usize;
                    let offset =
                        parse_cursor_offset(body.get("start_cursor").and_then(Value::as_str));
                    let end = (offset + page_size).min(filtered.len());
                    let slice = if offset < filtered.len() {
                        filtered[offset..end].to_vec()
                    } else {
                        Vec::new()
                    };
                    let has_more = end < filtered.len();
                    let next_cursor = has_more.then(|| format!("cur-{end}"));
                    Json(json!({
                        "object": "list",
                        "results": slice,
                        "has_more": has_more,
                        "next_cursor": next_cursor
                    }))
                    .into_response()
                },
            ),
        )
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

fn mcp_runtime(base_url: &str) -> (AppConfig, AuthManager, NotionClient, Storage) {
    let mut cfg = AppConfig::default();
    cfg.api_base_url = format!("{base_url}/v1");
    cfg.upsert_workspace(
        WorkspaceProfile {
            name: "default".into(),
            workspace_id: "default".into(),
            workspace_name: "Acme".into(),
            auth_mode: "integration".into(),
            api_base_url: Some(format!("{base_url}/v1")),
            ..Default::default()
        },
        true,
    );
    let auth = AuthManager::from_token(cfg.clone(), "ntn_mock_token");
    let client = NotionClient::new(&cfg, auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    (cfg, auth, client, storage)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_pages_get_json_defaults_to_compact_and_full_restores_detail() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // Default --json is compact
    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let compact_bytes = out.stdout.len();
    let compact: Value = serde_json::from_slice(&out.stdout).unwrap();
    let compact_keys: BTreeSet<&str> = compact
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        compact_keys,
        BTreeSet::from([
            "comments_state",
            "hint",
            "id",
            "incomplete_blocks",
            "last_edited_time",
            "markdown",
            "omitted",
            "title",
        ])
    );
    assert!(compact.get("comments").is_none());
    assert!(compact.get("comments_has_more").is_none());
    assert_eq!(
        compact["hint"],
        "notion pages get page-roadmap --json --full"
    );

    // --json --full restores full detail + incomplete_blocks + comments_has_more
    let out_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(
        out_full.status.success(),
        "{}",
        String::from_utf8_lossy(&out_full.stderr)
    );
    let full_bytes = out_full.stdout.len();
    let full: Value = serde_json::from_slice(&out_full.stdout).unwrap();
    assert!(full.get("page").is_some());
    assert!(full.get("blocks").is_some());
    assert_eq!(full["comments"].as_array().unwrap().len(), 2);
    assert_eq!(full["incomplete_blocks"], json!([]));
    assert_eq!(full["comments_has_more"], json!(false));

    eprintln!(
        "MEASURE_CLI pages_get_json_compact_bytes={} pages_get_json_full_bytes={}",
        compact_bytes, full_bytes
    );

    // --json --compact alias matches compact key set
    let out_alias = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json", "--compact"])
        .output()
        .await
        .unwrap();
    assert!(out_alias.status.success());
    let alias: Value = serde_json::from_slice(&out_alias.stdout).unwrap();
    let alias_keys: BTreeSet<&str> = alias
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(alias_keys, compact_keys);

    // Negative controls: exit 2
    let conflict = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-roadmap",
            "--json",
            "--compact",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));

    let no_json = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--full"])
        .output()
        .await
        .unwrap();
    assert_eq!(no_json.status.code(), Some(2));

    // Positive controls: text pages get and pages export unchanged
    let text_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap"])
        .output()
        .await
        .unwrap();
    assert!(text_out.status.success());
    assert!(String::from_utf8_lossy(&text_out.stdout).contains("## Goals"));

    let export_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-roadmap"])
        .output()
        .await
        .unwrap();
    assert!(export_out.status.success());
    assert!(String::from_utf8_lossy(&export_out.stdout).starts_with("# Roadmap\n\n"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_read_issues_no_comment_requests_and_keeps_comment_cache() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Cold cache compact read
    let out1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        out1.status.success(),
        "{}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let v1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 0);
    assert_eq!(v1["comments_state"], "not_fetched");
    assert!(v1["omitted"]["comments"].is_null());
    assert!(v1["markdown"].as_str().unwrap().contains("Ship v1.0"));

    // 2. Warm compact read reuses body marker (0 new block GETs, 0 comment GETs)
    let blocks_before = mock.block_reqs.load(Ordering::SeqCst);
    let out2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out2.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 0);
    assert_eq!(mock.block_reqs.load(Ordering::SeqCst), blocks_before);

    // 3. Full read fetches comments (1 comment request)
    let out3 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(out3.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 1);

    // 4. Compact read now sees cached count 2 without requesting comments
    let out4 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out4.status.success());
    let v4: Value = serde_json::from_slice(&out4.stdout).unwrap();
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 1);
    assert_eq!(v4["comments_state"], "cached");
    assert_eq!(v4["omitted"]["comments"], 2);

    // 5. Mutate last_edited_time and blocks; compact refetches blocks only, keeps cached comment state
    {
        mock.pages.lock().unwrap().insert(
            "page-roadmap".into(),
            page_obj("page-roadmap", "Roadmap", "2026-11-02T00:00:00.000Z"),
        );
        mock.blocks.lock().unwrap().insert(
            "page-roadmap".into(),
            vec![paragraph_block("blk-rm-new", "Ship v2.0")],
        );
    }
    let out5 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out5.status.success());
    let v5: Value = serde_json::from_slice(&out5.stdout).unwrap();
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), 1);
    assert!(v5["markdown"].as_str().unwrap().contains("Ship v2.0"));
    assert_eq!(v5["comments_state"], "cached");
    assert_eq!(v5["omitted"]["comments"], 2);

    // 6. Offline comments list still has the 2 cached comments
    let out6 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-roadmap", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        out6.status.success(),
        "{}",
        String::from_utf8_lossy(&out6.stderr)
    );
    let v6: Value = serde_json::from_slice(&out6.stdout).unwrap();
    assert_eq!(v6["count"], 2);

    // 7. 403 page: full records comments_error, compact reports failed without requesting comments, offline full keeps it
    let out7a = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(out7a.status.success());
    let full_err: Value = serde_json::from_slice(&out7a.stdout).unwrap();
    let expected_err = full_err["comments_error"].as_str().unwrap().to_string();
    assert!(
        expected_err.contains("403"),
        "expected 403 in {expected_err}"
    );

    let comment_reqs_after_full = mock.comment_reqs.load(Ordering::SeqCst);
    let out7b = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out7b.status.success());
    assert_eq!(
        mock.comment_reqs.load(Ordering::SeqCst),
        comment_reqs_after_full
    );
    let v7b: Value = serde_json::from_slice(&out7b.stdout).unwrap();
    assert_eq!(v7b["comments_state"], "failed");
    assert_eq!(v7b["comments_error"], expected_err);
    assert!(v7b["omitted"]["comments"].is_null());

    let out7c = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-cmt-err",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(out7c.status.success());
    let v7c: Value = serde_json::from_slice(&out7c.stdout).unwrap();
    assert_eq!(v7c["comments_error"], expected_err);

    // 8. MCP get_page on fresh storage issues 0 comment requests
    let comment_reqs_before_mcp = mock.comment_reqs.load(Ordering::SeqCst);
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    let mcp_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_page", "arguments": { "page": "page-roadmap" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_resp["result"]["isError"], false, "{mcp_resp}");
    assert_eq!(
        mock.comment_reqs.load(Ordering::SeqCst),
        comment_reqs_before_mcp
    );
    assert_eq!(
        mcp_resp["result"]["structuredContent"]["comments_state"],
        "not_fetched"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_tree_reads_13_levels_and_flags_the_14th() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-deep", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deep: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(deep["incomplete_blocks"], json!(["blk-depth-13"]));
    let md = deep["markdown"].as_str().unwrap();
    assert!(md.contains("Level 13"), "level 13 should be loaded: {md}");
    assert!(!md.contains("HIDDEN"), "level 14 must not be loaded: {md}");

    let export_path = tmp.path().join("deep.md");
    let export_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "export",
            "page-deep",
            "--out",
            export_path.to_str().unwrap(),
        ])
        .output()
        .await
        .unwrap();
    assert!(export_out.status.success());
    let stderr = String::from_utf8_lossy(&export_out.stderr);
    assert!(
        stderr.contains("Warning:"),
        "expected warning on stderr: {stderr}"
    );
    assert!(
        stderr.contains("blk-depth-13"),
        "expected blk-depth-13 on stderr: {stderr}"
    );

    // Direct lookup of blk-depth-13 loads HIDDEN
    let direct = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "blk-depth-13", "--json"])
        .output()
        .await
        .unwrap();
    assert!(direct.status.success());
    let direct_val: Value = serde_json::from_slice(&direct.stdout).unwrap();
    assert!(serde_json::to_string(&direct_val["blocks"])
        .unwrap()
        .contains("HIDDEN"));
    assert_eq!(direct_val["incomplete_blocks"], json!([]));

    // Control: 6-level tree loads completely with no warning
    let shallow_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-shallow6", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(shallow_out.status.success());
    let shallow: Value = serde_json::from_slice(&shallow_out.stdout).unwrap();
    assert_eq!(shallow["incomplete_blocks"], json!([]));
    assert!(shallow["markdown"].as_str().unwrap().contains("LEVEL6_OK"));

    let shallow_export = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-shallow6"])
        .output()
        .await
        .unwrap();
    assert!(shallow_export.status.success());
    assert!(
        String::from_utf8_lossy(&shallow_export.stderr)
            .trim()
            .is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&shallow_export.stderr)
    );
    assert!(String::from_utf8_lossy(&shallow_export.stdout).contains("LEVEL6_OK"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_cold_cache_is_an_error_and_fetched_empty_is_not() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Sync only
    let sync_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(sync_out.status.success());

    // Unfetched body -> exit 1 (pages get and pages export)
    let cold_get = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(cold_get.status.code(), Some(1));
    let cold_err: Value = serde_json::from_slice(&cold_get.stdout).unwrap();
    assert_eq!(cold_err["ok"], false);
    assert_eq!(cold_err["error"]["code"], "not_cached");
    assert!(String::from_utf8_lossy(&cold_get.stderr).contains(
        "Page 'page-roadmap' body not found in offline cache (not fetched); read it online once"
    ));

    let cold_export = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-roadmap", "--offline"])
        .output()
        .await
        .unwrap();
    assert_eq!(cold_export.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cold_export.stderr).contains(
        "Page 'page-roadmap' body not found in offline cache (not fetched); read it online once"
    ));

    let cold_cmts = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-roadmap", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(cold_cmts.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&cold_cmts.stderr)
        .contains("Comments of 'page-roadmap' not found in offline cache (not fetched)"));

    // 2. Compact read of genuinely empty page caches body only
    let empty_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_compact.status.success());

    let empty_off_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_off_compact.status.success());
    let empty_val: Value = serde_json::from_slice(&empty_off_compact.stdout).unwrap();
    assert_eq!(empty_val["markdown"], "");
    assert_eq!(empty_val["omitted"]["blocks"], 0);
    assert_eq!(empty_val["comments_state"], "not_fetched");
    assert!(empty_val["omitted"]["comments"].is_null());

    // Comments still unfetched -> exit 1 on comments list, and offline full reports comments_error
    let empty_cmts_before_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(empty_cmts_before_full.status.code(), Some(1));

    let empty_off_full_before = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-empty",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(empty_off_full_before.status.success());
    let empty_off_full_v: Value = serde_json::from_slice(&empty_off_full_before.stdout).unwrap();
    assert_eq!(
        empty_off_full_v["comments_error"],
        "Comments not fetched into the offline cache"
    );

    // Full read fetches verified empty comments (0)
    let empty_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(empty_full.status.success());

    let empty_off_after_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_off_after_full.status.success());
    let v_after: Value = serde_json::from_slice(&empty_off_after_full.stdout).unwrap();
    assert_eq!(v_after["comments_state"], "cached");
    assert_eq!(v_after["omitted"]["blocks"], 0);
    assert_eq!(v_after["omitted"]["comments"], 0);

    let empty_cmts_after_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-empty", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(empty_cmts_after_full.status.success());
    let c_after: Value = serde_json::from_slice(&empty_cmts_after_full.stdout).unwrap();
    assert_eq!(c_after["count"], 0);
    assert_eq!(c_after["returned"], 0);
    assert_eq!(c_after["has_more"], false);

    // 3. 403 comments error persists offline
    let err_online = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-cmt-err", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(err_online.status.code(), Some(1));

    let err_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(err_full.status.success());

    let err_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-cmt-err", "--json"])
        .output()
        .await
        .unwrap();
    assert!(err_compact.status.success());
    let ec: Value = serde_json::from_slice(&err_compact.stdout).unwrap();
    assert_eq!(ec["comments_state"], "failed");
    assert!(ec["comments_error"].as_str().unwrap().contains("403"));
    assert!(ec["omitted"]["comments"].is_null());

    let err_off = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-cmt-err", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(err_off.status.code(), Some(1));
    let err_off_stderr = String::from_utf8_lossy(&err_off.stderr);
    assert!(
        err_off_stderr
            .contains("Comments of 'page-cmt-err' not found in offline cache (last fetch failed:")
            && err_off_stderr.contains("403"),
        "unexpected stderr: {err_off_stderr}"
    );

    // HTTP 404 check on cold offline page body & comments
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    notion_cli::sync::sync_workspace_tree(&client, &storage)
        .await
        .unwrap();
    let api_state = Arc::new(serve::ApiState {
        config: cfg,
        auth,
        client,
        storage,
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });
    let http = reqwest::Client::new();
    let r_page = http
        .get(format!(
            "http://{api_addr}/api/v1/pages/page-roadmap?offline=true"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_page.status(), StatusCode::NOT_FOUND);
    let r_cmts = http
        .get(format!(
            "http://{api_addr}/api/v1/pages/page-roadmap/comments?offline=true"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_cmts.status(), StatusCode::NOT_FOUND);
    api_server.abort();

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_db_query_rejects_filter_and_sorts() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // Seed cache online
    let seed = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "query", "db-tasks", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let seed_val: Value = serde_json::from_slice(&seed.stdout).unwrap();
    assert_eq!(seed_val["count"], 3);

    // Online filter control
    let online_f = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "query",
            "db-tasks",
            "--filter",
            r#"{"property":"Status","status":{"equals":"Done"}}"#,
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(online_f.status.success());
    let online_f_val: Value = serde_json::from_slice(&online_f.stdout).unwrap();
    assert_eq!(online_f_val["count"], 1);

    // Offline + filter -> exit 1
    let off_f = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "query",
            "db-tasks",
            "--offline",
            "--filter",
            r#"{"property":"Status","status":{"equals":"Done"}}"#,
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(off_f.status.code(), Some(1));
    let off_err: Value = serde_json::from_slice(&off_f.stdout).unwrap();
    assert_eq!(off_err["ok"], false);
    assert_eq!(off_err["error"]["code"], "invalid_input");
    assert!(String::from_utf8_lossy(&off_f.stderr).contains("Invalid offline query"));

    // Offline + sorts -> exit 1
    let off_s = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "query",
            "db-tasks",
            "--offline",
            "--sorts",
            r#"[{"property":"Status","direction":"ascending"}]"#,
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(off_s.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&off_s.stderr).contains("Invalid offline query"));

    // Offline without filter/sorts -> exit 0, 3 rows
    let off_ok = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "query", "db-tasks", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(off_ok.status.success());
    let off_ok_val: Value = serde_json::from_slice(&off_ok.stdout).unwrap();
    assert_eq!(off_ok_val["count"], 3);
    assert_eq!(off_ok_val["returned"], 3);

    // MCP offline + filter -> isError: true
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    let mcp_err = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "query_database",
                "arguments": {
                    "database": "db-tasks",
                    "offline": true,
                    "filter": { "property": "Status", "status": { "equals": "Done" } }
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_err["result"]["isError"], true);
    assert!(mcp_err["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Invalid offline query"));

    // HTTP POST /api/v1/databases/db-tasks/query with offline:true + filter -> 400
    let api_state = Arc::new(serve::ApiState {
        config: cfg,
        auth,
        client,
        storage,
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(api_state)).await;
    });
    let http = reqwest::Client::new();
    let r_bad = http
        .post(format!("http://{api_addr}/api/v1/databases/db-tasks/query"))
        .json(&json!({
            "offline": true,
            "filter": { "property": "Status", "status": { "equals": "Done" } }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_bad.status(), StatusCode::BAD_REQUEST);
    api_server.abort();

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_query_pages_with_cursor_without_dropping_rows() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let p1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "query", "db-tasks", "--limit", "2", "--json"])
        .output()
        .await
        .unwrap();
    assert!(p1.status.success());
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(v1["count"], 2);
    assert_eq!(v1["returned"], 2);
    assert_eq!(v1["has_more"], true);
    let cur = v1["next_cursor"].as_str().unwrap().to_string();

    let p2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db", "query", "db-tasks", "--limit", "2", "--cursor", &cur, "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(p2.status.success());
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["count"], 1);
    assert_eq!(v2["returned"], 1);
    assert_eq!(v2["has_more"], false);
    assert!(v2["next_cursor"].is_null());

    let mut ids = HashSet::new();
    for row in v1["rows"]
        .as_array()
        .unwrap()
        .iter()
        .chain(v2["rows"].as_array().unwrap())
    {
        ids.insert(row["id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids.len(), 3);

    // Cache offset pagination & invalid cursor / limit 0 negative controls
    let db1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "list", "--json", "--limit", "1"])
        .output()
        .await
        .unwrap();
    assert!(db1.status.success());
    let db1_v: Value = serde_json::from_slice(&db1.stdout).unwrap();
    assert_eq!(db1_v["has_more"], true);
    assert_eq!(db1_v["next_cursor"], "1");

    let db_bad_cur = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["db", "list", "--json", "--cursor", "abc"])
        .output()
        .await
        .unwrap();
    assert_eq!(db_bad_cur.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&db_bad_cur.stderr)
        .contains("Invalid cursor 'abc': expected the integer offset returned as next_cursor"));

    let bad_lim = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["comments", "list", "page-roadmap", "--json", "--limit", "0"])
        .output()
        .await
        .unwrap();
    assert_eq!(bad_lim.status.code(), Some(1));
    let bad_lim_json: Value = serde_json::from_slice(&bad_lim.stdout).unwrap();
    assert_eq!(bad_lim_json["ok"], false);
    assert_eq!(bad_lim_json["error"]["code"], "invalid_input");
    assert!(
        String::from_utf8_lossy(&bad_lim.stderr).contains("Invalid limit 0: must be at least 1")
    );

    // W3: Remote 404 returns object_not_found JSON envelope on --json, and empty stdout without --json
    let missing_uuid = "99999999-9999-9999-9999-999999999999";
    let not_found_json = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", missing_uuid, "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(not_found_json.status.code(), Some(1));
    let nf_val: Value = serde_json::from_slice(&not_found_json.stdout).unwrap();
    assert_eq!(nf_val["ok"], false);
    assert_eq!(nf_val["error"]["code"], "object_not_found");

    let not_found_text = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", missing_uuid])
        .output()
        .await
        .unwrap();
    assert_eq!(not_found_text.status.code(), Some(1));
    assert!(
        not_found_text.stdout.is_empty(),
        "text-mode failure must keep stdout empty"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn comments_past_2000_are_listed_fully_and_detail_flags_more_without_refetch() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Cursor chain reads all 2,100 comments in 21 requests
    let mut cursor: Option<String> = None;
    let mut ids = HashSet::new();
    let mut rounds = 0usize;
    loop {
        rounds += 1;
        let mut args = vec![
            "comments".to_string(),
            "list".to_string(),
            "page-manycmt".to_string(),
            "--json".to_string(),
            "--limit".to_string(),
            "100".to_string(),
        ];
        if let Some(ref c) = cursor {
            args.push("--cursor".to_string());
            args.push(c.clone());
        }
        let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
            .args(&args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        for c in v["comments"].as_array().unwrap() {
            ids.insert(c["id"].as_str().unwrap().to_string());
        }
        if !v["has_more"].as_bool().unwrap() {
            assert!(v["next_cursor"].is_null());
            break;
        }
        cursor = v["next_cursor"].as_str().map(str::to_string);
    }
    assert_eq!(rounds, 21);
    assert_eq!(ids.len(), 2100);

    // 2. pages get --json --full fetches 2,001 comments in 21 requests (20x100 + 1x1),
    //    stores all 2,001 in cache, and serializes exactly the first 2,000 with comments_has_more:true
    let before_full = mock.comment_reqs.load(Ordering::SeqCst);
    let full_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-manycmt", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(full_out.status.success());
    let full_val: Value = serde_json::from_slice(&full_out.stdout).unwrap();
    let full_cmts = full_val["comments"].as_array().unwrap();
    assert_eq!(full_cmts.len(), 2000);
    assert_eq!(full_cmts.first().unwrap()["id"], "mc-0");
    assert_eq!(full_cmts.last().unwrap()["id"], "mc-1999");
    assert_eq!(full_val["comments_has_more"], true);
    assert!(full_val.get("comments_error").is_none());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst) - before_full, 21);

    // Repeating --json --full before the page changes reuses cached comments (0 comment GETs)
    let before_full_repeat = mock.comment_reqs.load(Ordering::SeqCst);
    let full_repeat = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-manycmt", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(full_repeat.status.success());
    let full_repeat_val: Value = serde_json::from_slice(&full_repeat.stdout).unwrap();
    assert_eq!(full_repeat_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(full_repeat_val["comments_has_more"], true);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_full_repeat);

    // Offline --json --full on warm partial cache also serializes 2,000 + comments_has_more:true
    let full_off = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-manycmt",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(full_off.status.success());
    let full_off_val: Value = serde_json::from_slice(&full_off.stdout).unwrap();
    assert_eq!(full_off_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(full_off_val["comments_has_more"], true);

    // 3. Compact read issues 0 comment requests, reports partial + null omitted.comments
    let before_compact = mock.comment_reqs.load(Ordering::SeqCst);
    let compact_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-manycmt", "--json"])
        .output()
        .await
        .unwrap();
    assert!(compact_out.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_compact);
    let compact_val: Value = serde_json::from_slice(&compact_out.stdout).unwrap();
    assert_eq!(compact_val["comments_state"], "partial");
    assert!(compact_val["omitted"]["comments"].is_null());

    // 4. Offline comments list at cursor 2000 proves the 2,001st lookahead comment is preserved in cache
    let off_tail = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-manycmt",
            "--offline",
            "--json",
            "--limit",
            "100",
            "--cursor",
            "2000",
        ])
        .output()
        .await
        .unwrap();
    assert!(off_tail.status.success());
    let tail_val: Value = serde_json::from_slice(&off_tail.stdout).unwrap();
    assert_eq!(tail_val["returned"], 1);
    assert_eq!(tail_val["count"], 1);
    assert_eq!(tail_val["comments"][0]["id"], "mc-2000");
    assert_eq!(tail_val["has_more"], true);
    assert!(tail_val["next_cursor"].is_null());
    assert!(tail_val["hint"]
        .as_str()
        .unwrap()
        .contains("Only part of the comments is cached"));

    // 5a. Boundary: exactly 2,000 comments (cold + warm + offline + compact)
    let before_2k = mock.comment_reqs.load(Ordering::SeqCst);
    let b2k = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2000", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2k.status.success());
    let b2k_val: Value = serde_json::from_slice(&b2k.stdout).unwrap();
    assert_eq!(b2k_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(b2k_val["comments_has_more"], false);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst) - before_2k, 20);

    let before_2k_warm = mock.comment_reqs.load(Ordering::SeqCst);
    let b2k_warm = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2000", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2k_warm.status.success());
    let b2k_warm_val: Value = serde_json::from_slice(&b2k_warm.stdout).unwrap();
    assert_eq!(b2k_warm_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(b2k_warm_val["comments_has_more"], false);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_2k_warm);

    let b2k_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2000", "--json"])
        .output()
        .await
        .unwrap();
    assert!(b2k_compact.status.success());
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_2k_warm);
    let b2k_cval: Value = serde_json::from_slice(&b2k_compact.stdout).unwrap();
    assert_eq!(b2k_cval["comments_state"], "cached");
    assert_eq!(b2k_cval["omitted"]["comments"], 2000);

    // 5b. Boundary: exactly 2,001 comments (CLI cold, warm, offline, compact, cached lookahead, online paging)
    let before_2001 = mock.comment_reqs.load(Ordering::SeqCst);
    let b2001 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2001", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2001.status.success());
    let b2001_val: Value = serde_json::from_slice(&b2001.stdout).unwrap();
    let b2001_cmts = b2001_val["comments"].as_array().unwrap();
    assert_eq!(b2001_cmts.len(), 2000);
    assert_eq!(b2001_cmts.last().unwrap()["id"], "mc-1999");
    assert_eq!(b2001_val["comments_has_more"], true);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst) - before_2001, 21);

    let before_2001_warm = mock.comment_reqs.load(Ordering::SeqCst);
    let b2001_warm = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-2001", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(b2001_warm.status.success());
    let b2001_warm_val: Value = serde_json::from_slice(&b2001_warm.stdout).unwrap();
    assert_eq!(b2001_warm_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(b2001_warm_val["comments_has_more"], true);
    assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), before_2001_warm);

    let b2001_off_tail = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-2001",
            "--offline",
            "--json",
            "--limit",
            "100",
            "--cursor",
            "2000",
        ])
        .output()
        .await
        .unwrap();
    assert!(b2001_off_tail.status.success());
    let b2001_tail_val: Value = serde_json::from_slice(&b2001_off_tail.stdout).unwrap();
    assert_eq!(b2001_tail_val["returned"], 1);
    assert_eq!(b2001_tail_val["comments"][0]["id"], "mc-2000");
    assert_eq!(b2001_tail_val["has_more"], true);
    assert!(b2001_tail_val["next_cursor"].is_null());

    // 6. Full one-shot list_comments (--limit 100000) upgrades cache to complete (2,100),
    //    while offline full page detail still caps serialized comments at 2,000 + comments_has_more:true
    let all_online = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-manycmt",
            "--json",
            "--limit",
            "100000",
        ])
        .output()
        .await
        .unwrap();
    assert!(all_online.status.success());
    let all_offline = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "comments",
            "list",
            "page-manycmt",
            "--offline",
            "--json",
            "--limit",
            "100000",
        ])
        .output()
        .await
        .unwrap();
    assert!(all_offline.status.success());
    let all_off_val: Value = serde_json::from_slice(&all_offline.stdout).unwrap();
    assert_eq!(all_off_val["count"], 2100);
    assert_eq!(all_off_val["has_more"], false);
    assert!(all_off_val.get("hint").is_none());

    let full_after_complete = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "get",
            "page-manycmt",
            "--offline",
            "--json",
            "--full",
        ])
        .output()
        .await
        .unwrap();
    assert!(full_after_complete.status.success());
    let full_after_val: Value = serde_json::from_slice(&full_after_complete.stdout).unwrap();
    assert_eq!(full_after_val["comments"].as_array().unwrap().len(), 2000);
    assert_eq!(full_after_val["comments_has_more"], true);

    // 7. MCP cold + warm + offline coverage for 2,000, 2,001, and 2,100 comments
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    for (page_id, expected_reqs, expected_has_more) in [
        ("page-2000", 20usize, false),
        ("page-2001", 21usize, true),
        ("page-manycmt", 21usize, true),
    ] {
        let reqs_before = mock.comment_reqs.load(Ordering::SeqCst);
        let cold_mcp = mcp::handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 10,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id, "compact": false } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(cold_mcp["result"]["isError"], false, "{cold_mcp}");
        assert_eq!(
            mock.comment_reqs.load(Ordering::SeqCst) - reqs_before,
            expected_reqs
        );
        let sc = &cold_mcp["result"]["structuredContent"];
        let text = cold_mcp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(!text.contains('\n'));
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(&parsed, sc);
        let cmts = sc["comments"].as_array().unwrap();
        assert_eq!(cmts.len(), 2000);
        assert_eq!(cmts.last().unwrap()["id"], "mc-1999");
        assert_eq!(sc["comments_has_more"], expected_has_more);

        // Warm repeat issues 0 comment requests
        let reqs_warm = mock.comment_reqs.load(Ordering::SeqCst);
        let warm_mcp = mcp::handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 11,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id, "compact": false } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(warm_mcp["result"]["isError"], false, "{warm_mcp}");
        assert_eq!(mock.comment_reqs.load(Ordering::SeqCst), reqs_warm);
        assert_eq!(
            warm_mcp["result"]["structuredContent"]["comments"]
                .as_array()
                .unwrap()
                .len(),
            2000
        );
        assert_eq!(
            warm_mcp["result"]["structuredContent"]["comments_has_more"],
            expected_has_more
        );

        // Offline MCP get_page matches
        let off_mcp = mcp::handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 12,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id, "compact": false, "offline": true } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(off_mcp["result"]["isError"], false, "{off_mcp}");
        assert_eq!(
            off_mcp["result"]["structuredContent"]["comments"]
                .as_array()
                .unwrap()
                .len(),
            2000
        );
        assert_eq!(
            off_mcp["result"]["structuredContent"]["comments_has_more"],
            expected_has_more
        );

        // Partial caches retain the 2,001st lookahead entry for offline list_comments
        if expected_has_more {
            let tail_mcp = mcp::handle_request_value(
                &cfg,
                &auth,
                &client,
                &storage,
                json!({
                    "jsonrpc": "2.0",
                    "id": 13,
                    "method": "tools/call",
                    "params": {
                        "name": "list_comments",
                        "arguments": { "page": page_id, "offline": true, "limit": 100, "cursor": "2000" }
                    }
                }),
            )
            .await
            .unwrap();
            assert_eq!(tail_mcp["result"]["isError"], false, "{tail_mcp}");
            let tsc = &tail_mcp["result"]["structuredContent"];
            assert_eq!(tsc["returned"], 1);
            assert_eq!(tsc["comments"][0]["id"], "mc-2000");
            assert_eq!(tsc["has_more"], true);
            assert!(tsc["next_cursor"].is_null());
        }
    }

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocks_list_json_defaults_to_50_and_text_prints_all() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Default --json returns 50, then 50, then 20
    let p1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json"])
        .output()
        .await
        .unwrap();
    assert!(p1.status.success());
    let v1: Value = serde_json::from_slice(&p1.stdout).unwrap();
    assert_eq!(v1["count"], 50);
    assert_eq!(v1["returned"], 50);
    assert_eq!(v1["has_more"], true);
    let c1 = v1["next_cursor"].as_str().unwrap().to_string();

    // Partial read did NOT write cache -> offline fails
    let off_partial = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(off_partial.status.code(), Some(1));

    let p2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json", "--cursor", &c1])
        .output()
        .await
        .unwrap();
    assert!(p2.status.success());
    let v2: Value = serde_json::from_slice(&p2.stdout).unwrap();
    assert_eq!(v2["count"], 50);
    assert_eq!(v2["has_more"], true);
    let c2 = v2["next_cursor"].as_str().unwrap().to_string();

    let p3 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json", "--cursor", &c2])
        .output()
        .await
        .unwrap();
    assert!(p3.status.success());
    let v3: Value = serde_json::from_slice(&p3.stdout).unwrap();
    assert_eq!(v3["count"], 20);
    assert_eq!(v3["has_more"], false);
    assert!(v3["next_cursor"].is_null());

    let mut ids = HashSet::new();
    for v in [&v1, &v2, &v3] {
        for b in v["blocks"].as_array().unwrap() {
            ids.insert(b["id"].as_str().unwrap().to_string());
        }
    }
    assert_eq!(ids.len(), 120);

    // Full read (--limit 1000) writes cache -> offline succeeds
    let full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--json", "--limit", "1000"])
        .output()
        .await
        .unwrap();
    assert!(full.status.success());
    let off_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide", "--offline", "--json"])
        .output()
        .await
        .unwrap();
    assert!(off_full.status.success());
    let off_val: Value = serde_json::from_slice(&off_full.stdout).unwrap();
    assert_eq!(off_val["count"], 50);
    assert_eq!(off_val["has_more"], true);

    // Text mode prints all 120 blocks
    let text_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "list", "page-wide"])
        .output()
        .await
        .unwrap();
    assert!(text_out.status.success());
    let stdout = String::from_utf8(text_out.stdout).unwrap();
    assert!(stdout.contains("Wide block 0"));
    assert!(stdout.contains("Wide block 119"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_streams_past_500_and_upgrades_unmarked_caches_once() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        // 628 pages + 1 db = 629 objects sorted newest-first
        for i in (0..628).rev() {
            let mins = i / 60;
            let secs = i % 60;
            items.push(page_obj(
                &format!("p-{i:04}"),
                &format!("Page {i}"),
                &format!("2026-11-01T00:{mins:02}:{secs:02}.000Z"),
            ));
        }
        items.push(db_obj("db-1", "DB 1", "2026-10-31T23:59:59.000Z"));
    }
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1st sync: streams all 629 objects across 7 requests
    let s1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        s1.status.success(),
        "{}",
        String::from_utf8_lossy(&s1.stderr)
    );
    let v1: Value = serde_json::from_slice(&s1.stdout).unwrap();
    assert_eq!(v1["fetched_count"], 629);
    assert_eq!(v1["pages_count"], 628);
    assert_eq!(v1["databases_count"], 1);
    assert_eq!(v1["complete"], true);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 7);

    // 2nd sync: stops at watermark after 1 request
    let before_s2 = mock.search_reqs.load(Ordering::SeqCst);
    let s2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(s2.status.success());
    let v2: Value = serde_json::from_slice(&s2.stdout).unwrap();
    assert_eq!(v2["complete"], true);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst) - before_s2, 1);

    // Separate profile simulating 0.2.2 cache: has watermark but no workspace-complete marker
    let ws_id = v1["workspace_id"].as_str().unwrap().to_string();
    let tmp2 = tempfile::tempdir().unwrap();
    let cfg_dir2 = tmp2.path().join("cfg");
    let data_dir2 = tmp2.path().join("data");
    let db_path = data_dir2.join(&ws_id).join("cache.db");
    {
        let st = Storage::open(&db_path).unwrap();
        st.set_sync_cursor(
            &format!("workspace:{ws_id}"),
            Some("2026-11-01T00:10:27.000Z"),
        )
        .unwrap();
    }
    let before_upg = mock.search_reqs.load(Ordering::SeqCst);
    let upg = notion_cli_cmd(&base_url, &cfg_dir2, &data_dir2)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        upg.status.success(),
        "{}",
        String::from_utf8_lossy(&upg.stderr)
    );
    let upg_v: Value = serde_json::from_slice(&upg.stdout).unwrap();
    assert_eq!(upg_v["fetched_count"], 629);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst) - before_upg, 7);

    let st2 = Storage::open(&db_path).unwrap();
    assert_eq!(
        st2.get_sync_cursor(&format!("workspace-complete:{ws_id}"))
            .as_deref(),
        Some("1")
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_failure_keeps_batches_and_does_not_advance_watermark() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        for i in (0..250).rev() {
            let mins = i / 60;
            let secs = i % 60;
            items.push(page_obj(
                &format!("p-{i:04}"),
                &format!("Page {i}"),
                &format!("2026-11-01T00:{mins:02}:{secs:02}.000Z"),
            ));
        }
    }
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");
    let db_path = data_dir.join("default").join("cache.db");

    // Scenario 1: Initial sync fails on 2nd search request
    mock.fail_search_on_req.store(2, Ordering::SeqCst);
    let fail1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(fail1.status.code(), Some(1));
    let fail1_err: Value = serde_json::from_slice(&fail1.stdout).unwrap();
    assert_eq!(fail1_err["ok"], false);
    assert_eq!(fail1_err["error"]["code"], "validation_error");
    assert!(
        String::from_utf8_lossy(&fail1.stderr).contains("Workspace sync stopped after 100 objects"),
        "stderr: {}",
        String::from_utf8_lossy(&fail1.stderr)
    );

    // Already-received 100 items stay cached
    let off1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "list", "--offline", "--json", "--limit", "1000"])
        .output()
        .await
        .unwrap();
    assert!(off1.status.success());
    let off1_v: Value = serde_json::from_slice(&off1.stdout).unwrap();
    assert_eq!(off1_v["count"], 100);

    // Watermark and complete marker untouched (both absent)
    {
        let st = Storage::open(&db_path).unwrap();
        assert_eq!(st.get_sync_cursor("workspace:default"), None);
        assert_eq!(st.get_sync_cursor("workspace-complete:default"), None);
    }

    // Retry succeeds and caches all 250
    let ok1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        ok1.status.success(),
        "{}",
        String::from_utf8_lossy(&ok1.stderr)
    );
    let ok1_v: Value = serde_json::from_slice(&ok1.stdout).unwrap();
    assert_eq!(ok1_v["pages_count"], 250);
    assert_eq!(ok1_v["complete"], true);

    let ws_id = ok1_v["workspace_id"].as_str().unwrap().to_string();
    let ws_db_path = data_dir.join(&ws_id).join("cache.db");
    let old_wm = Storage::open(&ws_db_path)
        .unwrap()
        .get_sync_cursor(&format!("workspace:{ws_id}"))
        .unwrap();

    // Scenario 2: Add 150 newer items; fail 2nd request of incremental sync
    {
        let mut items = mock.search_items.lock().unwrap();
        let mut newer = Vec::new();
        for i in (0..150).rev() {
            let mins = i / 60;
            let secs = i % 60;
            newer.push(page_obj(
                &format!("p-new-{i:04}"),
                &format!("New Page {i}"),
                &format!("2026-11-02T00:{mins:02}:{secs:02}.000Z"),
            ));
        }
        newer.extend(items.drain(..));
        *items = newer;
    }
    let next_req = mock.search_reqs.load(Ordering::SeqCst) + 2;
    mock.fail_search_on_req.store(next_req, Ordering::SeqCst);

    let fail2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(fail2.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&fail2.stderr).contains("Workspace sync stopped after 100 objects")
    );

    // Watermark must still be old_wm
    assert_eq!(
        Storage::open(&ws_db_path)
            .unwrap()
            .get_sync_cursor(&format!("workspace:{ws_id}"))
            .as_deref(),
        Some(old_wm.as_str())
    );

    // Retry incremental sync completes all 400 pages and advances watermark
    let ok2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        ok2.status.success(),
        "{}",
        String::from_utf8_lossy(&ok2.stderr)
    );
    let ok2_v: Value = serde_json::from_slice(&ok2.stdout).unwrap();
    assert_eq!(ok2_v["pages_count"], 400);
    assert_eq!(ok2_v["complete"], true);
    let new_wm = Storage::open(&ws_db_path)
        .unwrap()
        .get_sync_cursor(&format!("workspace:{ws_id}"))
        .unwrap();
    assert!(
        new_wm > old_wm,
        "expected watermark to advance: {old_wm} -> {new_wm}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_workspace_search_rejects_missing_or_repeated_cursor() {
    let mock = ReadContractMock::default();
    mock.search_items
        .lock()
        .unwrap()
        .push(page_obj("p-1", "One", "2026-11-01T00:00:00.000Z"));
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    *mock.search_cursor_fault.lock().unwrap() = CursorFault::MissingNextCursor;
    let out_missing = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(out_missing.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out_missing.stderr).contains("missing its next cursor"),
        "stderr: {}",
        String::from_utf8_lossy(&out_missing.stderr)
    );

    *mock.search_cursor_fault.lock().unwrap() = CursorFault::RepeatedCursor;
    let out_repeat = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["sync", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(out_repeat.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out_repeat.stderr).contains("repeated its pagination cursor"),
        "stderr: {}",
        String::from_utf8_lossy(&out_repeat.stderr)
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_fallback_keeps_database_hits() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        // Neither title nor description contains the query token "yellow", so local FTS refresh
        // returns 0 hits and falls back to SearchResponse hits.
        items.push(page_obj("p-sun", "Sun Notes", "2026-11-01T00:00:00.000Z"));
        items.push(db_obj(
            "db-lemon",
            "Lemon Tracker",
            "2026-11-01T00:00:00.000Z",
        ));
    }
    let (base_url, server) = start_mock_server(mock).await;
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();

    let fallback = notion_cli_cmd(
        &base_url,
        &tmp1.path().join("cfg"),
        &tmp1.path().join("data"),
    )
    .args(["search", "yellow", "--json"])
    .output()
    .await
    .unwrap();
    assert!(fallback.status.success());
    let fb_v: Value = serde_json::from_slice(&fallback.stdout).unwrap();

    let remote = notion_cli_cmd(
        &base_url,
        &tmp2.path().join("cfg"),
        &tmp2.path().join("data"),
    )
    .args(["search", "yellow", "--remote", "--json"])
    .output()
    .await
    .unwrap();
    assert!(remote.status.success());
    let rm_v: Value = serde_json::from_slice(&remote.stdout).unwrap();

    let extract = |v: &Value| -> BTreeSet<(String, String)> {
        v["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["item_type"].as_str().unwrap().to_string(),
                    h["id"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let fb_set = extract(&fb_v);
    let rm_set = extract(&rm_v);
    assert_eq!(fb_set, rm_set);
    assert!(fb_set.contains(&("database".into(), "db-lemon".into())));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_sync_now_reports_fetched_count_and_complete() {
    let mock = ReadContractMock::default();
    {
        let mut items = mock.search_items.lock().unwrap();
        for i in (0..250).rev() {
            items.push(page_obj(
                &format!("p-{i:04}"),
                &format!("Page {i}"),
                "2026-11-01T00:00:00.000Z",
            ));
        }
    }
    let (base_url, server) = start_mock_server(mock).await;
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);

    let resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "sync_now", "arguments": {} }
        }),
    )
    .await
    .unwrap();
    assert_eq!(resp["result"]["isError"], false, "{resp}");
    let sc = &resp["result"]["structuredContent"];
    assert_eq!(sc["fetched_count"], 250);
    assert_eq!(sc["complete"], true);
    assert!(sc.get("has_more").is_none());

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_get_page_defaults_compact_and_text_is_minified() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let (cfg, auth, client, storage) = mcp_runtime(&base_url);

    // Compact default
    let compact_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_page", "arguments": { "page": "page-roadmap" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(compact_resp["result"]["isError"], false, "{compact_resp}");
    let compact_text = compact_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(!compact_text.contains('\n'), "MCP text must be minified");
    let parsed_compact: Value = serde_json::from_str(compact_text).unwrap();
    assert_eq!(parsed_compact, compact_resp["result"]["structuredContent"]);
    assert_eq!(parsed_compact["comments_state"], "not_fetched");
    assert_eq!(
        parsed_compact["hint"],
        "Call get_page with {\"page\":\"page-roadmap\",\"compact\":false} for properties, blocks, comments and media."
    );

    // Full (compact: false)
    let full_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_page", "arguments": { "page": "page-roadmap", "compact": false } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(full_resp["result"]["isError"], false, "{full_resp}");
    let full_text = full_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(!full_text.contains('\n'), "MCP text must be minified");
    let parsed_full: Value = serde_json::from_str(full_text).unwrap();
    assert_eq!(parsed_full, full_resp["result"]["structuredContent"]);
    assert!(parsed_full.get("page").is_some());
    assert_eq!(parsed_full["comments_has_more"], false);

    let mcp_compact_wire = serde_json::to_string(&compact_resp).unwrap().len();
    let mcp_full_wire = serde_json::to_string(&full_resp).unwrap().len();
    eprintln!(
        "MEASURE_MCP get_page_compact_wire_bytes={} get_page_full_wire_bytes={}",
        mcp_compact_wire, mcp_full_wire
    );

    // Paginated get_page_blocks
    let blocks_resp = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "get_page_blocks", "arguments": { "page": "page-roadmap", "limit": 1 } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(blocks_resp["result"]["isError"], false, "{blocks_resp}");
    let bsc = &blocks_resp["result"]["structuredContent"];
    assert_eq!(bsc["returned"], 1);
    assert_eq!(bsc["count"], 1);
    assert_eq!(bsc["has_more"], true);
    assert_eq!(bsc["next_cursor"].is_string(), true);
    assert_eq!(bsc["incomplete_blocks"], json!([]));

    // W6: MCP append_blocks defaults to compact {ok, appended, block_ids}; compact:false returns blocks
    let append_compact = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "append_blocks", "arguments": { "page": "page-roadmap", "markdown": "- Item A\n- Item B" } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(append_compact["result"]["isError"], false, "{append_compact}");
    let ac = &append_compact["result"]["structuredContent"];
    assert_eq!(ac["ok"], true);
    assert_eq!(ac["appended"], 2);
    assert!(ac.get("blocks").is_none());
    assert_eq!(ac["block_ids"].as_array().unwrap().len(), 2);

    let append_full = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": { "name": "append_blocks", "arguments": { "page": "page-roadmap", "markdown": "- Item C", "compact": false } }
        }),
    )
    .await
    .unwrap();
    assert_eq!(append_full["result"]["isError"], false, "{append_full}");
    let af = &append_full["result"]["structuredContent"];
    assert_eq!(af["ok"], true);
    assert_eq!(af["appended"], 1);
    assert_eq!(af["blocks"].as_array().unwrap().len(), 1);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn title_writes_need_one_exact_active_match() {
    let ws1 = "22222222-2222-2222-2222-222222222221";
    let ws2 = "22222222-2222-2222-2222-222222222222";
    let launch_id = "11111111-1111-1111-1111-111111111111";
    let sprint_retro_id = "44444444-4444-4444-4444-444444444441";
    let sprint_exact_id = "44444444-4444-4444-4444-444444444442";
    let row_id = "55555555-5555-5555-5555-555555555551";
    let db_id = "dddddddd-dddd-dddd-dddd-ddddddddddd1";

    let mock = ReadContractMock::default();
    mock.filter_search.store(true, Ordering::SeqCst);
    {
        let p_ws1 = page_obj(ws1, "Weekly Sync", "2026-11-22T12:00:00.000Z");
        let p_ws2 = page_obj(ws2, "Weekly Sync", "2026-11-22T12:05:00.000Z");
        let p_launch = page_obj(launch_id, "Q4 Launch Plan", "2026-11-20T00:00:00.000Z");
        let p_sprint_retro =
            page_obj(sprint_retro_id, "Sprint Retrospective", "2026-11-20T00:00:00.000Z");
        let p_row = db_row_obj(row_id, db_id, "Ship CLI Audit", "Todo", "2026-11-20T00:00:00.000Z");
        let mut pages = mock.pages.lock().unwrap();
        for p in [&p_ws1, &p_ws2, &p_launch, &p_sprint_retro, &p_row] {
            pages.insert(p["id"].as_str().unwrap().into(), p.clone());
        }
        let mut items = mock.search_items.lock().unwrap();
        items.push(p_ws2);
        items.push(p_ws1);
        items.push(p_launch);
        items.push(p_sprint_retro);
        items.push(p_row);
    }
    {
        let mut blocks = mock.blocks.lock().unwrap();
        for id in [ws1, ws2, launch_id, sprint_retro_id, sprint_exact_id] {
            blocks.insert(id.into(), vec![paragraph_block(&format!("blk-{id}"), "Body")]);
        }
    }

    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Duplicate exact titles (same search page) -> ambiguous_page, 2 candidates, 0 PATCHes
    let dup = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Weekly Sync", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(dup.status.code(), Some(1));
    let dup_v: Value = serde_json::from_slice(&dup.stdout).unwrap();
    assert_eq!(dup_v["ok"], false);
    assert_eq!(dup_v["error"]["code"], "ambiguous_page");
    assert_eq!(
        dup_v["error"]["details"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    // 2. 2-page duplicate control: page 1 has 1 exact match, page 2 has 1 exact match
    mock.search_page_limit.store(1, Ordering::SeqCst);
    mock.search_reqs.store(0, Ordering::SeqCst);
    let dup_2p = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Weekly Sync", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(dup_2p.status.code(), Some(1));
    let dup_2p_v: Value = serde_json::from_slice(&dup_2p.stdout).unwrap();
    assert_eq!(dup_2p_v["error"]["code"], "ambiguous_page");
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 2);
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    // Same 2-page mock, rename page 2 so only page 1 matches -> exhausts search & patches page 1
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] =
                    json!("Weekly Sync Archive");
            }
        }
        mock.pages.lock().unwrap().get_mut(ws1).unwrap()["properties"]["title"]["title"][0]
            ["plain_text"] = json!("Weekly Sync Archive");
    }
    mock.search_reqs.store(0, Ordering::SeqCst);
    let unique_2p = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Weekly Sync", "unique-2p", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        unique_2p.status.success(),
        "{}",
        String::from_utf8_lossy(&unique_2p.stderr)
    );
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 2);
    let patches = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, ws2);

    // Restore ws1 title to "Weekly Sync" and remove page size cap for next checks
    mock.search_page_limit.store(0, Ordering::SeqCst);
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] = json!("Weekly Sync");
            }
        }
        mock.pages.lock().unwrap().get_mut(ws1).unwrap()["properties"]["title"]["title"][0]
            ["plain_text"] = json!("Weekly Sync");
    }

    // 3. Partial cache: fresh data dir, read only ws1 via UUID, then write by title -> still ambiguous_page
    let tmp_partial = tempfile::tempdir().unwrap();
    let p_cfg = tmp_partial.path().join("cfg");
    let p_data = tmp_partial.path().join("data");
    let get1 = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "get", ws1, "--json"])
        .output()
        .await
        .unwrap();
    assert!(get1.status.success());
    let dup_partial = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "append", "Weekly Sync", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(dup_partial.status.code(), Some(1));
    let dup_pv: Value = serde_json::from_slice(&dup_partial.stdout).unwrap();
    assert_eq!(dup_pv["error"]["code"], "ambiguous_page");
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    // 4. Stale cache row: ws1 is cached as "Weekly Sync", then renamed remotely to "Old Notes"
    // so search doesn't return ws1 -> GET /v1/pages/{ws1} verifies & drops it, then patches ws2
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] = json!("Old Notes");
            }
        }
        mock.pages.lock().unwrap().get_mut(ws1).unwrap()["properties"]["title"]["title"][0]
            ["plain_text"] = json!("Old Notes");
    }
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let stale_ok = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "append", "Weekly Sync", "after-rename", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        stale_ok.status.success(),
        "{}",
        String::from_utf8_lossy(&stale_ok.stderr)
    );
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 1);
    let patches = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, ws2);

    // 5. Archived + active with same title -> write hits active; --unarchive restores cached archived
    {
        let mut items = mock.search_items.lock().unwrap();
        for item in items.iter_mut() {
            if item["id"] == ws1 {
                item["properties"]["title"]["title"][0]["plain_text"] = json!("Weekly Sync");
                item["archived"] = json!(false);
            }
            if item["id"] == ws2 {
                item["archived"] = json!(true);
            }
        }
        let mut pages = mock.pages.lock().unwrap();
        pages.get_mut(ws1).unwrap()["properties"]["title"]["title"][0]["plain_text"] =
            json!("Weekly Sync");
        pages.get_mut(ws1).unwrap()["archived"] = json!(false);
        pages.get_mut(ws2).unwrap()["archived"] = json!(true);
    }
    let active_write = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "append", "Weekly Sync", "active-only", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        active_write.status.success(),
        "{}",
        String::from_utf8_lossy(&active_write.stderr)
    );
    let patches = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].0, ws1);

    // Remove ws2 from search results (simulating Notion Search omitting archived pages)
    // and ensure ws2 is cached as archived -> --unarchive verifies via GET and restores ws2
    {
        let mut items = mock.search_items.lock().unwrap();
        items.retain(|i| i["id"] != ws2);
    }
    let _ = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "get", ws2, "--json"])
        .output()
        .await
        .unwrap();
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let unarch = notion_cli_cmd(&base_url, &p_cfg, &p_data)
        .args(["pages", "archive", "Weekly Sync", "--unarchive", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        unarch.status.success(),
        "{}",
        String::from_utf8_lossy(&unarch.stderr)
    );
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 1);
    let page_patches = std::mem::take(&mut *mock.page_patch_reqs.lock().unwrap());
    assert_eq!(page_patches.len(), 1);
    assert_eq!(page_patches[0].0, ws2);
    assert_eq!(page_patches[0].1["archived"], false);

    // --unarchive when NOT in local cache -> page_not_found with ID/URL guidance
    let tmp_empty = tempfile::tempdir().unwrap();
    let unarch_miss = notion_cli_cmd(
        &base_url,
        &tmp_empty.path().join("cfg"),
        &tmp_empty.path().join("data"),
    )
    .args(["pages", "archive", "Weekly Sync", "--unarchive", "--json"])
    .output()
    .await
    .unwrap();
    assert_eq!(unarch_miss.status.code(), Some(1));
    let unarch_v: Value = serde_json::from_slice(&unarch_miss.stdout).unwrap();
    assert_eq!(unarch_v["error"]["code"], "page_not_found");
    assert!(unarch_v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("pass the page ID or URL"));

    // 6. Controls: UUID input -> 0 searches, 0 extra page GETs; unique title -> 1 search, 0 extra GETs
    mock.search_reqs.store(0, Ordering::SeqCst);
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let uuid_write = notion_cli_cmd(
        &base_url,
        &tmp_empty.path().join("cfg"),
        &tmp_empty.path().join("data"),
    )
    .args(["pages", "append", launch_id, "direct-uuid", "--json"])
    .output()
    .await
    .unwrap();
    assert!(uuid_write.status.success());
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 0);
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 0);
    let _ = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());

    let unique_write = notion_cli_cmd(
        &base_url,
        &tmp_empty.path().join("cfg"),
        &tmp_empty.path().join("data"),
    )
    .args(["pages", "append", "Q4 Launch Plan", "unique-title", "--json"])
    .output()
    .await
    .unwrap();
    assert!(unique_write.status.success());
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), 1);
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 0);
    let _ = std::mem::take(&mut *mock.block_patch_reqs.lock().unwrap());

    // 7. Substring write rejected with page_not_found + candidates, while substring read succeeds
    let sub_write = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "Launch", "x", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(sub_write.status.code(), Some(1));
    let sub_v: Value = serde_json::from_slice(&sub_write.stdout).unwrap();
    assert_eq!(sub_v["error"]["code"], "page_not_found");
    assert_eq!(
        sub_v["error"]["details"]["candidates"][0]["title"],
        "Q4 Launch Plan"
    );
    assert!(mock.block_patch_reqs.lock().unwrap().is_empty());

    let sub_read = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "Launch", "--json"])
        .output()
        .await
        .unwrap();
    assert!(sub_read.status.success());
    let sr_v: Value = serde_json::from_slice(&sub_read.stdout).unwrap();
    assert_eq!(sr_v["id"], launch_id);

    // Online missing title read fails locally with page_not_found and 0 GET /v1/pages/* calls
    mock.page_get_reqs.store(0, Ordering::SeqCst);
    let missing_read = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "No Such Page Title", "--json"])
        .output()
        .await
        .unwrap();
    assert_eq!(missing_read.status.code(), Some(1));
    let mr_v: Value = serde_json::from_slice(&missing_read.stdout).unwrap();
    assert_eq!(mr_v["error"]["code"], "page_not_found");
    assert_eq!(mock.page_get_reqs.load(Ordering::SeqCst), 0);

    // F4b resolver read: cached "Sprint Retrospective" must not shadow remote exact "Sprint"
    let _ = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", sprint_retro_id, "--json"])
        .output()
        .await
        .unwrap();
    {
        let p_sprint = page_obj(sprint_exact_id, "Sprint", "2026-11-22T10:00:00.000Z");
        mock.pages
            .lock()
            .unwrap()
            .insert(sprint_exact_id.into(), p_sprint.clone());
        mock.search_items.lock().unwrap().push(p_sprint);
    }
    let exact_read = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "Sprint", "--json"])
        .output()
        .await
        .unwrap();
    assert!(exact_read.status.success());
    let er_v: Value = serde_json::from_slice(&exact_read.stdout).unwrap();
    assert_eq!(er_v["id"], sprint_exact_id);

    // F5c: db update-item accepts Notion URL
    let url_update = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "db",
            "update-item",
            "https://www.notion.so/Ship-CLI-55555555555555555555555555555551",
            "--status",
            "Done",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        url_update.status.success(),
        "{}",
        String::from_utf8_lossy(&url_update.stderr)
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_cache_does_not_answer_lists() {
    let mock = standard_mock();
    let expected_pages = mock.pages.lock().unwrap().len();
    let expected_dbs = mock.databases.lock().unwrap().len();
    let (base_url, server) = start_mock_server(mock.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // 1. Single page get creates a partial cache (no workspace-complete marker)
    let get_one = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "get", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(get_one.status.success());

    // 2. pages list must sync full workspace rather than returning count: 1
    let before = mock.search_reqs.load(Ordering::SeqCst);
    let list1 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(list1.status.success());
    let v1: Value = serde_json::from_slice(&list1.stdout).unwrap();
    assert_eq!(v1["count"], expected_pages);
    assert!(mock.search_reqs.load(Ordering::SeqCst) > before);

    // 3. Second pages list hits complete cache with 0 additional search requests (control)
    let after_first = mock.search_reqs.load(Ordering::SeqCst);
    let list2 = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(list2.status.success());
    let v2: Value = serde_json::from_slice(&list2.stdout).unwrap();
    assert_eq!(v2["count"], expected_pages);
    assert_eq!(mock.search_reqs.load(Ordering::SeqCst), after_first);

    // 4. Separate fresh dir: single db get must not cause db list to return count: 1
    let tmp2 = tempfile::tempdir().unwrap();
    let db_get = notion_cli_cmd(
        &base_url,
        &tmp2.path().join("cfg"),
        &tmp2.path().join("data"),
    )
    .args(["db", "get", "db-tasks", "--json"])
    .output()
    .await
    .unwrap();
    assert!(db_get.status.success());
    let db_list = notion_cli_cmd(
        &base_url,
        &tmp2.path().join("cfg"),
        &tmp2.path().join("data"),
    )
    .args(["db", "list", "--json"])
    .output()
    .await
    .unwrap();
    assert!(db_list.status.success());
    let db_v: Value = serde_json::from_slice(&db_list.stdout).unwrap();
    assert_eq!(db_v["count"], expected_dbs);

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_append_and_export_compact_defaults_and_full_flag() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    // pages append --json defaults to compact {ok, appended, block_ids}
    let app_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "page-roadmap", "- One\n- Two", "--json"])
        .output()
        .await
        .unwrap();
    assert!(
        app_compact.status.success(),
        "{}",
        String::from_utf8_lossy(&app_compact.stderr)
    );
    let ac: Value = serde_json::from_slice(&app_compact.stdout).unwrap();
    assert_eq!(ac["ok"], true);
    assert_eq!(ac["appended"], 2);
    assert!(ac.get("blocks").is_none());
    assert_eq!(ac["block_ids"].as_array().unwrap().len(), 2);

    // pages append --json --full returns blocks
    let app_full = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "append", "page-roadmap", "- Three", "--json", "--full"])
        .output()
        .await
        .unwrap();
    assert!(app_full.status.success());
    let af: Value = serde_json::from_slice(&app_full.stdout).unwrap();
    assert_eq!(af["ok"], true);
    assert_eq!(af["appended"], 1);
    assert_eq!(af["blocks"].as_array().unwrap().len(), 1);

    // blocks append --json defaults to compact; --full restores blocks
    let blk_compact = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["blocks", "append", "page-roadmap", "- Four", "--json"])
        .output()
        .await
        .unwrap();
    assert!(blk_compact.status.success());
    let bc: Value = serde_json::from_slice(&blk_compact.stdout).unwrap();
    assert!(bc.get("blocks").is_none());
    assert_eq!(bc["block_ids"].as_array().unwrap().len(), 1);

    // pages export --out --json omits markdown and includes bytes
    let out_file = tmp.path().join("exported.md");
    let exp_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args([
            "pages",
            "export",
            "page-roadmap",
            "--out",
            out_file.to_str().unwrap(),
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(exp_out.status.success());
    let eo: Value = serde_json::from_slice(&exp_out.stdout).unwrap();
    assert_eq!(eo["ok"], true);
    assert!(eo.get("markdown").is_none());
    assert!(eo["bytes"].as_u64().unwrap() > 0);
    assert!(out_file.exists());

    // pages export --json (without --out) keeps markdown
    let exp_stdout = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["pages", "export", "page-roadmap", "--json"])
        .output()
        .await
        .unwrap();
    assert!(exp_stdout.status.success());
    let es: Value = serde_json::from_slice(&exp_stdout.stdout).unwrap();
    assert!(es["markdown"].as_str().unwrap().contains("Roadmap"));

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_and_mcp_list_users_return_workspace_users() {
    let mock = standard_mock();
    let (base_url, server) = start_mock_server(mock).await;
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("cfg");
    let data_dir = tmp.path().join("data");

    let out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["users", "list", "--json"])
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["ok"], true);
    assert_eq!(val["count"], 1);
    assert_eq!(val["users"][0]["id"], "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1");
    assert_eq!(val["users"][0]["name"], "Alice Kim");

    let text_out = notion_cli_cmd(&base_url, &cfg_dir, &data_dir)
        .args(["users", "list"])
        .output()
        .await
        .unwrap();
    assert!(text_out.status.success());
    let stdout = String::from_utf8_lossy(&text_out.stdout);
    assert!(stdout.contains("Alice Kim <alice@example.com> (aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1)"));

    let (cfg, auth, client, storage) = mcp_runtime(&base_url);
    let mcp_users = mcp::handle_request_value(
        &cfg,
        &auth,
        &client,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": { "name": "list_users", "arguments": {} }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_users["result"]["isError"], false, "{mcp_users}");
    assert_eq!(
        mcp_users["result"]["structuredContent"]["users"][0]["id"],
        "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaa1"
    );

    server.abort();
}
