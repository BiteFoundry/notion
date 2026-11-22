//! Typed async Notion REST API v1 client (`https://api.notion.com/v1`, `Notion-Version: 2022-06-28`).
//!
//! Features:
//! - Configurable base URL (supports mock Axum server for E2E tests)
//! - Automatic retry on HTTP 429 honoring `Retry-After` + transient 502/503/504 backoff
//! - Cursor pagination (`start_cursor` / `has_more`)
//! - Recursive block tree fetching with depth cap
//! - Automatic 100-block batching on `PATCH /v1/blocks/{block_id}/children`

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use reqwest::{
    header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER},
    Method, StatusCode,
};
use serde_json::{json, Value};
use tokio::time::Instant;

use crate::{
    auth::{parse_notion_user, AuthManager},
    config::{AppConfig, NOTION_API_VERSION},
    models::{
        normalize_notion_id, NotionBlock, NotionComment, NotionDatabase, NotionPage,
        NotionPageDetail, NotionUser,
    },
    parser::{
        blocks_to_markdown, blocks_to_plain_text, collect_media_refs, parse_notion_block,
        parse_notion_comment, parse_notion_database, parse_notion_page,
    },
};

const MAX_RETRIES: usize = 4;
const DEFAULT_BLOCK_TREE_DEPTH: usize = 12;
/// Comments carried inside a page detail. One extra comment is fetched to learn whether more exist.
pub const DETAIL_COMMENTS_LIMIT: usize = 2000;
const MAX_APPEND_BATCH_SIZE: usize = 100;
const STATUS_SERVICE_OVERLOAD: u16 = 529;
const MIN_RETRY_AFTER_SECS: f64 = 0.05;

#[derive(Debug, Clone)]
pub struct NotionApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for NotionApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match StatusCode::from_u16(self.status) {
            Ok(st) => write!(
                f,
                "Notion API error (HTTP {st}, {}): {}",
                self.code, self.message
            ),
            Err(_) => write!(
                f,
                "Notion API error (HTTP {}, {}): {}",
                self.status, self.code, self.message
            ),
        }
    }
}

impl std::error::Error for NotionApiError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFilterObject {
    All,
    Page,
    Database,
}

#[derive(Debug, Clone, Default)]
pub struct SearchResponse {
    pub pages: Vec<NotionPage>,
    pub databases: Vec<NotionDatabase>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct NotionClient {
    http: reqwest::Client,
    auth: AuthManager,
    api_base_url: String,
    rate_limit_until: Arc<Mutex<Option<Instant>>>,
}

impl NotionClient {
    pub fn new(config: &AppConfig, auth: AuthManager) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(45))
            .user_agent(format!("notion-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("Failed to initialize Notion HTTP client")?;
        Ok(Self {
            http,
            auth,
            api_base_url: config.effective_api_base_url().trim_end_matches('/').to_string(),
            rate_limit_until: Arc::new(Mutex::new(None)),
        })
    }

    /// Remaining cooldown recorded from the most recent HTTP 429/529 `Retry-After` response.
    pub fn rate_limit_remaining(&self) -> Duration {
        self.rate_limit_remaining_at(Instant::now())
    }

    fn rate_limit_remaining_at(&self, now: Instant) -> Duration {
        let guard = self
            .rate_limit_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .and_then(|until| until.checked_duration_since(now))
            .unwrap_or(Duration::ZERO)
    }

    fn record_rate_limit_wait(&self, wait: Duration) {
        self.record_rate_limit_wait_at(Instant::now(), wait);
    }

    fn record_rate_limit_wait_at(&self, now: Instant, wait: Duration) {
        let target = now + wait;
        let mut guard = self
            .rate_limit_until
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_none_or(|cur| target > cur) {
            *guard = Some(target);
        }
    }

    async fn wait_for_rate_limit_cooldown(&self) {
        self.wait_for_rate_limit_cooldown_with(Instant::now, tokio::time::sleep)
            .await;
    }

    async fn wait_for_rate_limit_cooldown_with<N, S, Fut>(&self, mut now_fn: N, mut sleep_fn: S)
    where
        N: FnMut() -> Instant,
        S: FnMut(Duration) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        loop {
            let remaining = self.rate_limit_remaining_at(now_fn());
            if remaining.is_zero() {
                return;
            }
            sleep_fn(remaining).await;
        }
    }

