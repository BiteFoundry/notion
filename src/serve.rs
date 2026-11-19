//! Local HTTP JSON API server (`notion serve`, default `127.0.0.1:8791`).
//!
//! Endpoints:
//! - `GET /health`
//! - `GET /api/v1/whoami`
//! - `GET /api/v1/workspaces`
//! - `GET /api/v1/search`
//! - `GET /api/v1/pages`, `POST /api/v1/pages`
//! - `GET /api/v1/pages/{id}`, `PATCH /api/v1/pages/{id}`
//! - `GET /api/v1/pages/{id}/blocks`, `POST /api/v1/pages/{id}/blocks`
//! - `PATCH /api/v1/blocks/{id}`
//! - `GET /api/v1/databases`
//! - `POST /api/v1/databases/{id}/query`
//! - `POST /api/v1/databases/{id}/items`
//! - `GET /api/v1/pages/{id}/comments`, `POST /api/v1/pages/{id}/comments`
//! - `POST /api/v1/sync`

use std::{net::IpAddr, sync::Arc};

use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};

use crate::{
    auth::AuthManager,
    cli,
    config::AppConfig,
    notion::NotionClient,
    storage::Storage,
    sync,
};

pub struct ApiState {
    pub config: AppConfig,
    pub auth: AuthManager,
    pub client: NotionClient,
    pub storage: Storage,
    pub api_key: Option<String>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn from_anyhow(err: anyhow::Error) -> Self {
        let msg = err.to_string();
        let lower = msg.to_ascii_lowercase();
        let status = if lower.contains("not found") || lower.contains("404") {
            StatusCode::NOT_FOUND
        } else if lower.contains("empty") || lower.contains("invalid") || lower.contains("required")
        {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        Self {
            status,
            message: msg,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "ok": false, "error": self.message })),
        )
            .into_response()
    }
}

#[derive(Debug, Default, Deserialize)]
struct SearchParams {
    q: String,
    #[serde(default)]
    remote: bool,
    #[serde(default)]
    offline: bool,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Debug, Default, Deserialize)]
struct PagesListParams {
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    refresh: bool,
    #[serde(default)]
    offline: bool,
}

#[derive(Debug, Default, Deserialize)]
struct OfflineParams {
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    refresh: bool,
}

#[derive(Debug, Deserialize)]
struct CreatePageBody {
    parent_id: String,
    title: String,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    markdown: Option<String>,
    #[serde(default)]
    parent_is_database: bool,
    #[serde(default)]
    properties: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct PatchPageBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    archived: Option<bool>,
    #[serde(default)]
    properties: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct AppendBlocksBody {
    markdown: String,
}

#[derive(Debug, Default, Deserialize)]
struct PatchBlockBody {
    #[serde(default)]
    block_type: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    checked: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct QueryDbBody {
    #[serde(default)]
    filter: Option<Value>,
    #[serde(default)]
    sorts: Option<Value>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    offline: bool,
}

#[derive(Debug, Deserialize)]
struct CreateDbItemBody {
    title: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    properties: Option<Value>,
    #[serde(default)]
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AddCommentBody {
    text: String,
    #[serde(default)]
    discussion_id: Option<String>,
}

fn default_limit() -> usize {
    50
}

pub fn build_router(state: Arc<ApiState>) -> Router {
    let protected = Router::new()
        .route("/api/v1/whoami", get(whoami_handler))
        .route("/api/v1/workspaces", get(workspaces_handler))
        .route("/api/v1/search", get(search_handler))
        .route(
            "/api/v1/pages",
            get(list_pages_handler).post(create_page_handler),
        )
        .route(
            "/api/v1/pages/{id}",
            get(get_page_handler).patch(patch_page_handler),
        )
        .route(
            "/api/v1/pages/{id}/blocks",
            get(get_page_blocks_handler).post(append_page_blocks_handler),
        )
        .route("/api/v1/blocks/{id}", patch(patch_block_handler))
        .route("/api/v1/databases", get(list_databases_handler))
        .route("/api/v1/databases/{id}/query", post(query_database_handler))
        .route(
            "/api/v1/databases/{id}/items",
            post(create_database_item_handler),
        )
        .route(
            "/api/v1/pages/{id}/comments",
            get(list_comments_handler).post(add_comment_handler),
        )
        .route("/api/v1/sync", post(sync_handler))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth_middleware,
        ));

    Router::new()
        .route("/health", get(health_handler))
        .merge(protected)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    client: NotionClient,
    storage: Storage,
    host: &str,
    port: u16,
    api_key: Option<String>,
) -> Result<()> {
    let ip: IpAddr = host
        .parse()
        .with_context(|| format!("Invalid bind host IP: {host}"))?;
    if !ip.is_loopback() && api_key.as_deref().unwrap_or("").trim().is_empty() {
        bail!("Refusing to bind non-loopback address {host} without --api-key");
    }
    let state = Arc::new(ApiState {
        config,
        auth,
        client,
        storage,
        api_key: api_key.filter(|k| !k.trim().is_empty()),
    });
    let app = build_router(state);
    let addr = format!("{host}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind HTTP API server on {addr}"))?;
    eprintln!("@shinick/notion HTTP API listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .context("HTTP API server error")
}

async fn auth_middleware(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(ref expected) = state.api_key {
        let got = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.strip_prefix("Bearer "))
            });
        if got != Some(expected.as_str()) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "ok": false, "error": "Unauthorized" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

