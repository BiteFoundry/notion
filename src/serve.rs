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

use std::{collections::HashSet, net::IpAddr, sync::Arc};

use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{AllowOrigin, CorsLayer};

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

/// Environment variable read when `--api-key` is not passed (keeps the key out of the process list).
pub const SERVE_API_KEY_ENV: &str = "NOTION_SERVE_API_KEY";

/// Request-guard settings for the HTTP API router.
///
/// The default allows only loopback `Host` names and no browser origins, which is what
/// `build_router` uses.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Extra `Host` names accepted besides `127.0.0.1`, `localhost` and `::1` (port ignored).
    pub allowed_hosts: Vec<String>,
    /// Exact browser origins (`scheme://host[:port]`) that may call the API cross-origin.
    pub cors_origins: Vec<String>,
    /// When false the `Host` header is not checked (non-loopback bind without `--allowed-host`).
    pub enforce_host: bool,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            allowed_hosts: Vec::new(),
            cors_origins: Vec::new(),
            enforce_host: true,
        }
    }
}

/// Runtime inputs of `notion serve` that come from the command line.
#[derive(Debug, Clone, Default)]
pub struct ServeArgs {
    pub api_key: Option<String>,
    pub allow_keyless: bool,
    pub allowed_hosts: Vec<String>,
    pub cors_origins: Vec<String>,
}

/// How `/api/v1/*` is protected for one `notion serve` run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyMode {
    /// Key passed with `--api-key` or `NOTION_SERVE_API_KEY`.
    Provided(String),
    /// No key was given on a loopback bind: a random key valid for this run.
    Generated(String),
    /// `--allow-keyless` on a loopback bind.
    Keyless,
}

impl KeyMode {
    fn into_key(self) -> Option<String> {
        match self {
            KeyMode::Provided(key) | KeyMode::Generated(key) => Some(key),
            KeyMode::Keyless => None,
        }
    }
}

/// Decides the API key for a run. Pure so it can be tested without binding a socket.
pub fn resolve_serve_key(
    flag: Option<&str>,
    env: Option<&str>,
    allow_keyless: bool,
    is_loopback: bool,
    host: &str,
) -> Result<KeyMode> {
    let given = [flag, env]
        .into_iter()
        .flatten()
        .find(|key| !key.trim().is_empty());
    if let Some(key) = given {
        return Ok(KeyMode::Provided(key.to_string()));
    }
    if !is_loopback {
        bail!("Refusing to bind non-loopback address {host} without --api-key");
    }
    if allow_keyless {
        return Ok(KeyMode::Keyless);
    }
    Ok(KeyMode::Generated(crate::auth::generate_random_hex(32)))
}

/// Lowercased host name of a `Host` header value without its port; `None` when malformed.
fn host_name_of(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.contains(['/', '@', '?', '#', ' ', '\\']) {
        return None;
    }
    let host = if let Some(rest) = value.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        if !(after.is_empty() || is_port_suffix(after)) {
            return None;
        }
        inner
    } else {
        match value.rsplit_once(':') {
            Some((name, port)) if !name.contains(':') => {
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                name
            }
            // A bare IPv6 literal such as `::1` (no brackets, so no port).
            Some(_) => value,
            None => value,
        }
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

fn is_port_suffix(after: &str) -> bool {
    after
        .strip_prefix(':')
        .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
}

/// Canonical `scheme://host[:port]` form of an origin; `None` for anything else (`null`, paths, `*`).
fn normalize_origin(value: &str) -> Option<String> {
    let value = value.trim();
    let parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if !matches!(parsed.path(), "" | "/") || parsed.query().is_some() || parsed.fragment().is_some()
    {
        return None;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return None;
    }
    Some(parsed.origin().ascii_serialization())
}

struct RequestGuard {
    allowed_hosts: HashSet<String>,
    allowed_origins: HashSet<String>,
    enforce_host: bool,
}

impl RequestGuard {
    fn new(options: &ServeOptions) -> Self {
        let mut allowed_hosts: HashSet<String> = ["127.0.0.1", "localhost", "::1"]
            .into_iter()
            .map(str::to_string)
            .collect();
        allowed_hosts.extend(options.allowed_hosts.iter().filter_map(|h| host_name_of(h)));
        Self {
            allowed_hosts,
            allowed_origins: options
                .cors_origins
                .iter()
                .filter_map(|o| normalize_origin(o))
                .collect(),
            enforce_host: options.enforce_host,
        }
    }

    fn host_allowed(&self, host: Option<&str>) -> bool {
        if !self.enforce_host {
            return true;
        }
        host.and_then(host_name_of)
            .is_some_and(|name| self.allowed_hosts.contains(&name))
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        normalize_origin(origin).is_some_and(|o| self.allowed_origins.contains(&o))
    }
}

fn forbidden(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "ok": false, "error": message })),
    )
        .into_response()
}

