use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    mcp,
    notion::NotionClient,
    serve,
    storage::Storage,
};

use crate::server::test_config;

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