async fn health_handler() -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "notion-cli",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn whoami_handler(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let val = cli::op_whoami(&state.config, &state.auth, &state.client)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(val))
}

async fn workspaces_handler(State(state): State<Arc<ApiState>>) -> Json<Value> {
    Json(cli::op_list_workspaces(&state.config))
}

async fn search_handler(
    State(state): State<Arc<ApiState>>,
    Query(q): Query<SearchParams>,
) -> Result<Json<Value>, ApiError> {
    let hits = cli::op_search(
        &state.client,
        &state.storage,
        &q.q,
        q.remote,
        q.offline,
        q.limit,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "count": hits.len(), "results": hits })))
}

async fn list_pages_handler(
    State(state): State<Arc<ApiState>>,
    Query(q): Query<PagesListParams>,
) -> Result<Json<Value>, ApiError> {
    let pages = cli::op_list_pages(&state.client, &state.storage, q.archived, q.refresh, q.offline)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "count": pages.len(), "pages": pages })))
}

async fn get_page_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<OfflineParams>,
) -> Result<Json<Value>, ApiError> {
    let detail = cli::op_get_page(&state.client, &state.storage, &id, q.offline)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "detail": detail })))
}

async fn create_page_handler(
    State(state): State<Arc<ApiState>>,
    Json(body): Json<CreatePageBody>,
) -> Result<Json<Value>, ApiError> {
    let created = cli::op_create_page(
        &state.client,
        &state.storage,
        &body.parent_id,
        &body.title,
        body.icon.as_deref(),
        body.markdown.as_deref(),
        body.parent_is_database,
        body.properties,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "page": created })))
}

async fn patch_page_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<PatchPageBody>,
) -> Result<Json<Value>, ApiError> {
    if let Some(archived) = body.archived {
        let updated = cli::op_archive_page(&state.client, &state.storage, &id, archived)
            .await
            .map_err(ApiError::from_anyhow)?;
        return Ok(Json(json!({ "ok": true, "page": updated })));
    }
    if body.properties.is_some() {
        let updated = cli::op_update_database_item(
            &state.client,
            &state.storage,
            &id,
            body.title.as_deref(),
            None,
            body.properties,
        )
        .await
        .map_err(ApiError::from_anyhow)?;
        return Ok(Json(json!({ "ok": true, "page": updated })));
    }
    let updated = cli::op_update_page_meta(
        &state.client,
        &state.storage,
        &id,
        body.title.as_deref(),
        body.icon.as_deref(),
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "page": updated })))
}

async fn get_page_blocks_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<OfflineParams>,
) -> Result<Json<Value>, ApiError> {
    let blocks = cli::op_list_blocks(&state.client, &state.storage, &id, q.offline)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "count": blocks.len(), "blocks": blocks })))
}

async fn append_page_blocks_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<AppendBlocksBody>,
) -> Result<Json<Value>, ApiError> {
    let blocks = cli::op_append_blocks(&state.client, &state.storage, &id, &body.markdown)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "appended": blocks.len(), "blocks": blocks })))
}

async fn patch_block_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<PatchBlockBody>,
) -> Result<Json<Value>, ApiError> {
    let updated = if let Some(ref text) = body.text {
        cli::op_update_block_text(
            &state.client,
            &id,
            body.block_type.as_deref(),
            text,
            body.checked,
        )
        .await
        .map_err(ApiError::from_anyhow)?
    } else {
        cli::op_toggle_todo(&state.client, &state.storage, &id, body.checked)
            .await
            .map_err(ApiError::from_anyhow)?
    };
    Ok(Json(json!({ "ok": true, "block": updated })))
}

async fn list_databases_handler(
    State(state): State<Arc<ApiState>>,
    Query(q): Query<OfflineParams>,
) -> Result<Json<Value>, ApiError> {
    let dbs = cli::op_list_databases(&state.client, &state.storage, q.refresh, q.offline)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "count": dbs.len(), "databases": dbs })))
}

async fn query_database_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<QueryDbBody>,
) -> Result<Json<Value>, ApiError> {
    let rows = cli::op_query_database(
        &state.client,
        &state.storage,
        &id,
        body.filter,
        body.sorts,
        body.limit,
        body.offline,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "count": rows.len(), "rows": rows })))
}

async fn create_database_item_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<CreateDbItemBody>,
) -> Result<Json<Value>, ApiError> {
    let item = cli::op_create_database_item(
        &state.client,
        &state.storage,
        &id,
        &body.title,
        body.status.as_deref(),
        body.properties,
        body.body.as_deref(),
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "item": item })))
}

async fn list_comments_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<OfflineParams>,
) -> Result<Json<Value>, ApiError> {
    let comments = cli::op_list_comments(&state.client, &state.storage, &id, q.offline)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "count": comments.len(), "comments": comments })))
}

async fn add_comment_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<AddCommentBody>,
) -> Result<Json<Value>, ApiError> {
    let comment = cli::op_add_comment(
        &state.client,
        &state.storage,
        &id,
        body.discussion_id.as_deref(),
        &body.text,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "comment": comment })))
}

async fn sync_handler(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let (workspace_id, pages, databases) = sync::sync_workspace_tree(&state.client, &state.storage)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({
        "ok": true,
        "workspace_id": workspace_id,
        "pages_count": pages.len(),
        "databases_count": databases.len(),
    })))
}
