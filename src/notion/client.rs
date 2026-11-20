//! Typed async Notion REST API v1 client (`https://api.notion.com/v1`, `Notion-Version: 2022-06-28`).
//!
//! Features:
//! - Configurable base URL (supports mock Axum server for E2E tests)
//! - Automatic retry on HTTP 429 honoring `Retry-After` + transient 502/503/504 backoff
//! - Cursor pagination (`start_cursor` / `has_more`)
//! - Recursive block tree fetching with depth cap
//! - Automatic 100-block batching on `PATCH /v1/blocks/{block_id}/children`

use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{
    header::{AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER},
    Method, StatusCode,
};
use serde_json::{json, Value};

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
const DEFAULT_BLOCK_TREE_DEPTH: usize = 4;
const MAX_APPEND_BATCH_SIZE: usize = 100;

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
        })
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
        let token = self.auth.access_token().await?;
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            path.to_string()
        } else {
            format!("{}/{}", self.api_base_url, path.trim_start_matches('/'))
        };

        let mut attempt = 0usize;
        loop {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header(AUTHORIZATION, format!("Bearer {token}"))
                .header("Notion-Version", NOTION_API_VERSION);
            if let Some(ref payload) = body {
                req = req
                    .header(CONTENT_TYPE, "application/json")
                    .json(payload);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(err) if attempt < MAX_RETRIES && (err.is_timeout() || err.is_connect()) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(250 * (1u64 << attempt.min(4)))).await;
                    continue;
                }
                Err(err) => {
                    return Err(err).with_context(|| format!("Notion API request failed: {method} {url}"));
                }
            };

            let status = resp.status();
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RETRIES {
                let wait_secs = resp
                    .headers()
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(1.0)
                    .clamp(0.05, 30.0);
                attempt += 1;
                tokio::time::sleep(Duration::from_secs_f64(wait_secs)).await;
                continue;
            }

            if matches!(
                status,
                StatusCode::BAD_GATEWAY
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::GATEWAY_TIMEOUT
            ) && attempt < MAX_RETRIES
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
                bail!("Notion API error (HTTP {status}, {code}): {msg}");
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
        let id = normalize_notion_id(block_id);
        let val = self
            .request_json(Method::GET, &format!("/blocks/{id}"), None)
            .await?;
        Ok(parse_notion_block(&val, page_id, None))
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
            if let Some(results) = val.get("results").and_then(Value::as_array) {
                for item in results {
                    out.push(parse_notion_block(item, page_id, parent_block_id));
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
            if remaining_depth == 0 {
                return Ok(blocks);
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
        let comments = comments_res.unwrap_or_default();
        let plain_text = blocks_to_plain_text(&blocks);
        let markdown = blocks_to_markdown(&blocks);
        let media = collect_media_refs(&blocks);
        page.plain_text = plain_text;
        Ok(NotionPageDetail {
            page,
            blocks,
            comments,
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
        let comments = comments_res.unwrap_or_default();
        page.plain_text = blocks_to_plain_text(&blocks);
        let markdown = blocks_to_markdown(&blocks);
        let media = collect_media_refs(&blocks);
        Ok(NotionPageDetail {
            page,
            blocks,
            comments,
            markdown,
            media,
        })
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
            let val = self
                .request_json(
                    Method::PATCH,
                    &format!("/blocks/{id}/children"),
                    Some(Value::Object(body)),
                )
                .await?;
            let mut batch_last_id: Option<String> = None;
            if let Some(results) = val.get("results").and_then(Value::as_array) {
                for item in results {
                    let parsed = parse_notion_block(item, &id, None);
                    if !parsed.id.is_empty() {
                        batch_last_id = Some(parsed.id.clone());
                    }
                    created.push(parsed);
                }
            }
            if batch_last_id.is_some() {
                cursor_after = batch_last_id;
            }
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
        let (rows, _) = self
            .query_database_all_with_meta(database_id, filter, sorts, max_rows)
            .await?;
        Ok(rows)
    }

    /// Paginated `POST /v1/databases/{database_id}/query` returning `(rows, has_more)`.
    /// `has_more` is `true` if Notion reported more pages or results were truncated at `max_rows`.
    pub async fn query_database_all_with_meta(
        &self,
        database_id: &str,
        filter: Option<Value>,
        sorts: Option<Value>,
        max_rows: usize,
    ) -> Result<(Vec<NotionPage>, bool)> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let limit = max_rows.max(1);
        let more_remaining: bool;
        loop {
            let (rows, has_more, next_cursor) = self
                .query_database(
                    database_id,
                    filter.clone(),
                    sorts.clone(),
                    100.min(limit),
                    cursor.as_deref(),
                )
                .await?;
            all.extend(rows);
            if !has_more || next_cursor.is_none() {
                more_remaining = has_more || all.len() > limit;
                break;
            }
            if all.len() >= limit {
                more_remaining = true;
                break;
            }
            cursor = next_cursor;
        }
        all.truncate(limit);
        Ok((all, more_remaining))
    }

    /// `POST /v1/databases`
    pub async fn create_database(&self, payload: Value) -> Result<NotionDatabase> {
        let workspace_id = self.auth.active_workspace_id().await;
        let val = self
            .request_json(Method::POST, "/databases", Some(payload))
            .await?;
        parse_notion_database(&val, &workspace_id)
    }

    /// `GET /v1/comments?block_id={id}` (paginated)
    pub async fn list_comments(&self, block_id: &str) -> Result<Vec<NotionComment>> {
        let id = normalize_notion_id(block_id);
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..20 {
            let path = match cursor.as_deref() {
                Some(c) => format!(
                    "/comments?block_id={}&page_size=100&start_cursor={}",
                    urlencoding::encode(&id),
                    urlencoding::encode(c)
                ),
                None => format!("/comments?block_id={}&page_size=100", urlencoding::encode(&id)),
            };
            let val = self.request_json(Method::GET, &path, None).await?;
            if let Some(results) = val.get("results").and_then(Value::as_array) {
                for item in results {
                    out.push(parse_notion_comment(item, &id));
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
