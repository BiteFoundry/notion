//! Standalone Notion stdio MCP (Model Context Protocol, JSON-RPC 2.0) server (`notion mcp`).
//!
//! Exposes 18 Notion tools (100% English descriptions):
//! `whoami`, `list_workspaces`, `search_pages`, `get_page`, `get_page_blocks`, `create_page`,
//! `append_blocks`, `update_block`, `toggle_todo`, `archive_page`, `list_databases`,
//! `query_database`, `create_database_item`, `update_database_item`, `list_comments`,
//! `add_comment`, `download_media`, `sync_now`.

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
                    let text = serde_json::to_string_pretty(&result_val)
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
            "name": "search_pages",
            "description": "Search Notion pages and databases using local SQLite FTS5 (with Korean <3 char LIKE fallback) or remote Notion Search API.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query string" },
                    "remote": { "type": "boolean", "description": "Query live Notion POST /v1/search instead of local SQLite cache" },
                    "offline": { "type": "boolean", "description": "Read local SQLite cache only" },
                    "limit": { "type": "integer", "default": 30 }
                }
            }
        }),
        json!({
            "name": "get_page",
            "description": "Fetch a Notion page with properties, recursive block tree, Markdown export, comments, and media references.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page UUID, Notion URL, or title" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "get_page_blocks",
            "description": "List the recursive block tree for a Notion page or block ID.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page or block ID" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "create_page",
            "description": "Create a new Notion page under a parent page or database from Markdown content.",
            "inputSchema": {
                "type": "object",
                "required": ["parent_id", "title"],
                "properties": {
                    "parent_id": { "type": "string", "description": "Parent page or database ID/title" },
                    "title": { "type": "string", "description": "New page title" },
                    "icon": { "type": "string", "description": "Optional emoji icon" },
                    "markdown": { "type": "string", "description": "Optional Markdown body converted into Notion blocks" },
                    "parent_is_database": { "type": "boolean", "default": false }
                }
            }
        }),
        json!({
            "name": "append_blocks",
            "description": "Append Markdown or slash-style blocks (/h1, /todo, /callout, /code, /quote, /toggle) to a Notion page or block.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "markdown"],
                "properties": {
                    "page": { "type": "string", "description": "Target page or block ID/title" },
                    "markdown": { "type": "string", "description": "Markdown or slash command text to append" }
                }
            }
        }),
        json!({
            "name": "update_block",
            "description": "Update an existing Notion block's rich text or to_do checked status.",
            "inputSchema": {
                "type": "object",
                "required": ["block_id", "text"],
                "properties": {
                    "block_id": { "type": "string" },
                    "block_type": { "type": "string", "description": "Optional Notion block type (auto-detected if omitted)" },
                    "text": { "type": "string", "description": "Updated inline Markdown text" },
                    "checked": { "type": "boolean", "description": "Optional checked state for to_do blocks" }
                }
            }
        }),
        json!({
            "name": "toggle_todo",
            "description": "Toggle or explicitly set a Notion to_do block's checkbox state.",
            "inputSchema": {
                "type": "object",
                "required": ["block_id"],
                "properties": {
                    "block_id": { "type": "string" },
                    "checked": { "type": "boolean", "description": "Optional explicit state; flips current state if omitted" }
                }
            }
        }),
        json!({
            "name": "archive_page",
            "description": "Archive (or unarchive) a Notion page. Destructive archive requires confirm=true.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "confirm"],
                "properties": {
                    "page": { "type": "string", "description": "Page ID, URL, or title" },
                    "archived": { "type": "boolean", "default": true },
                    "confirm": { "type": "boolean", "description": "Must be true to execute archive_page" }
                }
            }
        }),
        json!({
            "name": "list_databases",
            "description": "List accessible Notion databases with schemas.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "query_database",
            "description": "Query rows of a Notion database with optional Notion API JSON filter and sorts.",
            "inputSchema": {
                "type": "object",
                "required": ["database"],
                "properties": {
                    "database": { "type": "string", "description": "Database ID, URL, or title" },
                    "filter": { "type": "object", "description": "Optional Notion API filter JSON object" },
                    "sorts": { "type": "array", "description": "Optional Notion API sorts JSON array" },
                    "limit": { "type": "integer", "default": 50 },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "create_database_item",
            "description": "Create a new row page inside a Notion database with title, optional status, custom properties, and Markdown body.",
            "inputSchema": {
                "type": "object",
                "required": ["database", "title"],
                "properties": {
                    "database": { "type": "string" },
                    "title": { "type": "string" },
                    "status": { "type": "string", "description": "Optional Status/Select option name" },
                    "properties": { "type": "object", "description": "Optional raw Notion properties JSON map" },
                    "body": { "type": "string", "description": "Optional Markdown page body" }
                }
            }
        }),
        json!({
            "name": "update_database_item",
            "description": "Update title, status, or arbitrary Notion properties on a database row page.",
            "inputSchema": {
                "type": "object",
                "required": ["page_id"],
                "properties": {
                    "page_id": { "type": "string" },
                    "title": { "type": "string" },
                    "status": { "type": "string" },
                    "properties": { "type": "object" }
                }
            }
        }),
        json!({
            "name": "list_comments",
            "description": "List comments on a Notion page.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "add_comment",
            "description": "Add a comment to a Notion page or reply to an existing discussion thread.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "text"],
                "properties": {
                    "page": { "type": "string" },
                    "text": { "type": "string" },
                    "discussion_id": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "download_media",
            "description": "Download a Notion-hosted or external media URL or image/file/pdf block ID to local disk.",
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
            "description": "Synchronize Notion workspace pages and databases into the local SQLite WAL + FTS5 cache immediately.",
            "inputSchema": { "type": "object", "properties": {} }
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

    match name {
        "whoami" => cli::op_whoami(config, auth, client).await,
        "list_workspaces" => Ok(cli::op_list_workspaces(config)),
        "search_pages" => {
            let query = req_str("query")?;
            let remote = opt_bool("remote", false);
            let offline = opt_bool("offline", false);
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(30) as usize;
            let hits = cli::op_search(client, storage, &query, remote, offline, limit).await?;
            Ok(json!({ "ok": true, "count": hits.len(), "results": hits }))
        }
        "get_page" => {
            let page = req_str("page")?;
            let offline = opt_bool("offline", false);
            let detail = cli::op_get_page(client, storage, &page, offline).await?;
            Ok(serde_json::to_value(detail)?)
        }
        "get_page_blocks" => {
            let page = req_str("page")?;
            let offline = opt_bool("offline", false);
            let blocks = cli::op_list_blocks(client, storage, &page, offline).await?;
            Ok(json!({ "ok": true, "count": blocks.len(), "blocks": blocks }))
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
        "append_blocks" => {
            let page = req_str("page")?;
            let markdown = req_str("markdown")?;
            let blocks = cli::op_append_blocks(client, storage, &page, &markdown).await?;
            Ok(json!({ "ok": true, "appended": blocks.len(), "blocks": blocks }))
        }
        "update_block" => {
            let block_id = req_str("block_id")?;
            let text = req_str("text")?;
            let block_type = opt_str("block_type");
            let checked = args.get("checked").and_then(Value::as_bool);
            let updated =
                cli::op_update_block_text(client, &block_id, block_type.as_deref(), &text, checked)
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
            let dbs = cli::op_list_databases(client, storage, refresh, offline).await?;
            Ok(json!({ "ok": true, "count": dbs.len(), "databases": dbs }))
        }
        "query_database" => {
            let database = req_str("database")?;
            let filter = args.get("filter").cloned().filter(|v| !v.is_null());
            let sorts = args.get("sorts").cloned().filter(|v| !v.is_null());
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
            let offline = opt_bool("offline", false);
            let rows =
                cli::op_query_database(client, storage, &database, filter, sorts, limit, offline)
                    .await?;
            Ok(json!({ "ok": true, "count": rows.len(), "rows": rows }))
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
            let comments = cli::op_list_comments(client, storage, &page, offline).await?;
            Ok(json!({ "ok": true, "count": comments.len(), "comments": comments }))
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
            let (ws_id, pages, databases) = sync::sync_workspace_tree(client, storage).await?;
            Ok(json!({
                "ok": true,
                "workspace_id": ws_id,
                "pages_count": pages.len(),
                "databases_count": databases.len(),
            }))
        }
        other => bail!("Unknown MCP tool: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_exposes_18_english_tools() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 18);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        for expected in [
            "whoami",
            "list_workspaces",
            "search_pages",
            "get_page",
            "get_page_blocks",
            "create_page",
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
        for tool in &tools {
            let desc = tool["description"].as_str().unwrap_or("");
            assert!(!desc.is_empty());
            assert!(
                !desc.chars().any(|c| ('\u{AC00}'..='\u{D7A3}').contains(&c)),
                "MCP description must be 100% English: {desc}"
            );
        }
    }
}
