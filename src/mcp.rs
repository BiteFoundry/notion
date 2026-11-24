//! Standalone Notion stdio MCP (Model Context Protocol, JSON-RPC 2.0) server (`notion mcp`).
//!
//! Exposes Notion page, block, database, user, and workspace tools:
//! `whoami`, `list_workspaces`, `list_users`, `search_pages`, `get_page`, `get_page_blocks`,
//! `create_page`, `update_page`, `append_blocks`, `update_block`, `toggle_todo`, `archive_page`,
//! `list_databases`, `query_database`, `create_database_item`, `update_database_item`,
//! `list_comments`, `add_comment`, `download_media`, `sync_now`.

use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{
    auth::AuthManager,
    cli,
    config::AppConfig,
    notion::{media, NotionClient},
    storage::Storage,
    sync,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    client: NotionClient,
    storage: Storage,
) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {err}") }
                });
                writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(resp) = handle_request_value(&config, &auth, &client, &storage, req).await {
            writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Handles a single JSON-RPC 2.0 MCP message and returns an optional JSON response (`None` for notifications).
pub async fn handle_request_value(
    config: &AppConfig,
    auth: &AuthManager,
    client: &NotionClient,
    storage: &Storage,
    req: Value,
) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    if method == "notifications/initialized" || method.starts_with("notifications/") {
        return None;
    }

    let id = id.unwrap_or(Value::Null);
    match method {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "notion-cli",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        })),
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": tool_definitions() }
        })),
        "tools/call" => {
            let tool_name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match dispatch_tool(config, auth, client, storage, tool_name, &args).await {
                Ok(result_val) => {
                    let text = serde_json::to_string(&result_val)
                        .unwrap_or_else(|_| result_val.to_string());
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": text }],
                            "structuredContent": result_val,
                            "isError": false,
                        }
                    }))
                }
                Err(err) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("Error: {err}") }],
                        "isError": true,
                    }
                })),
            }
        }
        other => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {other}")
            }
        })),
    }
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "whoami",
            "description": "Return the currently authenticated Notion user/bot and active workspace metadata.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_workspaces",
            "description": "List all configured Notion workspace profiles and the currently active workspace.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_users",
            "description": "List Notion workspace users with their IDs, names, types, and emails. Cost: reads every user on each call, one request per 100 users.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "default": 50, "description": "Maximum users to return per page (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from a previous reply" }
                }
            }
        }),
        json!({
            "name": "search_pages",
            "description": "Search Notion pages and databases using local SQLite FTS5 (with Korean <3 char LIKE fallback) or remote Notion Search API. Cost: no network request when the synced local cache has hits; remote:true, or an empty or unsynced cache, calls Notion search.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query string" },
                    "remote": { "type": "boolean", "description": "Query live Notion POST /v1/search instead of local SQLite cache" },
                    "offline": { "type": "boolean", "description": "Read local SQLite cache only" },
                    "limit": { "type": "integer", "default": 30, "description": "Maximum search results to return (default 30)." }
                }
            }
        }),
        json!({
            "name": "get_page",
            "description": "Fetch a Notion page. Compact by default (never requests comments) with id, title, Markdown, omitted counts/properties, and a hint. Pass fields:[...] for specific standard fields or dynamic page properties (case-insensitive); skips comments unless \"comments\" is requested. Pass compact:false for full detail (up to 2000 comments). Cost: 1 page request; blocks on change/cache miss/refresh; comments only for compact:false or fields containing \"comments\".",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page UUID, Notion URL, or title" },
                    "refresh": { "type": "boolean", "description": "Force refetch of blocks and comments even if page timestamp did not change" },
                    "compact": { "type": "boolean", "default": true, "description": "false returns the full page detail" },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Select specific standard fields (properties, blocks, comments, media, url, parent, created_time, last_edited_time, created_by, last_edited_by, body_markdown, etc.) or dynamic property names (case-insensitive, e.g. \"Status\" or \"properties.Status\"). Skips comments unless \"comments\" is listed."
                    },
                    "offline": { "type": "boolean", "description": "Read from local SQLite cache only without network requests." }
                }
            }
        }),
        json!({
            "name": "get_page_blocks",
            "description": "List the recursive block tree for a Notion page or block ID. Offline reads report a cache miss when no children have been fetched. Returns has_more/next_cursor and incomplete_blocks. Cost: one request per 100 blocks plus one per block with children; use limit and cursor on long pages. offline:true makes none.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page or block ID" },
                    "limit": { "type": "integer", "minimum": 1, "default": 50, "description": "Maximum top-level blocks to return per page (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from a previous reply" },
                    "offline": { "type": "boolean", "description": "Read cached blocks from local SQLite only without network requests." }
                }
            }
        }),
        json!({
            "name": "create_page",
            "description": "Create a new Notion page under a parent page or database (set parent_is_database:true) from Markdown. Use create_database_item for typed database rows with status or custom properties. Cost: one write request per 100 blocks.",
            "inputSchema": {
                "type": "object",
                "required": ["parent_id", "title"],
                "properties": {
                    "parent_id": { "type": "string", "description": "Parent page or database ID/title" },
                    "title": { "type": "string", "description": "New page title" },
                    "icon": { "type": "string", "description": "Optional emoji icon" },
                    "markdown": { "type": "string", "description": "Optional Markdown body converted into Notion blocks" },
                    "parent_is_database": { "type": "boolean", "default": false, "description": "True if parent_id is a database; false for a parent page." }
                }
            }
        }),
        json!({
            "name": "update_page",
            "description": "Edit a Notion page's title and/or emoji icon (omit to keep, null to clear). Use update_database_item for row properties or update_block for body text.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "additionalProperties": false,
                "properties": {
                    "page": { "type": "string", "description": "Page UUID, Notion URL, or title" },
                    "title": { "type": ["string", "null"], "description": "New title, or null for an untitled page" },
                    "icon": { "type": ["string", "null"], "description": "New emoji, or null to remove the icon" }
                },
                "anyOf": [{ "required": ["title"] }, { "required": ["icon"] }]
            }
        }),
        json!({
            "name": "append_blocks",
            "description": "Append Markdown or slash-style blocks (/h1, /todo, /callout, /code, /quote, /toggle) to a Notion page or block. Returns compact {ok, appended, block_ids} by default; pass compact:false for full block objects. Cost: one write request per 100 blocks, then a read of the target's block tree to refresh the cache.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "markdown"],
                "properties": {
                    "page": { "type": "string", "description": "Target page or block ID, URL, or exact title" },
                    "markdown": { "type": "string", "description": "Markdown or slash command text to append" },
                    "compact": { "type": "boolean", "default": true, "description": "false returns the full appended block objects" }
                }
            }
        }),
        json!({
            "name": "update_block",
            "description": "Edit a single block's rich text (and optional to_do checkbox). Use toggle_todo for checkbox-only changes, update_page for page title/icon, or update_database_item for row properties.",
            "inputSchema": {
                "type": "object",
                "required": ["block_id", "text"],
                "properties": {
                    "block_id": { "type": "string", "description": "Target Notion block UUID from get_page_blocks or append_blocks." },
                    "block_type": { "type": "string", "description": "Optional Notion block type (auto-detected if omitted)" },
                    "text": { "type": "string", "description": "Updated inline Markdown text" },
                    "checked": { "type": "boolean", "description": "Optional checked state for to_do blocks" }
                }
            }
        }),
        json!({
            "name": "toggle_todo",
            "description": "Toggle or set only a Notion to_do block's checkbox without editing text.",
            "inputSchema": {
                "type": "object",
                "required": ["block_id"],
                "properties": {
                    "block_id": { "type": "string", "description": "Target Notion to_do block UUID from get_page_blocks." },
                    "checked": { "type": "boolean", "description": "Optional explicit state; flips current state if omitted" }
                }
            }
        }),
        json!({
            "name": "archive_page",
            "description": "Move a Notion page to trash (reversible with archived:false) or restore it. Requires confirm:true.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "confirm"],
                "properties": {
                    "page": { "type": "string", "description": "Page ID, URL, or title" },
                    "archived": { "type": "boolean", "default": true, "description": "True moves page to trash; false restores an archived page." },
                    "confirm": { "type": "boolean", "description": "Must be true to execute archive_page" }
                }
            }
        }),
        json!({
            "name": "list_databases",
            "description": "List accessible Notion databases with schemas. Cost: served from the cache once the workspace has synced; refresh:true or an unsynced cache runs a workspace sync.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "description": "Force workspace sync to refresh cached database list and schemas." },
                    "limit": { "type": "integer", "minimum": 1, "default": 50, "description": "Maximum databases to return per page (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from a previous reply" },
                    "offline": { "type": "boolean", "description": "Read cached databases from local SQLite only without network requests." }
                }
            }
        }),
        json!({
            "name": "query_database",
            "description": "Query rows of a Notion database with optional Notion API JSON filter and sorts. filter and sorts are rejected with offline:true. Cost: one request per 100 rows up to limit; offline:true makes none.",
            "inputSchema": {
                "type": "object",
                "required": ["database"],
                "properties": {
                    "database": { "type": "string", "description": "Database ID, URL, or title" },
                    "filter": { "type": "object", "description": "Optional Notion API filter JSON object" },
                    "sorts": { "type": "array", "description": "Optional Notion API sorts JSON array" },
                    "limit": { "type": "integer", "default": 50, "description": "Maximum database rows to return per page (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from a previous reply" },
                    "offline": { "type": "boolean", "description": "Read cached rows from local SQLite only; rejects filter and sorts." }
                }
            }
        }),
        json!({
            "name": "create_database_item",
            "description": "Add a typed row page to a Notion database with title, optional status, custom properties, and Markdown body. Use create_page for regular subpages.",
            "inputSchema": {
                "type": "object",
                "required": ["database", "title"],
                "properties": {
                    "database": { "type": "string", "description": "Target database UUID, Notion URL, or title from list_databases." },
                    "title": { "type": "string", "description": "Title text for the new database row's title property." },
                    "status": { "type": "string", "description": "Optional Status/Select option name" },
                    "properties": { "type": "object", "description": "Optional raw Notion properties JSON map" },
                    "body": { "type": "string", "description": "Optional Markdown page body" }
                }
            }
        }),
        json!({
            "name": "update_database_item",
            "description": "Edit title, status, or custom properties on an existing database row page. Use update_page for non-database page icon/title or update_block for body text.",
            "inputSchema": {
                "type": "object",
                "required": ["page_id"],
                "properties": {
                    "page_id": { "type": "string", "description": "Row page UUID, Notion URL, or exact title" },
                    "title": { "type": "string", "description": "Optional new title for the row's title property." },
                    "status": { "type": "string", "description": "Optional Status or Select option name matching database schema." },
                    "properties": { "type": "object", "description": "Optional Notion API properties JSON map to update." }
                }
            }
        }),
        json!({
            "name": "list_comments",
            "description": "List comments on a Notion page. With offline:true, comments that were never fetched are a tool error. Cost: one request per 100 comments unless offline:true.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Target page UUID, Notion URL, or title from search_pages." },
                    "limit": { "type": "integer", "minimum": 1, "default": 50, "description": "Maximum comments to return per page (default 50)." },
                    "cursor": { "type": "string", "description": "next_cursor from a previous reply" },
                    "offline": { "type": "boolean", "description": "Read cached comments from local SQLite only; errors if unfetched." }
                }
            }
        }),
        json!({
            "name": "add_comment",
            "description": "Post a new page-level comment on page, or reply to an existing thread by also passing discussion_id from list_comments.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "text"],
                "properties": {
                    "page": { "type": "string", "description": "Target page UUID, Notion URL, or title from search_pages." },
                    "text": { "type": "string", "description": "Plain text body for the comment (must be non-empty)." },
                    "discussion_id": { "type": "string", "description": "Optional discussion_id from list_comments to reply to an existing thread." }
                }
            }
        }),
        json!({
            "name": "download_media",
            "description": "Download a Notion-hosted or external media URL or image/file/pdf block ID to local disk. Cost: downloads the whole file to disk; large files take proportionally longer.",
            "inputSchema": {
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": { "type": "string", "description": "Direct media URL or Notion block ID" },
                    "out_dir": { "type": "string", "description": "Optional destination directory" }
                }
            }
        }),
        json!({
            "name": "sync_now",
            "description": "Synchronize Notion workspace pages and databases into the local SQLite WAL + FTS5 cache immediately. Streams every search batch into the cache; returns fetched_count, complete and full. Cost: one Notion search request per 100 objects. Incremental by default and usually 1-2 requests; full:true rescans the whole workspace and can take minutes on large workspaces.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "full": { "type": "boolean", "default": false, "description": "Rescan the whole workspace, ignoring the incremental watermark. Slow on large workspaces; use only when a page shared long ago is missing." }
                }
            }
        }),
    ]
}