    fn parse_retry_after_header(
        raw: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Duration {
        let Some(trimmed) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
            return Duration::from_secs(1);
        };
        if let Ok(secs) = trimmed.parse::<f64>() {
            if secs.is_finite() && secs >= 0.0 {
                if let Ok(dur) = Duration::try_from_secs_f64(secs.max(MIN_RETRY_AFTER_SECS)) {
                    return dur;
                }
            }
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(trimmed) {
            let delta = dt.with_timezone(&chrono::Utc).signed_duration_since(now);
            let secs = (delta.num_milliseconds() as f64) / 1000.0;
            if secs.is_finite() {
                if let Ok(dur) = Duration::try_from_secs_f64(secs.max(MIN_RETRY_AFTER_SECS)) {
                    return dur;
                }
            }
        }
        Duration::from_secs(1)
    }

    fn parse_retry_after(resp: &reqwest::Response) -> Duration {
        let raw = resp.headers().get(RETRY_AFTER).and_then(|v| v.to_str().ok());
        Self::parse_retry_after_header(raw, chrono::Utc::now())
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.api_base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn base_url(&self) -> &str {
        &self.api_base_url
    }

    pub fn http_client(&self) -> &reqwest::Client {
        &self.http
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
    }

    /// Returns `true` only when a Notion API error is an authoritative HTTP 404 `object_not_found`.
    /// Transient 5xx, 429, network, auth (401/403), or generic errors return `false`.
    pub fn is_authoritative_not_found(err: &anyhow::Error) -> bool {
        let msg = err.to_string();
        msg.contains("HTTP 404") && msg.contains("object_not_found")
    }

    /// Executes an authenticated Notion REST API request with HTTP 429 (`Retry-After`) and 5xx retry.
    async fn request_json(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        self.request_json_version(method, path, body, NOTION_API_VERSION).await
    }

    async fn request_json_version(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        version: &str,
    ) -> Result<Value> {
        let token = self.auth.access_token().await?;
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}/{}", self.api_base_url, path.trim_start_matches('/'))
        };

        // Append has no idempotency key. A timeout or gateway error may follow a successful write.
        let can_retry_write = !(method == Method::PATCH && path.ends_with("/children"));
        let mut attempt = 0usize;
        loop {
            self.wait_for_rate_limit_cooldown().await;

            let mut req = self
                .http
                .request(method.clone(), &url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header("Notion-Version", version);
            if let Some(ref payload) = body {
                req = req
                    .header(CONTENT_TYPE, "application/json")
                    .json(payload);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(err) if attempt < MAX_RETRIES
                    && (err.is_connect() || (can_retry_write && err.is_timeout())) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(250 * (1u64 << attempt.min(4)))).await;
                    continue;
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("Notion API request failed: {method} {url}"));
                }
            };

            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                let wait = Self::parse_retry_after(&resp);
                self.record_rate_limit_wait(wait);
                if attempt < MAX_RETRIES {
                    attempt += 1;
                    continue;
                }
            }

            if status.as_u16() == STATUS_SERVICE_OVERLOAD {
                let wait = Self::parse_retry_after(&resp);
                let text = resp.text().await.unwrap_or_default();
                let parsed: Value =
                    serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
                let code = parsed
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("service_overload");
                if code != "public_api_request_blocked" {
                    self.record_rate_limit_wait(wait);
                    if can_retry_write && attempt < MAX_RETRIES {
                        attempt += 1;
                        continue;
                    }
                }
                let msg = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| text.trim());
                return Err(NotionApiError {
                    status: status.as_u16(),
                    code: code.to_string(),
                    message: msg.to_string(),
                }
                .into());
            }

            if matches!(
                status,
                StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ) && can_retry_write && attempt < MAX_RETRIES
            {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(300 * (1u64 << attempt.min(4)))).await;
                continue;
            }

            let text = resp.text().await.unwrap_or_default();
            let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));

            if !status.is_success() {
                let code = parsed
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let msg = parsed
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| text.trim());
                return Err(NotionApiError {
                    status: status.as_u16(),
                    code: code.to_string(),
                    message: msg.to_string(),
                }
                .into());
            }

            return Ok(parsed);
        }
    }

    /// `GET /v1/users/me`
    pub async fn get_me(&self) -> Result<NotionUser> {
        let val = self.request_json(Method::GET, "/users/me", None).await?;
        Ok(parse_notion_user(&val))
    }

    /// `GET /v1/users` (paginated)
    pub async fn list_users(&self) -> Result<Vec<NotionUser>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let path = match cursor.as_deref() {
                Some(c) => format!("/users?page_size=100&start_cursor={}", urlencoding::encode(c)),
                None => "/users?page_size=100".to_string(),
            };
            let val = self.request_json(Method::GET, &path, None).await?;
            if let Some(results) = val.get("results").and_then(Value::as_array) {
                for item in results {
                    out.push(parse_notion_user(item));
                }
            }
            if val.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                cursor = val
                    .get("next_cursor")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if cursor.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// `POST /v1/search` (single page)
    pub async fn search(
        &self,
        query: Option<&str>,
        filter: SearchFilterObject,
        page_size: usize,
        start_cursor: Option<&str>,
    ) -> Result<SearchResponse> {
        let workspace_id = self.auth.active_workspace_id().await;
        let mut body = serde_json::Map::new();
        if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
            body.insert("query".into(), json!(q));
        }
        match filter {
            SearchFilterObject::Page => {
                body.insert("filter".into(), json!({ "property": "object", "value": "page" }));
            }
            SearchFilterObject::Database => {
                body.insert(
                    "filter".into(),
                    json!({ "property": "object", "value": "database" }),
                );
            }
            SearchFilterObject::All => {}
        }
        body.insert(
            "sort".into(),
            json!({ "direction": "descending", "timestamp": "last_edited_time" }),
        );
        body.insert("page_size".into(), json!(page_size.clamp(1, 100)));
        if let Some(c) = start_cursor {
            body.insert("start_cursor".into(), json!(c));
        }

        let val = self
            .request_json(Method::POST, "/search", Some(Value::Object(body)))
            .await?;

        let mut pages = Vec::new();
        let mut databases = Vec::new();
        if let Some(results) = val.get("results").and_then(Value::as_array) {
            for item in results {
                match item.get("object").and_then(Value::as_str) {
                    Some("page") => {
                        if let Ok(p) = parse_notion_page(item, &workspace_id) {
                            pages.push(p);
                        }
                    }
                    Some("database") => {
                        if let Ok(db) = parse_notion_database(item, &workspace_id) {
                            databases.push(db);
                        }
                    }
                    _ => {}
                }
            }
        }

        let has_more = val
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next_cursor = val
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_string);

        Ok(SearchResponse {
            pages,
            databases,
            has_more,
            next_cursor,
        })
    }

    /// Paginated `POST /v1/search` fetching up to `max_items` across pages & databases.
    pub async fn search_all(
        &self,
        query: Option<&str>,
        filter: SearchFilterObject,
        max_items: usize,
    ) -> Result<SearchResponse> {
        self.search_incremental(query, filter, max_items, None, |_, _, _| {})
            .await
    }

    /// Incremental paginated `POST /v1/search` (sorted by `last_edited_time` descending).
    /// When `stop_before_edited` is provided, stops paginating as soon as a batch reaches items
    /// already older than or equal to the watermark — turning repeat syncs into a single API call.
    pub async fn search_incremental<F>(
        &self,
        query: Option<&str>,
        filter: SearchFilterObject,
        max_items: usize,
        stop_before_edited: Option<&str>,
        mut on_batch: F,
    ) -> Result<SearchResponse>
    where
        F: FnMut(usize, usize, usize),
    {
        let mut all_pages = Vec::new();
        let mut all_dbs = Vec::new();
        let mut cursor: Option<String> = None;
        let limit = max_items.max(1);
        let mut batch_num = 0usize;

        loop {
            batch_num += 1;
            let batch = self
                .search(query, filter, 100.min(limit), cursor.as_deref())
                .await?;

            // Check if this batch crossed our previous sync watermark (results are sorted newest-first)
            let hit_watermark = if let Some(wm) = stop_before_edited.filter(|s| !s.is_empty()) {
                let mut oldest_in_batch: Option<&str> = None;
                for p in &batch.pages {
                    if let Some(ts) = p.last_edited_time.as_deref().filter(|s| !s.is_empty()) {
                        if oldest_in_batch.map_or(true, |cur| ts < cur) {
                            oldest_in_batch = Some(ts);
                        }
                    }
                }
                for d in &batch.databases {
                    if let Some(ts) = d.last_edited_time.as_deref().filter(|s| !s.is_empty()) {
                        if oldest_in_batch.map_or(true, |cur| ts < cur) {
                            oldest_in_batch = Some(ts);
                        }
                    }
                }
                oldest_in_batch.is_some_and(|oldest| oldest < wm)
            } else {
                false
            };

            all_pages.extend(batch.pages);
            all_dbs.extend(batch.databases);
            on_batch(batch_num, all_pages.len(), all_dbs.len());

            if hit_watermark
                || !batch.has_more
                || batch.next_cursor.is_none()
                || all_pages.len() + all_dbs.len() >= limit
            {
                return Ok(SearchResponse {
                    pages: all_pages,
                    databases: all_dbs,
                    has_more: !hit_watermark && batch.has_more,
                    next_cursor: if hit_watermark { None } else { batch.next_cursor },
                });
            }
            cursor = batch.next_cursor;
        }
    }

    /// `GET /v1/pages/{page_id}`
    pub async fn get_page(&self, page_id: &str) -> Result<NotionPage> {
        let id = normalize_notion_id(page_id);
        let workspace_id = self.auth.active_workspace_id().await;
        let val = self
            .request_json(Method::GET, &format!("/pages/{id}"), None)
            .await?;
        parse_notion_page(&val, &workspace_id)
    }

    /// `POST /v1/pages`
    pub async fn create_page(&self, payload: Value) -> Result<NotionPage> {
        let workspace_id = self.auth.active_workspace_id().await;
        let val = self
            .request_json(Method::POST, "/pages", Some(payload))
            .await?;
        parse_notion_page(&val, &workspace_id)
    }

    /// `PATCH /v1/pages/{page_id}`
    pub async fn update_page(&self, page_id: &str, payload: Value) -> Result<NotionPage> {
        let id = normalize_notion_id(page_id);
        let workspace_id = self.auth.active_workspace_id().await;
        let val = self
            .request_json(Method::PATCH, &format!("/pages/{id}"), Some(payload))
            .await?;
        parse_notion_page(&val, &workspace_id)
    }

    /// Archives or unarchives a page via `PATCH /v1/pages/{page_id}`.
    pub async fn set_page_archived(&self, page_id: &str, archived: bool) -> Result<NotionPage> {
        self.update_page(page_id, json!({ "archived": archived }))
            .await
    }

    /// `GET /v1/blocks/{block_id}`
    pub async fn get_block(&self, block_id: &str, page_id: &str) -> Result<NotionBlock> {
        let val = self.get_block_json(block_id).await?;
        Ok(parse_notion_block(&val, page_id, None))
    }

    pub(crate) async fn get_block_json(&self, block_id: &str) -> Result<Value> {
        let id = normalize_notion_id(block_id);
        self.request_json(Method::GET, &format!("/blocks/{id}"), None).await
    }

    /// `GET /v1/blocks/{block_id}` returning the block's raw `(parent_type, parent_id)`.
    pub async fn get_block_parent_raw(&self, block_id: &str) -> Result<(String, Option<String>)> {
        let id = normalize_notion_id(block_id);
        let val = self
            .request_json(Method::GET, &format!("/blocks/{id}"), None)
            .await?;
        Ok(crate::parser::parse_parent(val.get("parent")))
    }

    /// `GET /v1/blocks/{block_id}/children` (single level, full pagination)
    pub async fn list_block_children(
        &self,
        block_id: &str,
        page_id: &str,
        parent_block_id: Option<&str>,
    ) -> Result<Vec<NotionBlock>> {
        let id = normalize_notion_id(block_id);
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..50 {
            let path = match cursor.as_deref() {
                Some(c) => format!(
                    "/blocks/{id}/children?page_size=100&start_cursor={}",
                    urlencoding::encode(c)
                ),
                None => format!("/blocks/{id}/children?page_size=100"),
            };
            let val = self.request_json(Method::GET, &path, None).await?;
            let results = val.get("results").and_then(Value::as_array)
                .context("Notion block children response is missing results")?;
            for item in results {
                out.push(parse_notion_block(item, page_id, parent_block_id));
            }
            if val.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
                let next = val
                    .get("next_cursor")
                    .and_then(Value::as_str)
                    .filter(|cursor| !cursor.is_empty())
                    .context("Notion block children response is missing its next cursor")?;
                if cursor.as_deref() == Some(next) {
                    bail!("Notion block children response repeated its pagination cursor");
                }
                cursor = Some(next.to_owned());
            } else {
                return Ok(out);
            }
        }
        bail!("Notion block children pagination did not complete")
    }

    /// Recursively fetches a block tree with depth cap & cursor pagination.
    pub async fn fetch_block_tree(
        &self,
        root_block_id: &str,
        max_depth: Option<usize>,
    ) -> Result<Vec<NotionBlock>> {
        let page_id = normalize_notion_id(root_block_id);
        let limit = max_depth.unwrap_or(DEFAULT_BLOCK_TREE_DEPTH);
        self.fetch_block_tree_rec(&page_id, &page_id, None, limit)
            .await
    }

    /// One page of a block's direct children (each with its subtree), following Notion cursors up to `limit`.
    pub async fn fetch_block_tree_page(
        &self,
        root_block_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NotionBlock>, bool, Option<String>)> {
        if limit == 0 {
            bail!("Invalid limit 0: must be at least 1");
        }
        let page_id = normalize_notion_id(root_block_id);
        let mut out = Vec::new();
        let mut cur: Option<String> = cursor.map(str::to_owned);
        let mut last_has_more = false;
        let mut last_next_cursor: Option<String> = None;

        loop {
            let remaining = limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(100);
            let path = match cur.as_deref() {
                Some(c) => format!(
                    "/blocks/{page_id}/children?page_size={page_size}&start_cursor={}",
                    urlencoding::encode(c)
                ),
                None => format!("/blocks/{page_id}/children?page_size={page_size}"),
            };
            let val = self.request_json(Method::GET, &path, None).await?;
            let results = val
                .get("results")
                .and_then(Value::as_array)
                .context("Notion block children response is missing results")?;
            for item in results {
                out.push(parse_notion_block(item, &page_id, None));
            }
            let has_more = val
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if has_more {
                let next = val
                    .get("next_cursor")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .context("Notion block children response is missing its next cursor")?;
                if cur.as_deref() == Some(next) {
                    bail!("Notion block children response repeated its pagination cursor");
                }
                last_has_more = true;
                last_next_cursor = Some(next.to_owned());
                if out.len() >= limit {
                    break;
                }
                cur = Some(next.to_owned());
            } else {
                last_has_more = false;
                last_next_cursor = None;
                break;
            }
        }
        self.hydrate_children(&mut out, &page_id, DEFAULT_BLOCK_TREE_DEPTH)
            .await?;
        Ok((out, last_has_more, last_next_cursor))
    }

    fn hydrate_children<'a>(
        &'a self,
        blocks: &'a mut Vec<NotionBlock>,
        page_id: &'a str,
        remaining_depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if remaining_depth == 0 {
                return Ok(());
            }
            // Collect indices of child blocks to recurse into and fetch in bounded parallel chunks of 3
            // (respects Notion's 3 req/s rate limit while cutting nested page latency ~3x).
            let recurse_indices: Vec<usize> = blocks
                .iter()
                .enumerate()
                .filter_map(|(i, blk)| {
                    let should_recurse = blk.has_children
                        && blk.children.is_empty()
                        && !matches!(
                            blk.block_type.as_str(),
                            "child_page" | "child_database" | "unsupported"
                        );
                    should_recurse.then_some(i)
                })
                .collect();

            for chunk in recurse_indices.chunks(3) {
                let futs: Vec<_> = chunk
                    .iter()
                    .map(|&idx| {
                        let bid = blocks[idx].id.clone();
                        async move {
                            let res = self
                                .fetch_block_tree_rec(&bid, page_id, Some(&bid), remaining_depth - 1)
                                .await;
                            (idx, res)
                        }
                    })
                    .collect();
                for (idx, res) in futures::future::join_all(futs).await {
                    blocks[idx].children = res?;
                }
            }
            Ok(())
        })
    }

    fn fetch_block_tree_rec<'a>(
        &'a self,
        block_id: &'a str,
        page_id: &'a str,
        parent_block_id: Option<&'a str>,
        remaining_depth: usize,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<NotionBlock>>> + Send + 'a>> {
        Box::pin(async move {
            let mut blocks = self
                .list_block_children(block_id, page_id, parent_block_id)
                .await?;
            if remaining_depth > 0 {
                self.hydrate_children(&mut blocks, page_id, remaining_depth)
                    .await?;
            }
            Ok(blocks)
        })
    }

    /// Fetches a complete `NotionPageDetail` (page metadata + recursive blocks + comments) in parallel.
    pub async fn fetch_page_detail(&self, page_id: &str) -> Result<NotionPageDetail> {
        let id = normalize_notion_id(page_id);
        let (page_res, blocks_res, comments_res) = tokio::join!(
            self.get_page(&id),
            self.fetch_block_tree(&id, None),
            self.list_comments(&id),
        );
        let mut page = page_res?;
        let blocks = blocks_res?;
        let (comments, comments_error) = match comments_res {
            Ok(comments) => (comments, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        let plain_text = blocks_to_plain_text(&blocks);
        let markdown = blocks_to_markdown(&blocks);
        let media = collect_media_refs(&blocks);
        page.plain_text = plain_text;
        Ok(NotionPageDetail {
            page,
            blocks,
            comments,
            comments_error,
            markdown,
            media,
        })
    }

    /// Fetches blocks & comments in parallel for an already-fetched `NotionPage`.
    pub async fn fetch_page_detail_with_page(&self, mut page: NotionPage) -> Result<NotionPageDetail> {
        let id = normalize_notion_id(&page.id);
        let (blocks_res, comments_res) = tokio::join!(
            self.fetch_block_tree(&id, None),
            self.list_comments(&id),
        );
        let blocks = blocks_res?;
        let (comments, comments_error) = match comments_res {
            Ok(comments) => (comments, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
        page.plain_text = blocks_to_plain_text(&blocks);
        let markdown = blocks_to_markdown(&blocks);
        let media = collect_media_refs(&blocks);
        Ok(NotionPageDetail {
            page,
            blocks,
            comments,
            comments_error,
            markdown,
            media,
        })
    }

    /// Page metadata and block tree only; never calls /v1/comments.
    pub async fn fetch_page_body(
        &self,
        page_id: &str,
        known_page: Option<NotionPage>,
    ) -> Result<(NotionPage, Vec<NotionBlock>)> {
        let id = normalize_notion_id(page_id);
        let (mut page, blocks) = match known_page {
            Some(page) => {
                let blocks = self.fetch_block_tree(&id, None).await?;
                (page, blocks)
            }
            None => {
                let (page_res, blocks_res) = tokio::join!(
                    self.get_page(&id),
                    self.fetch_block_tree(&id, None),
                );
                (page_res?, blocks_res?)
            }
        };
        page.plain_text = blocks_to_plain_text(&blocks);
        Ok((page, blocks))
    }

    /// `PATCH /v1/blocks/{block_id}/children` (automatically chunks by 100 blocks per Notion API limits).
    pub async fn append_block_children(
        &self,
        block_id: &str,
        children: Vec<Value>,
    ) -> Result<Vec<NotionBlock>> {
        self.append_block_children_after(block_id, None, children)
            .await
    }

    /// `PATCH /v1/blocks/{block_id}/children` with an optional `"after"` anchor block ID
    /// (automatically chunks by 100 blocks per Notion API limits, chaining `"after"` across chunks).
    pub async fn append_block_children_after(
        &self,
        block_id: &str,
        after: Option<&str>,
        children: Vec<Value>,
    ) -> Result<Vec<NotionBlock>> {
        self.insert_block_children(block_id, after, false, children).await
    }

    pub(crate) async fn insert_block_children(
        &self,
        block_id: &str,
        after: Option<&str>,
        at_start: bool,
        children: Vec<Value>,
    ) -> Result<Vec<NotionBlock>> {
        let id = normalize_notion_id(block_id);
        let mut created = Vec::new();
        let mut cursor_after = after
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalize_notion_id);
        for chunk in children.chunks(MAX_APPEND_BATCH_SIZE) {
            let mut body = serde_json::Map::new();
            body.insert("children".into(), json!(chunk));
            if let Some(ref after_id) = cursor_after {
                body.insert("after".into(), json!(after_id));
            }
            // Only the newer block API can insert before the first sibling.
            // Keep the existing API version and `after` for all other placements.
            let version = if at_start && created.is_empty() {
                body.insert("position".into(), json!({ "type": "start" }));
                "2026-03-11"
            } else {
                NOTION_API_VERSION
            };
            let val = self
                .request_json_version(
                    Method::PATCH,
                    &format!("/blocks/{id}/children"),
                    Some(Value::Object(body)),
                    version,
                )
                .await?;
            let results = val.get("results").and_then(Value::as_array)
                .filter(|results| results.len() == chunk.len())
                .context("Notion did not confirm every appended block")?;
            for item in results {
                let parsed = parse_notion_block(item, &id, None);
                if parsed.id.is_empty() || created.iter().any(|block: &NotionBlock| block.id == parsed.id) {
                    bail!("Notion did not return a distinct ID for every appended block");
                }
                created.push(parsed);
            }
            cursor_after = created.last().map(|block| block.id.clone());
        }
        Ok(created)
    }

    /// `PATCH /v1/blocks/{block_id}`
    pub async fn update_block(
        &self,
        block_id: &str,
        page_id: &str,
        payload: Value,
    ) -> Result<NotionBlock> {
        let id = normalize_notion_id(block_id);
        let val = self
            .request_json(Method::PATCH, &format!("/blocks/{id}"), Some(payload))
            .await?;
        Ok(parse_notion_block(&val, page_id, None))
    }

    /// Toggles a `to_do` block's `checked` field via `PATCH /v1/blocks/{block_id}`.
    pub async fn set_todo_checked(
        &self,
        block_id: &str,
        page_id: &str,
        checked: bool,
    ) -> Result<NotionBlock> {
        self.update_block(block_id, page_id, json!({ "to_do": { "checked": checked } }))
            .await
    }

    /// `DELETE /v1/blocks/{block_id}`
    pub async fn delete_block(&self, block_id: &str) -> Result<Value> {
        let id = normalize_notion_id(block_id);
        self.request_json(Method::DELETE, &format!("/blocks/{id}"), None)
            .await
    }

    /// `GET /v1/databases/{database_id}`
    pub async fn get_database(&self, database_id: &str) -> Result<NotionDatabase> {
        let id = normalize_notion_id(database_id);
        let workspace_id = self.auth.active_workspace_id().await;
        let val = self
            .request_json(Method::GET, &format!("/databases/{id}"), None)
            .await?;
        parse_notion_database(&val, &workspace_id)
    }

    /// `POST /v1/databases/{database_id}/query` (single page)
    pub async fn query_database(
        &self,
        database_id: &str,
        filter: Option<Value>,
        sorts: Option<Value>,
        page_size: usize,
        start_cursor: Option<&str>,
    ) -> Result<(Vec<NotionPage>, bool, Option<String>)> {
        let id = normalize_notion_id(database_id);
        let workspace_id = self.auth.active_workspace_id().await;
        let mut body = serde_json::Map::new();
        if let Some(f) = filter {
            body.insert("filter".into(), f);
        }
        if let Some(s) = sorts {
            body.insert("sorts".into(), s);
        }
        body.insert("page_size".into(), json!(page_size.clamp(1, 100)));
        if let Some(c) = start_cursor {
            body.insert("start_cursor".into(), json!(c));
        }
        let val = self
            .request_json(
                Method::POST,
                &format!("/databases/{id}/query"),
                Some(Value::Object(body)),
            )
            .await?;

        let mut pages = Vec::new();
        if let Some(results) = val.get("results").and_then(Value::as_array) {
            for item in results {
                if let Ok(p) = parse_notion_page(item, &workspace_id) {
                    pages.push(p);
                }
            }
        }
        let has_more = val
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let next_cursor = val
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok((pages, has_more, next_cursor))
    }

    /// Paginated `POST /v1/databases/{database_id}/query`
    pub async fn query_database_all(
        &self,
        database_id: &str,
        filter: Option<Value>,
        sorts: Option<Value>,
        max_rows: usize,
    ) -> Result<Vec<NotionPage>> {
        let (rows, _, _) = self
            .query_database_all_with_meta(database_id, filter, sorts, max_rows, None)
            .await?;
        Ok(rows)
    }

    /// Paginated `POST /v1/databases/{database_id}/query` returning `(rows, has_more, next_cursor)`.
    pub async fn query_database_all_with_meta(
        &self,
        database_id: &str,
        filter: Option<Value>,
        sorts: Option<Value>,
        max_rows: usize,
        start_cursor: Option<&str>,
    ) -> Result<(Vec<NotionPage>, bool, Option<String>)> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = start_cursor.map(str::to_owned);
        let limit = max_rows.max(1);
        let mut last_has_more = false;
        let mut last_next_cursor: Option<String> = None;
        loop {
            let remaining = limit.saturating_sub(all.len());
            if remaining == 0 {
                break;
            }
            let (rows, has_more, next_cursor) = self
                .query_database(
                    database_id,
                    filter.clone(),
                    sorts.clone(),
                    100.min(remaining),
                    cursor.as_deref(),
                )
                .await?;
            all.extend(rows);
            if !has_more || next_cursor.is_none() {
                last_has_more = has_more;
                last_next_cursor = if has_more { next_cursor } else { None };
                break;
            }
            if all.len() >= limit {
                last_has_more = true;
                last_next_cursor = next_cursor;
                break;
            }
            cursor = next_cursor;
        }
        Ok((all, last_has_more, last_next_cursor))
    }

    /// `POST /v1/databases`
    pub async fn create_database(&self, payload: Value) -> Result<NotionDatabase> {
        let workspace_id = self.auth.active_workspace_id().await;
        let val = self
            .request_json(Method::POST, "/databases", Some(payload))
            .await?;
        parse_notion_database(&val, &workspace_id)
    }

    /// `GET /v1/comments?block_id={id}` paginated up to `limit` starting at `cursor`.
    pub async fn list_comments_page(
        &self,
        block_id: &str,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<(Vec<NotionComment>, bool, Option<String>)> {
        if limit == 0 {
            bail!("Invalid limit 0: must be at least 1");
        }
        let id = normalize_notion_id(block_id);
        let mut out = Vec::new();
        let mut cur: Option<String> = cursor.map(str::to_owned);
        let mut last_has_more = false;
        let mut last_next_cursor: Option<String> = None;

        loop {
            let remaining = limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            let page_size = remaining.min(100);
            let path = match cur.as_deref() {
                Some(c) => format!(
                    "/comments?block_id={}&page_size={page_size}&start_cursor={}",
                    urlencoding::encode(&id),
                    urlencoding::encode(c)
                ),
                None => format!(
                    "/comments?block_id={}&page_size={page_size}",
                    urlencoding::encode(&id)
                ),
            };
            let val = self.request_json(Method::GET, &path, None).await?;
            if let Some(results) = val.get("results").and_then(Value::as_array) {
                for item in results {
                    out.push(parse_notion_comment(item, &id));
                }
            }
            let has_more = val
                .get("has_more")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if has_more {
                let next = val
                    .get("next_cursor")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .context("Notion comments response is missing its next cursor")?;
                if cur.as_deref() == Some(next) {
                    bail!("Notion comments response repeated its pagination cursor");
                }
                last_has_more = true;
                last_next_cursor = Some(next.to_owned());
                if out.len() >= limit {
                    break;
                }
                cur = Some(next.to_owned());
            } else {
                last_has_more = false;
                last_next_cursor = None;
                break;
            }
        }
        Ok((out, last_has_more, last_next_cursor))
    }

    /// `GET /v1/comments?block_id={id}` (for page detail: up to `DETAIL_COMMENTS_LIMIT + 1`).
    pub async fn list_comments(&self, block_id: &str) -> Result<Vec<NotionComment>> {
        let (comments, _, _) = self
            .list_comments_page(block_id, DETAIL_COMMENTS_LIMIT + 1, None)
            .await?;
        Ok(comments)
    }

    /// `POST /v1/comments` (on a page or existing discussion thread)
    pub async fn add_comment(
        &self,
        page_id: Option<&str>,
        discussion_id: Option<&str>,
        text: &str,
    ) -> Result<NotionComment> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            bail!("Comment text cannot be empty");
        }
        let mut payload = serde_json::Map::new();
        if let Some(disc) = discussion_id.map(str::trim).filter(|s| !s.is_empty()) {
            payload.insert("discussion_id".into(), json!(disc));
        } else if let Some(pid) = page_id.map(str::trim).filter(|s| !s.is_empty()) {
            payload.insert("parent".into(), json!({ "page_id": normalize_notion_id(pid) }));
        } else {
            bail!("Either `page_id` or `discussion_id` is required to add a comment");
        }
        payload.insert(
            "rich_text".into(),
            json!([{ "type": "text", "text": { "content": trimmed } }]),
        );
        let val = self
            .request_json(Method::POST, "/comments", Some(Value::Object(payload)))
            .await?;
        let pid = page_id.map(normalize_notion_id).unwrap_or_default();
        Ok(parse_notion_comment(&val, &pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_retry_after_preserves_values_over_120s_and_http_date() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-20T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        assert_eq!(
            NotionClient::parse_retry_after_header(Some("180"), now),
            Duration::from_secs(180)
        );
        assert_eq!(
            NotionClient::parse_retry_after_header(Some("360.5"), now),
            Duration::from_secs_f64(360.5)
        );
        assert_eq!(
            NotionClient::parse_retry_after_header(Some("0"), now),
            Duration::from_millis(50)
        );
        assert_eq!(
            NotionClient::parse_retry_after_header(Some("Fri, 20 Nov 2026 10:04:00 GMT"), now),
            Duration::from_secs(240)
        );
        assert_eq!(
            NotionClient::parse_retry_after_header(None, now),
            Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn test_rate_limit_cooldown_honors_over_120s_and_extension_while_sleeping() {
        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_test_cooldown");
        let client = NotionClient::new(&cfg, auth).unwrap();

        // Deterministic virtual clock anchored at an Instant (zero wall-clock sleep).
        let origin = Instant::now();
        let elapsed = Arc::new(Mutex::new(Duration::ZERO));
        let recorded_sleeps = Arc::new(Mutex::new(Vec::<Duration>::new()));

        // Initial 10s cooldown recorded at t=0.
        client.record_rate_limit_wait_at(origin, Duration::from_secs(10));

        let elapsed_now = Arc::clone(&elapsed);
        let elapsed_sleep = Arc::clone(&elapsed);
        let sleeps_cap = Arc::clone(&recorded_sleeps);
        let client_ext = client.clone();

        client
            .wait_for_rate_limit_cooldown_with(
                move || origin + *elapsed_now.lock().unwrap(),
                move |dur| {
                    let elapsed_sleep = Arc::clone(&elapsed_sleep);
                    let sleeps_cap = Arc::clone(&sleeps_cap);
                    let client_ext = client_ext.clone();
                    async move {
                        let call_idx = {
                            let mut s = sleeps_cap.lock().unwrap();
                            s.push(dur);
                            s.len()
                        };
                        if call_idx == 1 {
                            // Mid-sleep (t=5s), another request receives Retry-After: 240s (>120s!)
                            // extending cooldown to t = 5s + 240s = 245s.
                            client_ext.record_rate_limit_wait_at(
                                origin + Duration::from_secs(5),
                                Duration::from_secs(240),
                            );
                        }
                        *elapsed_sleep.lock().unwrap() += dur;
                    }
                },
            )
            .await;

        let sleeps = recorded_sleeps.lock().unwrap().clone();
        assert_eq!(
            sleeps,
            vec![Duration::from_secs(10), Duration::from_secs(235)],
            "waiter must re-check cooldown after waking and honor full >120s extension"
        );
        assert_eq!(*elapsed.lock().unwrap(), Duration::from_secs(245));
    }

    #[tokio::test]
    async fn test_rate_limit_cooldown_real_concurrent_extension_while_sleeping() {
        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_test_cooldown_live");
        let client = NotionClient::new(&cfg, auth).unwrap();

        let start = Instant::now();
        client.record_rate_limit_wait(Duration::from_millis(80));
        let waiter_client = client.clone();
        let waiter = tokio::spawn(async move {
            waiter_client.wait_for_rate_limit_cooldown().await;
            Instant::now()
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        // Extend cooldown while waiter is mid-sleep
        client.record_rate_limit_wait(Duration::from_millis(160));

        let finished = waiter.await.unwrap();
        assert!(
            finished.duration_since(start) >= Duration::from_millis(170),
            "concurrent waiter must honor cooldown extended while sleeping (elapsed={:?})",
            finished.duration_since(start)
        );
    }
}
