use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use notion_cli::{
    auth::AuthManager,
    config::AppConfig,
    notion::NotionClient,
    serve,
    storage::Storage,
};

use crate::server::test_config;

#[derive(Clone, Debug)]
pub(crate) struct AuditRequest {
    pub(crate) method: String,
    pub(crate) path: String,
    // Read only through Debug, in assertion messages.
    #[allow(dead_code)]
    pub(crate) query: String,
    pub(crate) body: Value,
}

#[derive(Clone, Default)]
pub(crate) struct AuditMock {
    pub(crate) pages: Arc<Mutex<Vec<Value>>>,
    pub(crate) databases: Arc<Mutex<Vec<Value>>>,
    pub(crate) children: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    pub(crate) comments: Arc<Mutex<Vec<Value>>>,
    pub(crate) media: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    pub(crate) requests: Arc<Mutex<Vec<AuditRequest>>>,
    pub(crate) next_id: Arc<AtomicUsize>,
}

pub(crate) fn audit_text(text: &str) -> Value {
    json!([{ "type": "text", "text": { "content": text }, "plain_text": text }])
}

pub(crate) fn audit_page(id: &str, title: &str, parent: Value, last_edited: &str) -> Value {
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

pub(crate) fn audit_database(id: &str, title: &str, last_edited: &str) -> Value {
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

pub(crate) fn audit_block(id: &str, kind: &str, text: &str) -> Value {
    json!({
        "object": "block",
        "id": id,
        "type": kind,
        "has_children": false,
        kind: { "rich_text": audit_text(text) }
    })
}

pub(crate) fn audit_error(status: StatusCode, code: &str, message: impl Into<String>) -> axum::response::Response {
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

pub(crate) fn audit_not_found(what: &str, id: &str) -> axum::response::Response {
    audit_error(
        StatusCode::NOT_FOUND,
        "object_not_found",
        format!("Could not find {what} with ID: {id}."),
    )
}

/// Returns the first Notion request-limit violation in a write body.
pub(crate) fn audit_limit_violation(value: &Value) -> Option<String> {
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
pub(crate) fn audit_fill_plain_text(value: &mut Value) {
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

pub(crate) fn audit_title_of(object: &Value) -> String {
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

pub(crate) fn audit_paginate(items: Vec<Value>, page_size: usize, cursor: Option<&str>) -> Value {
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
    pub(crate) fn new_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        format!("{prefix}0000-0000-4000-8000-{n:012x}")
    }

    pub(crate) fn add_page(&self, page: Value) {
        self.pages.lock().unwrap().push(page);
    }

    pub(crate) fn set_children(&self, parent: &str, blocks: Vec<Value>) {
        self.children.lock().unwrap().insert(parent.to_string(), blocks);
    }

    pub(crate) fn children_of(&self, parent: &str) -> Vec<Value> {
        self.children.lock().unwrap().get(parent).cloned().unwrap_or_default()
    }

    pub(crate) fn page(&self, id: &str) -> Option<Value> {
        self.pages.lock().unwrap().iter().find(|p| p["id"] == id).cloned()
    }

    pub(crate) fn requests_matching(&self, method: &str, path_prefix: &str) -> Vec<AuditRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path.starts_with(path_prefix))
            .cloned()
            .collect()
    }

    pub(crate) fn count(&self, method: &str, path_prefix: &str) -> usize {
        self.requests_matching(method, path_prefix).len()
    }

    pub(crate) fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }

    pub(crate) fn block_exists(&self, id: &str) -> bool {
        self.children
            .lock()
            .unwrap()
            .values()
            .any(|list| list.iter().any(|b| b["id"] == id))
    }

    pub(crate) fn with_has_children(&self, mut block: Value) -> Value {
        let id = block["id"].as_str().unwrap_or_default().to_string();
        let has = self.children.lock().unwrap().get(&id).is_some_and(|c| !c.is_empty());
        block["has_children"] = json!(has);
        block
    }

    /// Stores one appended block (and its nested children) and returns the stored JSON.
    pub(crate) fn store_block(&self, mut block: Value) -> Value {
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

    pub(crate) async fn handle(
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

pub(crate) struct AuditEnv {
    pub(crate) mock: AuditMock,
    pub(crate) cfg: AppConfig,
    pub(crate) auth: AuthManager,
    pub(crate) client: NotionClient,
    pub(crate) storage: Storage,
    pub(crate) server: tokio::task::JoinHandle<()>,
}

pub(crate) async fn start_audit_env() -> AuditEnv {
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
pub(crate) async fn start_audit_api(
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

pub(crate) const AUDIT_PAGE_A: &str = "a11d0000-0000-4000-8000-00000000000a";
pub(crate) const AUDIT_PAGE_B: &str = "a11d0000-0000-4000-8000-00000000000b";
pub(crate) const AUDIT_DB: &str = "a11d0000-0000-4000-8000-0000000000db";