/// Rejects DNS-rebinding (`Host`) and cross-site browser requests (`Origin`, `Sec-Fetch-Site`)
/// before any handler runs. Runs for every route, `/health` included.
async fn origin_host_guard(
    State(guard): State<Arc<RequestGuard>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let headers = req.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| req.uri().authority().map(|a| a.as_str()));
    if !guard.host_allowed(host) {
        return forbidden("Forbidden host");
    }
    match headers.get(header::ORIGIN) {
        Some(origin) => {
            if !origin.to_str().is_ok_and(|o| guard.origin_allowed(o)) {
                return forbidden("Forbidden origin");
            }
        }
        None => {
            // Browsers omit `Origin` on some no-CORS requests but still send `Sec-Fetch-Site`.
            let site = headers
                .get("sec-fetch-site")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_ascii_lowercase());
            if matches!(site.as_deref(), Some("cross-site" | "same-site")) {
                return forbidden("Forbidden origin");
            }
        }
    }
    next.run(req).await
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn from_anyhow(err: anyhow::Error) -> Self {
        let msg = err.to_string();
        let status = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<crate::notion::NotionApiError>())
            .and_then(|api| StatusCode::from_u16(api.status).ok())
            .unwrap_or_else(|| {
                let lower = msg.to_ascii_lowercase();
                if lower.contains("not found") || lower.contains("404") {
                    StatusCode::NOT_FOUND
                } else if lower.contains("empty")
                    || lower.contains("invalid")
                    || lower.contains("required")
                {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            });
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

#[derive(Debug, Default, Deserialize)]
struct PageGetParams {
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    refresh: bool,
    #[serde(default)]
    fields: Option<String>,
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
    /// `next_cursor` from the previous response.
    #[serde(default)]
    cursor: Option<String>,
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
    build_router_with(state, ServeOptions::default())
}

/// `build_router` with explicit `Host` / origin settings (used by `notion serve`).
pub fn build_router_with(state: Arc<ApiState>, options: ServeOptions) -> Router {
    let guard = Arc::new(RequestGuard::new(&options));
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

    let mut app = Router::new()
        .route("/health", get(health_handler))
        .merge(protected);
    // No CORS headers at all unless an origin was opted in with `--cors-origin`.
    if !guard.allowed_origins.is_empty() {
        let origins: Vec<HeaderValue> = guard
            .allowed_origins
            .iter()
            .filter_map(|o| HeaderValue::from_str(o).ok())
            .collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST, Method::PATCH])
                .allow_headers([
                    header::CONTENT_TYPE,
                    header::AUTHORIZATION,
                    HeaderName::from_static("x-api-key"),
                ]),
        );
    }
    app.layer(middleware::from_fn_with_state(guard, origin_host_guard))
        .with_state(state)
}

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    client: NotionClient,
    storage: Storage,
    host: &str,
    port: u16,
    args: ServeArgs,
) -> Result<()> {
    let ip: IpAddr = host
        .parse()
        .with_context(|| format!("Invalid bind host IP: {host}"))?;
    let env_key = std::env::var(SERVE_API_KEY_ENV).ok();
    let key_mode = resolve_serve_key(
        args.api_key.as_deref(),
        env_key.as_deref(),
        args.allow_keyless,
        ip.is_loopback(),
        host,
    )?;
    for origin in &args.cors_origins {
        if normalize_origin(origin).is_none() {
            bail!("Invalid --cors-origin {origin:?}: expected an exact origin such as https://app.example.com (no path, no wildcard)");
        }
    }
    for allowed in &args.allowed_hosts {
        if host_name_of(allowed).is_none() {
            bail!("Invalid --allowed-host {allowed:?}: expected a host name or IP address");
        }
    }
    let mut allowed_hosts = args.allowed_hosts.clone();
    allowed_hosts.push(host.to_string());
    let options = ServeOptions {
        // A wildcard bind such as 0.0.0.0 cannot enumerate its valid names, and that mode
        // always requires a key, so the Host check there is opt-in through --allowed-host.
        enforce_host: ip.is_loopback() || !args.allowed_hosts.is_empty(),
        allowed_hosts,
        cors_origins: args.cors_origins,
    };
    // All notices go to stderr; the generated key is shown once and never stored.
    match &key_mode {
        KeyMode::Provided(_) => {}
        KeyMode::Generated(key) => {
            eprintln!("API key (generated, valid for this run): {key}");
            eprintln!(
                "Send it as `Authorization: Bearer <key>` or `x-api-key: <key>`. Pass --api-key or set {SERVE_API_KEY_ENV} to choose your own; --allow-keyless turns the key off."
            );
        }
        KeyMode::Keyless => {
            eprintln!(
                "Warning: --allow-keyless is set; any local process can call this API without a key."
            );
        }
    }
    let state = Arc::new(ApiState {
        config,
        auth,
        client,
        storage,
        api_key: key_mode.into_key(),
    });
    let app = build_router_with(state, options);
    let addr = if ip.is_ipv6() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind HTTP API server on {addr}"))?;
    eprintln!("@bitefoundry/notion HTTP API listening on http://{addr}");
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
        let matches = got.is_some_and(|key| {
            crate::models::constant_time_eq(key.as_bytes(), expected.as_bytes())
        });
        if !matches {
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
    let pages = cli::op_list_pages(
        &state.client,
        &state.storage,
        q.archived,
        q.refresh,
        q.offline,
        usize::MAX,
        None,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(
        json!({ "ok": true, "count": pages.items.len(), "pages": pages.items }),
    ))
}

async fn get_page_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<PageGetParams>,
) -> Result<Json<Value>, ApiError> {
    if let Some(raw_fields) = q.fields.as_deref() {
        let fields: Vec<String> = raw_fields
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !fields.is_empty() {
            let projected = cli::op_get_page_fields(
                &state.client,
                &state.storage,
                &id,
                q.refresh,
                q.offline,
                &fields,
            )
            .await
            .map_err(ApiError::from_anyhow)?;
            return Ok(Json(json!({ "ok": true, "detail": projected })));
        }
    }
    let (detail, comments_has_more) =
        cli::op_get_page_for_output(&state.client, &state.storage, &id, q.refresh, q.offline)
            .await
            .map_err(ApiError::from_anyhow)?;
    Ok(Json(
        json!({ "ok": true, "detail": detail.full_json(comments_has_more) }),
    ))
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
    // An empty or whitespace icon is ignored, as before.
    let icon = body.icon.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let patch = cli::PagePatch {
        title: body.title.as_deref(),
        icon,
        archived: body.archived,
        properties: body.properties,
    };
    let updated = cli::op_patch_page(&state.client, &state.storage, &id, patch)
        .await
        .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({ "ok": true, "page": updated })))
}

