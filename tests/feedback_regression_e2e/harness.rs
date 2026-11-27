use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::{net::TcpListener, process::Command};

pub(crate) const PAGE_ID: &str = "10000000-0000-0000-0000-000000000001";
pub(crate) const COL_LIST_ID: &str = "20000000-0000-0000-0000-000000000030";
pub(crate) const COL_LEFT_ID: &str = "20000000-0000-0000-0000-000000000031";
pub(crate) const COL_RIGHT_ID: &str = "20000000-0000-0000-0000-000000000032";
pub(crate) const PARA_LEFT_ID: &str = "20000000-0000-0000-0000-000000000034";
pub(crate) const PARA_RIGHT_ID: &str = "20000000-0000-0000-0000-000000000035";

#[derive(Clone, Debug)]
pub(crate) struct RecordedWrite {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: Value,
}

#[derive(Clone)]
pub(crate) struct ColumnMockState {
    pub(crate) blocks_by_parent: Arc<Mutex<HashMap<String, Vec<Value>>>>,
    pub(crate) writes: Arc<Mutex<Vec<RecordedWrite>>>,
    pub(crate) drift_on_next_root_get: Arc<AtomicBool>,
}

impl ColumnMockState {
    pub(crate) fn seeded() -> Self {
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

    pub(crate) fn take_writes(&self) -> Vec<RecordedWrite> {
        std::mem::take(&mut *self.writes.lock().unwrap())
    }
}

pub(crate) fn paragraph_json(id: &str, parent_block_id: &str, content: &str) -> Value {
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

pub(crate) async fn start_column_mock_server() -> (String, ColumnMockState, tokio::task::JoinHandle<()>) {
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

pub(crate) fn write_editor_script(dir: &std::path::Path, name: &str, body_script: &str) -> String {
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

pub(crate) fn notion_cli_cmd(base_url: &str, cfg_dir: &std::path::Path, data_dir: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_notion"));
    cmd.env("NOTION_API_BASE_URL", format!("{base_url}/v1"))
        .env("NOTION_TOKEN", "ntn_mock_token")
        .env("NOTION_CONFIG_DIR", cfg_dir)
        .env("NOTION_DATA_DIR", data_dir)
        .env("NOTION_DISABLE_KEYRING", "1")
        .env("NOTION_CLI_CACHE_SECS", "0");
    cmd
}

pub(crate) fn extract_recovery_path(stderr: &str) -> Option<PathBuf> {
    stderr.lines().find_map(|line| {
        line.split_once("Edited text saved for recovery at:")
            .map(|(_, path)| PathBuf::from(path.trim()))
    })
}

#[derive(Clone, Default)]
pub(crate) struct CliMockState {
    pub(crate) requests: Arc<Mutex<Vec<RecordedWrite>>>,
}

impl CliMockState {
    pub(crate) fn take(&self) -> Vec<RecordedWrite> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}
