//! Smart resolver for Notion Page IDs, Database IDs, Notion URLs, and titles
//! using the local SQLite cache with Notion API search fallback.

use std::collections::HashSet;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

use crate::{
    models::{
        coded_with_details, extract_id_from_notion_url, extract_page_id_from_notion_url,
        normalize_notion_id, NotionDatabase,
        NotionPage,
    },
    notion::{NotionClient, SearchFilterObject},
    storage::Storage,
};

pub fn looks_like_notion_uuid(raw: &str) -> bool {
    let trimmed = raw.trim();
    let hex_len = trimmed.chars().filter(|c| c.is_ascii_hexdigit()).count();
    hex_len == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn page_candidate_json(p: &NotionPage) -> Value {
    json!({
        "id": p.id,
        "title": p.title,
        "url": p.url,
        "last_edited_time": p.last_edited_time,
    })
}

fn db_candidate_json(d: &NotionDatabase) -> Value {
    json!({
        "id": d.id,
        "title": d.title,
        "url": d.url,
        "last_edited_time": d.last_edited_time,
    })
}

fn ambiguous_page_err(query: &str, matches: &[NotionPage]) -> anyhow::Error {
    let list = matches
        .iter()
        .take(10)
        .map(|p| {
            format!(
                "{} (edited {})",
                p.id,
                p.last_edited_time.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let candidates: Vec<Value> = matches.iter().take(10).map(page_candidate_json).collect();
    coded_with_details(
        "ambiguous_page",
        format!(
            "Title '{query}' matches {} pages. Pass a page ID or URL: {list}",
            matches.len()
        ),
        json!({ "candidates": candidates }),
    )
}

fn page_not_found_err(query: &str, want_archived: bool, similar: &[NotionPage]) -> anyhow::Error {
    if want_archived {
        return coded_with_details(
            "page_not_found",
            format!(
                "No archived page titled '{query}' in the local cache. Archived pages cannot be looked up by title; pass the page ID or URL."
            ),
            json!({ "candidates": [] }),
        );
    }
    let mut msg = format!("No page titled '{query}'. Pass a page ID or URL.");
    if !similar.is_empty() {
        let preview = similar
            .iter()
            .take(5)
            .map(|p| format!("{} ({})", p.title, p.id))
            .collect::<Vec<_>>()
            .join(", ");
        msg.push_str(&format!(" Similar titles: {preview}"));
    }
    let candidates: Vec<Value> = similar.iter().take(10).map(page_candidate_json).collect();
    coded_with_details("page_not_found", msg, json!({ "candidates": candidates }))
}

fn ambiguous_database_err(query: &str, matches: &[NotionDatabase]) -> anyhow::Error {
    let list = matches
        .iter()
        .take(10)
        .map(|d| {
            format!(
                "{} (edited {})",
                d.id,
                d.last_edited_time.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let candidates: Vec<Value> = matches.iter().take(10).map(db_candidate_json).collect();
    coded_with_details(
        "ambiguous_database",
        format!(
            "Title '{query}' matches {} databases. Pass a database ID or URL: {list}",
            matches.len()
        ),
        json!({ "candidates": candidates }),
    )
}

fn database_not_found_err(query: &str, similar: &[NotionDatabase]) -> anyhow::Error {
    let mut msg = format!("No database titled '{query}'. Pass a database ID or URL.");
    if !similar.is_empty() {
        let preview = similar
            .iter()
            .take(5)
            .map(|d| format!("{} ({})", d.title, d.id))
            .collect::<Vec<_>>()
            .join(", ");
        msg.push_str(&format!(" Similar titles: {preview}"));
    }
    let candidates: Vec<Value> = similar.iter().take(10).map(db_candidate_json).collect();
    coded_with_details("database_not_found", msg, json!({ "candidates": candidates }))
}

struct WriteSearchState<T> {
    exact: Vec<T>,
    seen_ids: HashSet<String>,
    similar_active: Vec<T>,
}

enum WriteSearchOutcome<T> {
    DirectId(String),
    Scanned(WriteSearchState<T>),
}

async fn search_pages_for_write(
    storage: &Storage,
    client: &NotionClient,
    query: &str,
    want_archived: bool,
) -> Result<WriteSearchOutcome<NotionPage>> {
    let lower = query.to_ascii_lowercase();
    let mut cursor: Option<String> = None;
    let mut exact: Vec<NotionPage> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut similar_active: Vec<NotionPage> = Vec::new();

    loop {
        let res = client
            .search(Some(query), SearchFilterObject::Page, 100, cursor.as_deref())
            .await?;
        let _ = storage.upsert_pages(&res.pages);

        for p in res.pages {
            if p.id.eq_ignore_ascii_case(query) {
                return Ok(WriteSearchOutcome::DirectId(p.id));
            }
            if !seen_ids.insert(p.id.clone()) {
                continue;
            }
            if p.title.eq_ignore_ascii_case(query) && p.archived == want_archived {
                exact.push(p);
            } else if !p.archived && p.title.to_ascii_lowercase().contains(&lower) {
                similar_active.push(p);
            }
        }

        if exact.len() >= 2 {
            return Err(ambiguous_page_err(query, &exact));
        }

        if !res.has_more {
            break;
        }
        let next = res
            .next_cursor
            .as_deref()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow!("Notion search response is missing its next cursor"))?;
        if cursor.as_deref() == Some(next) {
            bail!("Notion search response repeated its pagination cursor");
        }
        cursor = Some(next.to_owned());
    }

    Ok(WriteSearchOutcome::Scanned(WriteSearchState {
        exact,
        seen_ids,
        similar_active,
    }))
}

async fn verify_cached_pages_for_write(
    storage: &Storage,
    client: &NotionClient,
    workspace_id: &str,
    query: &str,
    want_archived: bool,
    cached_pages: &[NotionPage],
    state: &mut WriteSearchState<NotionPage>,
) -> Result<()> {
    let lower = query.to_ascii_lowercase();
    for p in cached_pages {
        if state.seen_ids.contains(&p.id) {
            continue;
        }
        if p.title.eq_ignore_ascii_case(query) && p.archived == want_archived {
            match client.get_page(&p.id).await {
                Ok(fresh) => {
                    let _ = storage.upsert_page(&fresh);
                    if fresh.title.eq_ignore_ascii_case(query) && fresh.archived == want_archived {
                        if !state.exact.iter().any(|e| e.id == fresh.id) {
                            state.exact.push(fresh);
                        }
                        if state.exact.len() >= 2 {
                            return Err(ambiguous_page_err(query, &state.exact));
                        }
                    }
                }
                Err(err) if NotionClient::is_authoritative_not_found(&err) => {
                    let _ = storage.mark_page_archived(workspace_id, &p.id);
                }
                Err(err) => return Err(err),
            }
        } else if !want_archived
            && !p.archived
            && p.title.to_ascii_lowercase().contains(&lower)
            && !state.similar_active.iter().any(|s| s.id == p.id)
        {
            state.similar_active.push(p.clone());
        }
    }
    Ok(())
}

/// Resolves a page identifier for write/mutate commands.
/// UUIDs and Notion URLs pass through with zero lookups; title queries require exactly one
/// matching page (active by default, or archived when `want_archived == true`).
pub async fn resolve_page_id_for_write(
    storage: &Storage,
    client: &NotionClient,
    workspace_id: &str,
    query: &str,
    want_archived: bool,
) -> Result<String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("Page identifier cannot be empty"));
    }
    if let Some(id) = extract_page_id_from_notion_url(trimmed) {
        return Ok(id);
    }
    if looks_like_notion_uuid(trimmed) {
        return Ok(normalize_notion_id(trimmed));
    }

    let cached_pages = storage.list_pages(workspace_id, true).unwrap_or_default();
    if let Some(found) = cached_pages
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(trimmed))
    {
        return Ok(found.id.clone());
    }

    let mut state = match search_pages_for_write(storage, client, trimmed, want_archived).await? {
        WriteSearchOutcome::DirectId(id) => return Ok(id),
        WriteSearchOutcome::Scanned(state) => state,
    };

    verify_cached_pages_for_write(
        storage,
        client,
        workspace_id,
        trimmed,
        want_archived,
        &cached_pages,
        &mut state,
    )
    .await?;

    match state.exact.as_slice() {
        [only] => Ok(only.id.clone()),
        [] => Err(page_not_found_err(
            trimmed,
            want_archived,
            &state.similar_active,
        )),
        _ => Err(ambiguous_page_err(trimmed, &state.exact)),
    }
}

async fn search_databases_for_write(
    storage: &Storage,
    client: &NotionClient,
    query: &str,
) -> Result<WriteSearchOutcome<NotionDatabase>> {
    let lower = query.to_ascii_lowercase();
    let mut cursor: Option<String> = None;
    let mut exact: Vec<NotionDatabase> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    let mut similar_active: Vec<NotionDatabase> = Vec::new();

    loop {
        let res = client
            .search(
                Some(query),
                SearchFilterObject::Database,
                100,
                cursor.as_deref(),
            )
            .await?;
        let _ = storage.upsert_databases(&res.databases);

        for d in res.databases {
            if d.id.eq_ignore_ascii_case(query) {
                return Ok(WriteSearchOutcome::DirectId(d.id));
            }
            if !seen_ids.insert(d.id.clone()) {
                continue;
            }
            if d.title.eq_ignore_ascii_case(query) && !d.archived {
                exact.push(d);
            } else if !d.archived && d.title.to_ascii_lowercase().contains(&lower) {
                similar_active.push(d);
            }
        }

        if exact.len() >= 2 {
            return Err(ambiguous_database_err(query, &exact));
        }

        if !res.has_more {
            break;
        }
        let next = res
            .next_cursor
            .as_deref()
            .filter(|c| !c.is_empty())
            .ok_or_else(|| anyhow!("Notion search response is missing its next cursor"))?;
        if cursor.as_deref() == Some(next) {
            bail!("Notion search response repeated its pagination cursor");
        }
        cursor = Some(next.to_owned());
    }

    Ok(WriteSearchOutcome::Scanned(WriteSearchState {
        exact,
        seen_ids,
        similar_active,
    }))
}

async fn verify_cached_databases_for_write(
    storage: &Storage,
    client: &NotionClient,
    workspace_id: &str,
    query: &str,
    cached_dbs: &[NotionDatabase],
    state: &mut WriteSearchState<NotionDatabase>,
) -> Result<()> {
    let lower = query.to_ascii_lowercase();
    for d in cached_dbs {
        if state.seen_ids.contains(&d.id) {
            continue;
        }
        if d.title.eq_ignore_ascii_case(query) && !d.archived {
            match client.get_database(&d.id).await {
                Ok(fresh) => {
                    let _ = storage.upsert_database(&fresh);
                    if fresh.title.eq_ignore_ascii_case(query) && !fresh.archived {
                        if !state.exact.iter().any(|e| e.id == fresh.id) {
                            state.exact.push(fresh);
                        }
                        if state.exact.len() >= 2 {
                            return Err(ambiguous_database_err(query, &state.exact));
                        }
                    }
                }
                Err(err) if NotionClient::is_authoritative_not_found(&err) => {
                    let _ = storage.mark_database_archived(workspace_id, &d.id);
                }
                Err(err) => return Err(err),
            }
        } else if !d.archived
            && d.title.to_ascii_lowercase().contains(&lower)
            && !state.similar_active.iter().any(|s| s.id == d.id)
        {
            state.similar_active.push(d.clone());
        }
    }
    Ok(())
}

/// Resolves a database identifier for write/mutate commands.
/// UUIDs and Notion URLs pass through with zero lookups; title queries require exactly one
/// active database with an exact title match.
pub async fn resolve_database_id_for_write(
    storage: &Storage,
    client: &NotionClient,
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

    let cached_dbs = storage.list_databases(workspace_id).unwrap_or_default();
    if let Some(found) = cached_dbs
        .iter()
        .find(|d| d.id.eq_ignore_ascii_case(trimmed))
    {
        return Ok(found.id.clone());
    }

    let mut state = match search_databases_for_write(storage, client, trimmed).await? {
        WriteSearchOutcome::DirectId(id) => return Ok(id),
        WriteSearchOutcome::Scanned(state) => state,
    };

    verify_cached_databases_for_write(
        storage,
        client,
        workspace_id,
        trimmed,
        &cached_dbs,
        &mut state,
    )
    .await?;

    match state.exact.as_slice() {
        [only] => Ok(only.id.clone()),
        [] => Err(database_not_found_err(trimmed, &state.similar_active)),
        _ => Err(ambiguous_database_err(trimmed, &state.exact)),
    }
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
    if let Some(id) = extract_page_id_from_notion_url(trimmed) {
        return Ok(id);
    }
    if looks_like_notion_uuid(trimmed) {
        return Ok(normalize_notion_id(trimmed));
    }

    // Check exact ID or exact title in local cache (active before archived).
    let mut local_partial: Option<String> = None;
    if let Ok(pages) = storage.list_pages(workspace_id, true) {
        if let Some(found) = match_page_exact(&pages, trimmed) {
            return Ok(found.id.clone());
        }
        if let Some(found) = match_page_partial(&pages, trimmed) {
            local_partial = Some(found.id.clone());
        }
    }

    // Remote search fallback when online client is provided.
    if let Some(api) = client {
        let res = api
            .search(Some(trimmed), SearchFilterObject::Page, 25, None)
            .await?;
        let _ = storage.upsert_pages(&res.pages);
        if let Some(found) = match_page_exact(&res.pages, trimmed) {
            return Ok(found.id.clone());
        }
        if let Some(local_id) = local_partial {
            return Ok(local_id);
        }
        if let Some(found) = match_page_partial(&res.pages, trimmed) {
            return Ok(found.id.clone());
        }
        return Err(page_not_found_err(trimmed, false, &[]));
    }

    if let Some(local_id) = local_partial {
        return Ok(local_id);
    }

    // Offline pass-through preserves "Page '...' not found in local cache" in callers.
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

    let mut local_partial: Option<String> = None;
    if let Ok(dbs) = storage.list_databases(workspace_id) {
        if let Some(found) = match_database_exact(&dbs, trimmed) {
            return Ok(found.id.clone());
        }
        if let Some(found) = match_database_partial(&dbs, trimmed) {
            local_partial = Some(found.id.clone());
        }
    }

    if let Some(api) = client {
        let res = api
            .search(Some(trimmed), SearchFilterObject::Database, 25, None)
            .await?;
        let _ = storage.upsert_databases(&res.databases);
        if let Some(found) = match_database_exact(&res.databases, trimmed) {
            return Ok(found.id.clone());
        }
        if let Some(local_id) = local_partial {
            return Ok(local_id);
        }
        if let Some(found) = match_database_partial(&res.databases, trimmed) {
            return Ok(found.id.clone());
        }
        return Err(database_not_found_err(trimmed, &[]));
    }

    if let Some(local_id) = local_partial {
        return Ok(local_id);
    }

    Ok(trimmed.to_string())
}

fn match_page_exact<'a>(pages: &'a [NotionPage], needle: &str) -> Option<&'a NotionPage> {
    pages
        .iter()
        .find(|p| p.id.eq_ignore_ascii_case(needle))
        .or_else(|| {
            pages
                .iter()
                .find(|p| !p.archived && p.title.eq_ignore_ascii_case(needle))
        })
        .or_else(|| {
            pages
                .iter()
                .find(|p| p.archived && p.title.eq_ignore_ascii_case(needle))
        })
}