async fn get_page_blocks_handler(
    State(state): State<Arc<ApiState>>,
    AxumPath(id): AxumPath<String>,
    Query(q): Query<OfflineParams>,
) -> Result<Json<Value>, ApiError> {
    let blocks =
        cli::op_list_blocks(&state.client, &state.storage, &id, q.offline, usize::MAX, None)
            .await
            .map_err(ApiError::from_anyhow)?;
    Ok(Json(
        json!({ "ok": true, "count": blocks.items.len(), "blocks": blocks.items }),
    ))
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
            &state.storage,
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
    let dbs = cli::op_list_databases(
        &state.client,
        &state.storage,
        q.refresh,
        q.offline,
        usize::MAX,
        None,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(
        json!({ "ok": true, "count": dbs.items.len(), "databases": dbs.items }),
    ))
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
        body.cursor.as_deref().map(str::trim).filter(|c| !c.is_empty()),
        body.offline,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({
        "ok": true,
        "count": rows.items.len(),
        "rows": rows.items,
        "has_more": rows.has_more,
        "next_cursor": rows.next_cursor,
    })))
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
    let comments = cli::op_list_comments(
        &state.client,
        &state.storage,
        &id,
        q.offline,
        usize::MAX,
        None,
    )
    .await
    .map_err(ApiError::from_anyhow)?;
    Ok(Json(
        json!({ "ok": true, "count": comments.items.len(), "comments": comments.items }),
    ))
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

#[derive(Debug, Default, Deserialize)]
struct SyncParams {
    /// `?full=true` rescans the whole workspace instead of stopping at the last sync point.
    #[serde(default)]
    full: bool,
}

