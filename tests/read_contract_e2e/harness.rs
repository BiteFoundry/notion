use std::{
    collections::HashMap,
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
    notion::NotionClient,
    storage::Storage,
};
use serde_json::{json, Value};
use tokio::{net::TcpListener, process::Command};

pub(crate) fn notion_cli_cmd(base_url: &str, cfg_dir: &Path, data_dir: &Path) -> Command {
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
pub(crate) enum CursorFault {
    None,
    MissingNextCursor,
    RepeatedCursor,
}

#[derive(Clone)]
pub(crate) struct ReadContractMock {
    pub(crate) pages: Arc<Mutex<HashMap<String, Value>>>,
    pub(crate) databases: Arc<Mutex<HashMap<String, Value>>>,
    pub(crate) blocks: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    pub(crate) comment_counts: Arc<Mutex<HashMap<String, usize>>>,
    pub(crate) db_rows: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    pub(crate) search_items: Arc<Mutex<Vec<Value>>>,
    pub(crate) users: Arc<Mutex<Vec<Value>>>,
    pub(crate) search_reqs: Arc<AtomicUsize>,
    pub(crate) page_get_reqs: Arc<AtomicUsize>,
    pub(crate) comment_reqs: Arc<AtomicUsize>,
    pub(crate) block_reqs: Arc<AtomicUsize>,
    pub(crate) block_patch_reqs: Arc<Mutex<Vec<(String, Value)>>>,
    pub(crate) page_patch_reqs: Arc<Mutex<Vec<(String, Value)>>>,
    /// Optional cap on `/v1/search` page size (0 = honor client `page_size`).
    pub(crate) search_page_limit: Arc<AtomicUsize>,
    /// When true, `/v1/search` filters by query substring/id and `filter.value`.
    pub(crate) filter_search: Arc<std::sync::atomic::AtomicBool>,
    /// 1-based request number on which POST /v1/search returns HTTP 400 once (0 = disabled).
    pub(crate) fail_search_on_req: Arc<AtomicUsize>,
    pub(crate) search_cursor_fault: Arc<Mutex<CursorFault>>,
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

pub(crate) fn page_obj(id: &str, title: &str, edited: &str) -> Value {
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

pub(crate) fn db_obj(id: &str, title: &str, edited: &str) -> Value {
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

pub(crate) fn db_row_obj(id: &str, db_id: &str, title: &str, status: &str, edited: &str) -> Value {
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

pub(crate) fn paragraph_block(id: &str, text: &str) -> Value {
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

pub(crate) fn heading2_block(id: &str, text: &str) -> Value {
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

pub(crate) fn toggle_block(id: &str, text: &str, has_children: bool) -> Value {
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

pub(crate) fn comment_obj(page_id: &str, idx: usize) -> Value {
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

pub(crate) fn standard_mock() -> ReadContractMock {
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

pub(crate) fn parse_cursor_offset(cursor: Option<&str>) -> usize {
    cursor
        .and_then(|c| c.strip_prefix("cur-").unwrap_or(c).parse::<usize>().ok())
        .unwrap_or(0)
}

pub(crate) async fn start_mock_server(state: ReadContractMock) -> (String, tokio::task::JoinHandle<()>) {
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

pub(crate) fn mcp_runtime(base_url: &str) -> (AppConfig, AuthManager, NotionClient, Storage) {
    let mut cfg = AppConfig {
        api_base_url: format!("{base_url}/v1"),
        ..Default::default()
    };
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