async fn dispatch_tool(
    config: &AppConfig,
    auth: &AuthManager,
    client: &NotionClient,
    storage: &Storage,
    name: &str,
    args: &Value,
) -> Result<Value> {
    let req_str = |key: &str| -> Result<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Missing required string argument `{key}`"))
    };
    let opt_str = |key: &str| -> Option<String> {
        args.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let opt_bool = |key: &str, default: bool| -> bool {
        args.get(key).and_then(Value::as_bool).unwrap_or(default)
    };
    let limit_arg = |default: u64| -> usize {
        args.get("limit").and_then(Value::as_u64).unwrap_or(default) as usize
    };
    let cursor = opt_str("cursor");

    match name {
        "whoami" => cli::op_whoami(config, auth, client).await,
        "list_workspaces" => Ok(cli::op_list_workspaces(config)),
        "list_users" => {
            let list = cli::op_list_users(client, limit_arg(50), cursor.as_deref()).await?;
            Ok(cli::list_envelope("users", &list))
        }
        "search_pages" => {
            let query = req_str("query")?;
            let remote = opt_bool("remote", false);
            let offline = opt_bool("offline", false);
            let limit = limit_arg(30);
            let hits = cli::op_search(client, storage, &query, remote, offline, limit).await?;
            Ok(json!({ "ok": true, "count": hits.len(), "results": hits }))
        }
        "get_page" => {
            let page = req_str("page")?;
            let refresh = opt_bool("refresh", false);
            let offline = opt_bool("offline", false);
            let fields: Option<Vec<String>> = match args.get("fields") {
                None | Some(Value::Null) => None,
                Some(Value::Array(arr)) => {
                    let mut out = Vec::with_capacity(arr.len());
                    for item in arr {
                        let Some(s) = item.as_str() else {
                            bail!("`fields` must be an array of strings");
                        };
                        let trimmed = s.trim();
                        if !trimmed.is_empty() {
                            out.push(trimmed.to_string());
                        }
                    }
                    (!out.is_empty()).then_some(out)
                }
                Some(_) => bail!("`fields` must be an array of strings"),
            };
            if let Some(req_fields) = fields {
                return cli::op_get_page_fields(
                    client,
                    storage,
                    &page,
                    refresh,
                    offline,
                    &req_fields,
                )
                .await;
            }
            if opt_bool("compact", true) {
                return cli::op_get_page_compact(
                    client,
                    storage,
                    &page,
                    refresh,
                    offline,
                    |id| {
                        format!(
                            "Call get_page with {{\"page\":\"{id}\",\"fields\":[\"<property name>\",...]}} to fetch specific properties or fields without pulling full blocks or comments, or {{\"page\":\"{id}\",\"compact\":false}} for full detail."
                        )
                    },
                )
                .await;
            }
            let (detail, comments_has_more) =
                cli::op_get_page_for_output(client, storage, &page, refresh, offline).await?;
            Ok(detail.full_json(comments_has_more))
        }
        "get_page_blocks" => {
            let page = req_str("page")?;
            let offline = opt_bool("offline", false);
            let list = cli::op_list_blocks(
                client,
                storage,
                &page,
                offline,
                limit_arg(50),
                cursor.as_deref(),
            )
            .await?;
            let incomplete = crate::parser::incomplete_block_ids(&list.items);
            let mut val = cli::list_envelope("blocks", &list);
            if let Some(map) = val.as_object_mut() {
                map.insert("incomplete_blocks".into(), json!(incomplete));
            }
            Ok(val)
        }
        "create_page" => {
            let parent_id = req_str("parent_id")?;
            let title = req_str("title")?;
            let icon = opt_str("icon");
            let markdown = opt_str("markdown");
            let parent_is_db = opt_bool("parent_is_database", false);
            let created = cli::op_create_page(
                client,
                storage,
                &parent_id,
                &title,
                icon.as_deref(),
                markdown.as_deref(),
                parent_is_db,
                None,
            )
            .await?;
            Ok(serde_json::to_value(created)?)
        }
        "update_page" => {
            let page = req_str("page")?;
            if let Some(key) = args.as_object().and_then(|map| {
                map.keys().find(|key| !matches!(key.as_str(), "page" | "title" | "icon"))
            }) {
                bail!("Unknown update_page argument `{key}`");
            }
            let field = |key| match args.get(key) {
                None => Ok(None),
                Some(Value::Null) => Ok(Some(None)),
                Some(Value::String(text)) => Ok(Some(Some(text.as_str()))),
                Some(_) => bail!("`{key}` must be a string or null"),
            };
            let title = field("title")?;
            let icon = field("icon")?.map(|value| value.map(str::trim));
            if icon == Some(Some("")) {
                bail!("Provide an emoji for `icon` or use null to clear it");
            }
            let updated = cli::op_update_page_metadata(client, storage, &page, title, icon).await?;
            Ok(serde_json::to_value(updated)?)
        }
        "append_blocks" => {
            let page = req_str("page")?;
            let markdown = req_str("markdown")?;
            let blocks = cli::op_append_blocks(client, storage, &page, &markdown).await?;
            if opt_bool("compact", true) {
                let block_ids: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
                Ok(json!({ "ok": true, "appended": blocks.len(), "block_ids": block_ids }))
            } else {
                Ok(json!({ "ok": true, "appended": blocks.len(), "blocks": blocks }))
            }
        }
        "update_block" => {
            let block_id = req_str("block_id")?;
            let text = args
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("Missing required string argument `text`"))?
                .to_string();
            let block_type = opt_str("block_type");
            let checked = args.get("checked").and_then(Value::as_bool);
            let updated = cli::op_update_block_text(
                client,
                storage,
                &block_id,
                block_type.as_deref(),
                &text,
                checked,
            )
            .await?;
            Ok(serde_json::to_value(updated)?)
        }
        "toggle_todo" => {
            let block_id = req_str("block_id")?;
            let checked = args.get("checked").and_then(Value::as_bool);
            let updated = cli::op_toggle_todo(client, storage, &block_id, checked).await?;
            Ok(serde_json::to_value(updated)?)
        }
        "archive_page" => {
            if !opt_bool("confirm", false) {
                bail!("Confirmation required: pass `confirm: true` to archive_page");
            }
            let page = req_str("page")?;
            let archived = opt_bool("archived", true);
            let updated = cli::op_archive_page(client, storage, &page, archived).await?;
            Ok(serde_json::to_value(updated)?)
        }
        "list_databases" => {
            let refresh = opt_bool("refresh", false);
            let offline = opt_bool("offline", false);
            let list = cli::op_list_databases(
                client,
                storage,
                refresh,
                offline,
                limit_arg(50),
                cursor.as_deref(),
            )
            .await?;
            Ok(cli::list_envelope("databases", &list))
        }
        "query_database" => {
            let database = req_str("database")?;
            let filter = args.get("filter").cloned().filter(|v| !v.is_null());
            let sorts = args.get("sorts").cloned().filter(|v| !v.is_null());
            let limit = limit_arg(50);
            let offline = opt_bool("offline", false);
            let list = cli::op_query_database(
                client,
                storage,
                &database,
                filter,
                sorts,
                limit,
                cursor.as_deref(),
                offline,
            )
            .await?;
            Ok(cli::list_envelope("rows", &list))
        }
        "create_database_item" => {
            let database = req_str("database")?;
            let title = req_str("title")?;
            let status = opt_str("status");
            let properties = args.get("properties").cloned().filter(|v| !v.is_null());
            let body = opt_str("body");
            let created = cli::op_create_database_item(
                client,
                storage,
                &database,
                &title,
                status.as_deref(),
                properties,
                body.as_deref(),
            )
            .await?;
            Ok(serde_json::to_value(created)?)
        }
        "update_database_item" => {
            let page_id = req_str("page_id")?;
            let title = opt_str("title");
            let status = opt_str("status");
            let properties = args.get("properties").cloned().filter(|v| !v.is_null());
            let updated = cli::op_update_database_item(
                client,
                storage,
                &page_id,
                title.as_deref(),
                status.as_deref(),
                properties,
            )
            .await?;
            Ok(serde_json::to_value(updated)?)
        }
        "list_comments" => {
            let page = req_str("page")?;
            let offline = opt_bool("offline", false);
            let list = cli::op_list_comments(
                client,
                storage,
                &page,
                offline,
                limit_arg(50),
                cursor.as_deref(),
            )
            .await?;
            Ok(cli::list_envelope("comments", &list))
        }
        "add_comment" => {
            let page = req_str("page")?;
            let text = req_str("text")?;
            let discussion_id = opt_str("discussion_id");
            let comment =
                cli::op_add_comment(client, storage, &page, discussion_id.as_deref(), &text)
                    .await?;
            Ok(serde_json::to_value(comment)?)
        }
        "download_media" => {
            let target = req_str("target")?;
            let out_dir = opt_str("out_dir").map(PathBuf::from);
            let res = media::download_media_target(config, client, &target, out_dir.as_deref())
                .await?;
            Ok(serde_json::to_value(res)?)
        }
        "sync_now" => {
            let full = opt_bool("full", false);
            let (ws_id, pages, databases, outcome) =
                sync::sync_workspace_tree_with_options(client, storage, full, |_, _, _| {})
                    .await?;
            Ok(json!({
                "ok": true,
                "workspace_id": ws_id,
                "pages_count": pages.len(),
                "databases_count": databases.len(),
                "fetched_count": outcome.fetched,
                "complete": true,
                "full": full,
            }))
        }
        other => bail!("Unknown MCP tool: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_exposes_20_english_tools() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 20);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        for expected in [
            "whoami",
            "list_workspaces",
            "list_users",
            "search_pages",
            "get_page",
            "get_page_blocks",
            "create_page",
            "update_page",
            "append_blocks",
            "update_block",
            "toggle_todo",
            "archive_page",
            "list_databases",
            "query_database",
            "create_database_item",
            "update_database_item",
            "list_comments",
            "add_comment",
            "download_media",
            "sync_now",
        ] {
            assert!(names.contains(&expected), "Missing MCP tool: {expected}");
        }
        for costed in [
            "sync_now",
            "get_page",
            "get_page_blocks",
            "search_pages",
            "query_database",
            "list_databases",
            "list_users",
            "list_comments",
            "create_page",
            "append_blocks",
            "download_media",
        ] {
            let tool = tools.iter().find(|t| t["name"] == costed).unwrap();
            assert!(
                tool["description"].as_str().unwrap().contains("Cost:"),
                "{costed} must state its request cost"
            );
        }
        let sync_now = tools.iter().find(|t| t["name"] == "sync_now").unwrap();
        assert_eq!(sync_now["inputSchema"]["properties"]["full"]["type"], "boolean");
        for tool in &tools {
            let len = tool["description"].as_str().unwrap_or("").chars().count();
            assert!(len < 600, "{} description is {len} characters", tool["name"]);
        }
        for tool in &tools {
            let desc = tool["description"].as_str().unwrap_or("");
            assert!(!desc.is_empty());
            assert!(
                !desc.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)),
                "MCP description must be 100% English: {desc}"
            );
        }
        let get_page = tools.iter().find(|t| t["name"] == "get_page").unwrap();
        assert_eq!(
            get_page["inputSchema"]["properties"]["fields"]["type"],
            "array"
        );
    }

    #[test]
    fn test_mcp_every_tool_property_has_description() {
        let tools = tool_definitions();
        for tool in &tools {
            let tool_name = tool["name"].as_str().unwrap_or("<unknown>");
            let props = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{tool_name} must define inputSchema.properties object"));
            for (prop_name, schema) in props {
                let desc = schema
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                assert!(
                    !desc.is_empty(),
                    "MCP tool `{tool_name}` property `{prop_name}` is missing a non-empty description"
                );
                assert!(
                    !desc.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)),
                    "MCP tool `{tool_name}` property `{prop_name}` description must be 100% English: {desc}"
                );
            }
        }
    }

    #[tokio::test]
    async fn mcp_get_page_compact_omitted_properties_and_selective_fields_projection() {
        use axum::{
            extract::State,
            routing::get,
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::net::TcpListener;

        #[derive(Clone, Default)]
        struct MockState {
            block_reqs: Arc<AtomicUsize>,
            comment_reqs: Arc<AtomicUsize>,
        }

        let page_id = "11111111-1111-1111-1111-111111111111";
        let state = MockState::default();
        let app = Router::new()
            .route(
                "/v1/pages/11111111-1111-1111-1111-111111111111",
                get(|| async {
                    Json(json!({
                        "object": "page",
                        "id": "11111111-1111-1111-1111-111111111111",
                        "url": "https://www.notion.so/11111111111111111111111111111111",
                        "parent": { "type": "database_id", "database_id": "db-stories" },
                        "archived": false,
                        "created_time": "2026-01-01T00:00:00.000Z",
                        "last_edited_time": "2026-11-01T00:00:00.000Z",
                        "created_by": { "object": "user", "id": "u-creator", "name": "Alice" },
                        "last_edited_by": { "object": "user", "id": "u-editor" },
                        "properties": {
                            "Name": { "type": "title", "title": [{ "plain_text": "Checkout Flow" }] },
                            "Status": { "type": "status", "status": { "name": "In Review", "color": "blue" } },
                            "Acceptance Criteria": {
                                "type": "rich_text",
                                "rich_text": [{ "plain_text": "1. Card validates\n2. Receipt emailed" }]
                            },
                            "Empty Field": { "type": "rich_text", "rich_text": [] }
                        }
                    }))
                }),
            )
            .route(
                "/v1/blocks/11111111-1111-1111-1111-111111111111/children",
                get(|State(st): State<MockState>| async move {
                    st.block_reqs.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "object": "list",
                        "results": [
                            {
                                "object": "block",
                                "id": "blk-1",
                                "type": "paragraph",
                                "has_children": false,
                                "paragraph": {
                                    "rich_text": [{ "type": "text", "text": { "content": "Overview text" }, "plain_text": "Overview text" }]
                                }
                            }
                        ],
                        "has_more": false,
                        "next_cursor": Value::Null
                    }))
                }),
            )
            .route(
                "/v1/comments",
                get(|State(st): State<MockState>| async move {
                    st.comment_reqs.fetch_add(1, Ordering::SeqCst);
                    Json(json!({
                        "object": "list",
                        "results": [
                            {
                                "object": "comment",
                                "id": "cmt-1",
                                "parent": { "type": "page_id", "page_id": "11111111-1111-1111-1111-111111111111" },
                                "discussion_id": "disc-1",
                                "created_by": { "object": "user", "id": "u-1", "name": "Reviewer" },
                                "created_time": "2026-11-01T01:00:00.000Z",
                                "rich_text": [{ "plain_text": "Looks good" }]
                            }
                        ],
                        "has_more": false,
                        "next_cursor": Value::Null
                    }))
                }),
            )
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let base_url = format!("http://{addr}/v1");
        let mut cfg = AppConfig::default();
        cfg.api_base_url = base_url.clone();
        cfg.upsert_workspace(
            crate::config::WorkspaceProfile {
                name: "default".into(),
                workspace_id: "default".into(),
                workspace_name: "Test".into(),
                auth_mode: "integration".into(),
                api_base_url: Some(base_url),
                ..Default::default()
            },
            true,
        );
        let auth = AuthManager::from_token(cfg.clone(), "ntn_test_token");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        // 1. Compact get_page surfaces populated properties in omitted.properties & mentions `fields` in hint.
        let compact_resp = handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "get_page", "arguments": { "page": page_id } }
            }),
        )
        .await
        .unwrap();
        assert_eq!(compact_resp["result"]["isError"], false, "{compact_resp}");
        let compact = &compact_resp["result"]["structuredContent"];
        assert_eq!(
            compact["omitted"]["properties"],
            json!([
                { "name": "Acceptance Criteria", "type": "rich_text" },
                { "name": "Name", "type": "title" },
                { "name": "Status", "type": "status" }
            ])
        );
        let hint = compact["hint"].as_str().unwrap();
        assert!(hint.contains("fields"), "hint should mention fields: {hint}");
        assert_eq!(state.comment_reqs.load(Ordering::SeqCst), 0);

        // 2. Selective fields (metadata + dynamic properties) fetches NEITHER blocks NOR comments!
        let blocks_before = state.block_reqs.load(Ordering::SeqCst);
        let proj_resp = handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "get_page",
                    "arguments": {
                        "page": page_id,
                        "fields": ["acceptance criteria", "properties.Status", "url", "created_by", "last_edited_by", "UnknownProp"]
                    }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(proj_resp["result"]["isError"], false, "{proj_resp}");
        let proj = &proj_resp["result"]["structuredContent"];
        assert_eq!(
            proj["properties"]["Acceptance Criteria"]["value"],
            "1. Card validates\n2. Receipt emailed"
        );
        assert_eq!(proj["properties"]["Status"]["value"]["name"], "In Review");
        assert_eq!(
            proj["url"],
            "https://www.notion.so/11111111111111111111111111111111"
        );
        assert_eq!(proj["created_by"], "Alice");
        assert_eq!(proj["last_edited_by"], "u-editor");
        assert_eq!(proj["missing_fields"], json!(["UnknownProp"]));
        assert!(proj.get("comments").is_none());
        assert!(proj.get("blocks").is_none());
        assert_eq!(state.block_reqs.load(Ordering::SeqCst), blocks_before);
        assert_eq!(state.comment_reqs.load(Ordering::SeqCst), 0);

        // 3. Requesting body_markdown still does NOT fetch comments.
        let body_resp = handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": "get_page",
                    "arguments": {
                        "page": page_id,
                        "fields": ["body_markdown", "Status"]
                    }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(body_resp["result"]["isError"], false, "{body_resp}");
        let body = &body_resp["result"]["structuredContent"];
        assert!(body["body_markdown"].as_str().unwrap().contains("Overview text"));
        assert_eq!(body["properties"]["Status"]["value"]["name"], "In Review");
        assert!(body.get("missing_fields").is_none());
        assert_eq!(state.comment_reqs.load(Ordering::SeqCst), 0);

        // 4. Requesting `comments` explicitly DOES fetch comments.
        let cmt_resp = handle_request_value(
            &cfg,
            &auth,
            &client,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "get_page",
                    "arguments": {
                        "page": page_id,
                        "fields": ["comments"]
                    }
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(cmt_resp["result"]["isError"], false, "{cmt_resp}");
        let cmt = &cmt_resp["result"]["structuredContent"];
        assert_eq!(cmt["comments"].as_array().unwrap().len(), 1);
        assert_eq!(cmt["comments"][0]["body"], "Looks good");
        assert_eq!(state.comment_reqs.load(Ordering::SeqCst), 1);

        server.abort();
    }
}