async fn sync_handler(
    State(state): State<Arc<ApiState>>,
    Query(params): Query<SyncParams>,
) -> Result<Json<Value>, ApiError> {
    let full = params.full;
    let (workspace_id, pages, databases, outcome) =
        sync::sync_workspace_tree_with_options(&state.client, &state.storage, full, |_, _, _| {})
            .await
            .map_err(ApiError::from_anyhow)?;
    Ok(Json(json!({
        "ok": true,
        "workspace_id": workspace_id,
        "pages_count": pages.len(),
        "databases_count": databases.len(),
        "fetched_count": outcome.fetched,
        "complete": true,
        "full": full,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_name_parsing_accepts_loopback_forms_and_rejects_lookalikes() {
        assert_eq!(host_name_of("127.0.0.1:8791").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_name_of("localhost").as_deref(), Some("localhost"));
        assert_eq!(host_name_of("LOCALHOST:80").as_deref(), Some("localhost"));
        assert_eq!(host_name_of("localhost.").as_deref(), Some("localhost"));
        assert_eq!(host_name_of("[::1]:8791").as_deref(), Some("::1"));
        assert_eq!(host_name_of("[::1]").as_deref(), Some("::1"));
        assert_eq!(host_name_of("::1").as_deref(), Some("::1"));
        assert_eq!(host_name_of(""), None);
        assert_eq!(host_name_of("   "), None);
        assert_eq!(host_name_of("127.0.0.1:"), None);
        assert_eq!(host_name_of("127.0.0.1:80a"), None);
        assert_eq!(host_name_of("[::1]evil"), None);
        assert_eq!(host_name_of("user@127.0.0.1"), None);
        assert_eq!(host_name_of("127.0.0.1/evil"), None);

        let guard = RequestGuard::new(&ServeOptions::default());
        for ok in ["127.0.0.1:8791", "localhost", "[::1]:8791", "localhost:1"] {
            assert!(guard.host_allowed(Some(ok)), "{ok}");
        }
        for bad in [
            "evil.example",
            "127.0.0.1.evil.example",
            "localhost.evil.example:8791",
            "evil.example:8791",
            "",
        ] {
            assert!(!guard.host_allowed(Some(bad)), "{bad}");
        }
        assert!(!guard.host_allowed(None));
    }

    #[test]
    fn host_guard_honours_allowed_hosts_and_opt_out() {
        let guard = RequestGuard::new(&ServeOptions {
            allowed_hosts: vec!["notion.internal".into(), "10.0.0.5".into()],
            ..ServeOptions::default()
        });
        assert!(guard.host_allowed(Some("notion.internal:8791")));
        assert!(guard.host_allowed(Some("10.0.0.5")));
        assert!(guard.host_allowed(Some("127.0.0.1:8791")));
        assert!(!guard.host_allowed(Some("other.internal")));

        let open = RequestGuard::new(&ServeOptions {
            enforce_host: false,
            ..ServeOptions::default()
        });
        assert!(open.host_allowed(Some("anything.example")));
        assert!(open.host_allowed(None));
    }

    #[test]
    fn origins_match_exactly_and_never_by_prefix_or_wildcard() {
        let guard = RequestGuard::new(&ServeOptions {
            cors_origins: vec!["https://app.example.com".into(), "*".into()],
            ..ServeOptions::default()
        });
        assert!(guard.origin_allowed("https://app.example.com"));
        assert!(guard.origin_allowed("HTTPS://APP.EXAMPLE.COM"));
        assert!(guard.origin_allowed("https://app.example.com:443"));
        assert!(!guard.origin_allowed("http://app.example.com"));
        assert!(!guard.origin_allowed("https://app.example.com.evil.example"));
        assert!(!guard.origin_allowed("https://app.example.com:8443"));
        assert!(!guard.origin_allowed("null"));
        assert!(!guard.origin_allowed("*"));
        assert_eq!(normalize_origin("https://app.example.com/path"), None);
        assert_eq!(normalize_origin("*"), None);

        let none = RequestGuard::new(&ServeOptions::default());
        assert!(!none.origin_allowed("http://127.0.0.1:8791"));
    }

    #[test]
    fn serve_key_resolution_covers_loopback_and_remote_binds() {
        // Non-loopback without a key always fails, with or without --allow-keyless.
        for keyless in [false, true] {
            let err = resolve_serve_key(None, None, keyless, false, "0.0.0.0").unwrap_err();
            assert!(err.to_string().contains("Refusing to bind non-loopback"));
        }
        assert!(resolve_serve_key(Some("  "), Some(""), false, false, "0.0.0.0").is_err());
        assert_eq!(
            resolve_serve_key(Some("k1"), None, false, false, "0.0.0.0").unwrap(),
            KeyMode::Provided("k1".into())
        );

        // Loopback without a key generates 32 random bytes (64 hex characters).
        let KeyMode::Generated(first) =
            resolve_serve_key(None, None, false, true, "127.0.0.1").unwrap()
        else {
            panic!("expected a generated key");
        };
        let KeyMode::Generated(second) =
            resolve_serve_key(None, Some(" "), false, true, "127.0.0.1").unwrap()
        else {
            panic!("expected a generated key");
        };
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(first, second);

        // --allow-keyless only applies on loopback and only when no key was given.
        assert_eq!(
            resolve_serve_key(None, None, true, true, "127.0.0.1").unwrap(),
            KeyMode::Keyless
        );
        assert_eq!(
            resolve_serve_key(Some("k2"), None, true, true, "127.0.0.1").unwrap(),
            KeyMode::Provided("k2".into())
        );
        // The flag wins over the environment variable.
        assert_eq!(
            resolve_serve_key(Some("flag"), Some("env"), false, true, "127.0.0.1").unwrap(),
            KeyMode::Provided("flag".into())
        );
        assert_eq!(
            resolve_serve_key(None, Some("env"), false, true, "127.0.0.1").unwrap(),
            KeyMode::Provided("env".into())
        );
    }
}
