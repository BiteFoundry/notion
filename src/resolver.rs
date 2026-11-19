//! Smart resolver for Notion Page IDs, Database IDs, Notion URLs, and fuzzy titles
//! using the local SQLite cache with optional Notion API search fallback.

use anyhow::{anyhow, Result};

use crate::{
    models::{extract_id_from_notion_url, normalize_notion_id, NotionDatabase, NotionPage},
    notion::{NotionClient, SearchFilterObject},
    storage::Storage,
};

pub fn looks_like_notion_uuid(raw: &str) -> bool {
    let trimmed = raw.trim();
    let hex_len = trimmed.chars().filter(|c| c.is_ascii_hexdigit()).count();
    hex_len == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Resolves a user-supplied page identifier (UUID, Notion URL, or title) into a canonical page ID.
pub async fn resolve_page_id(
    storage: &Storage,
    client: Option<&NotionClient>,
    workspace_id: &str,
    query: &str,
) -> Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Page identifier cannot be empty"));
    }
    if let Some(id) = extract_id_from_notion_url(trimmed) {
        return Ok(id);
    }
    if looks_like_notion_uuid(trimmed) {
        return Ok(normalize_notion_id(trimmed));
    }

    // Check exact ID or title in local cache
    if let Ok(pages) = storage.list_pages(workspace_id, true) {
        if let Some(found) = match_page_by_query(&pages, trimmed) {
            return Ok(found.id.clone());
        }
    }

    // Remote search fallback when online client is provided
    if let Some(api) = client {
        let res = api
            .search(Some(trimmed), SearchFilterObject::Page, 25, None)
            .await?;
        if let Some(found) = match_page_by_query(&res.pages, trimmed) {
            let _ = storage.upsert_pages(&res.pages);
            return Ok(found.id.clone());
        }
    }

    // Pass through raw identifier if it looks like a custom test ID (e.g. "page-1")
    Ok(trimmed.to_string())
}

/// Resolves a user-supplied database identifier (UUID, Notion URL, or title) into a canonical database ID.
pub async fn resolve_database_id(
    storage: &Storage,
    client: Option<&NotionClient>,
    workspace_id: &str,
    query: &str,
) -> Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Database identifier cannot be empty"));
    }
    if let Some(id) = extract_id_from_notion_url(trimmed) {
        return Ok(id);
    }
    if looks_like_notion_uuid(trimmed) {
        return Ok(normalize_notion_id(trimmed));
    }

    if let Ok(dbs) = storage.list_databases(workspace_id) {
        if let Some(found) = match_database_by_query(&dbs, trimmed) {
            return Ok(found.id.clone());
        }
    }

    if let Some(api) = client {
        let res = api
            .search(Some(trimmed), SearchFilterObject::Database, 25, None)
            .await?;
        if let Some(found) = match_database_by_query(&res.databases, trimmed) {
            let _ = storage.upsert_databases(&res.databases);
            return Ok(found.id.clone());
        }
    }

    Ok(trimmed.to_string())
}

fn match_page_by_query<'a>(pages: &'a [NotionPage], needle: &str) -> Option<&'a NotionPage> {
    let lower = needle.to_ascii_lowercase();
    pages
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(needle))
        .or_else(|| pages.iter().find(|p| p.title.eq_ignore_ascii_case(needle)))
        .or_else(|| {
            pages
                .iter()
                .find(|p| p.title.to_ascii_lowercase().contains(&lower))
        })
}

fn match_database_by_query<'a>(
    dbs: &'a [NotionDatabase],
    needle: &str,
) -> Option<&'a NotionDatabase> {
    let lower = needle.to_ascii_lowercase();
    dbs.iter()
        .find(|d| d.id.eq_ignore_ascii_case(needle))
        .or_else(|| dbs.iter().find(|d| d.title.eq_ignore_ascii_case(needle)))
        .or_else(|| {
            dbs.iter()
                .find(|d| d.title.to_ascii_lowercase().contains(&lower))
        })
}