fn match_page_partial<'a>(pages: &'a [NotionPage], needle: &str) -> Option<&'a NotionPage> {
    let lower = needle.to_ascii_lowercase();
    pages
        .iter()
        .find(|p| !p.archived && p.title.to_ascii_lowercase().contains(&lower))
        .or_else(|| {
            pages
                .iter()
                .find(|p| p.archived && p.title.to_ascii_lowercase().contains(&lower))
        })
}

fn match_database_exact<'a>(
    dbs: &'a [NotionDatabase],
    needle: &str,
) -> Option<&'a NotionDatabase> {
    dbs.iter()
        .find(|d| d.id.eq_ignore_ascii_case(needle))
        .or_else(|| {
            dbs.iter()
                .find(|d| !d.archived && d.title.eq_ignore_ascii_case(needle))
        })
        .or_else(|| {
            dbs.iter()
                .find(|d| d.archived && d.title.eq_ignore_ascii_case(needle))
        })
}

fn match_database_partial<'a>(
    dbs: &'a [NotionDatabase],
    needle: &str,
) -> Option<&'a NotionDatabase> {
    let lower = needle.to_ascii_lowercase();
    dbs.iter()
        .find(|d| !d.archived && d.title.to_ascii_lowercase().contains(&lower))
        .or_else(|| {
            dbs.iter()
                .find(|d| d.archived && d.title.to_ascii_lowercase().contains(&lower))
        })
}
