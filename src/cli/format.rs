//! Shared pagination envelopes, formatting helpers, and terminal output warnings.

use std::{fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{
    models::{coded, NotionBlock, NotionSearchHit},
    notion::SearchResponse,
    parser::incomplete_block_ids,
};

pub struct ListPage<T> {
    pub items: Vec<T>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    /// Set only when the list cannot be continued with next_cursor (partial offline comment cache).
    pub hint: Option<String>,
}

/// Envelope shared by CLI JSON and MCP: ok, count, returned, <key>, has_more, next_cursor, and hint when set.
pub fn list_envelope<T: serde::Serialize>(key: &str, page: &ListPage<T>) -> Value {
    let n = page.items.len();
    let mut map = serde_json::Map::new();
    map.insert("ok".into(), json!(true));
    map.insert("count".into(), json!(n));
    map.insert("returned".into(), json!(n));
    map.insert(
        key.into(),
        serde_json::to_value(&page.items).unwrap_or_else(|_| Value::Array(Vec::new())),
    );
    map.insert("has_more".into(), json!(page.has_more));
    map.insert(
        "next_cursor".into(),
        match &page.next_cursor {
            Some(c) => Value::String(c.clone()),
            None => Value::Null,
        },
    );
    if let Some(hint) = &page.hint {
        map.insert("hint".into(), Value::String(hint.clone()));
    }
    Value::Object(map)
}

pub(super) fn require_limit(limit: usize) -> Result<()> {
    if limit == 0 {
        return Err(coded("invalid_input", "Invalid limit 0: must be at least 1"));
    }
    Ok(())
}

pub(super) fn parse_offset_cursor(cursor: Option<&str>) -> Result<usize> {
    match cursor {
        None => Ok(0),
        Some(raw) => {
            if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
                bail!("Invalid cursor '{raw}': expected the integer offset returned as next_cursor");
            }
            let parsed: u64 = raw.parse().map_err(|_| {
                anyhow::anyhow!(
                    "Invalid cursor '{raw}': expected the integer offset returned as next_cursor"
                )
            })?;
            Ok(usize::try_from(parsed).unwrap_or(usize::MAX))
        }
    }
}

/// Slices a fully loaded cache list by integer-offset cursor.
pub(super) fn offset_page<T>(all: Vec<T>, limit: usize, cursor: Option<&str>) -> Result<ListPage<T>> {
    require_limit(limit)?;
    let offset = parse_offset_cursor(cursor)?;
    let total = all.len();
    if offset >= total {
        return Ok(ListPage {
            items: Vec::new(),
            has_more: false,
            next_cursor: None,
            hint: None,
        });
    }
    let end = offset.saturating_add(limit).min(total);
    let items: Vec<T> = all.into_iter().skip(offset).take(end - offset).collect();
    let has_more = end < total;
    let next_cursor = has_more.then(|| end.to_string());
    Ok(ListPage {
        items,
        has_more,
        next_cursor,
        hint: None,
    })
}

pub(super) fn search_hits_from_response(resp: SearchResponse, limit: usize) -> Vec<NotionSearchHit> {
    let mut hits = Vec::new();
    for p in resp.pages {
        hits.push(NotionSearchHit {
            id: p.id.clone(),
            workspace_id: p.workspace_id.clone(),
            item_type: "page".into(),
            title: p.title.clone(),
            icon: p.icon.clone(),
            snippet: p.properties_summary(),
            url: p.url.clone(),
            last_edited_time: p.last_edited_time.clone(),
        });
    }
    for d in resp.databases {
        hits.push(NotionSearchHit {
            id: d.id.clone(),
            workspace_id: d.workspace_id.clone(),
            item_type: "database".into(),
            title: d.title.clone(),
            icon: d.icon.clone(),
            snippet: d.description.clone(),
            url: d.url.clone(),
            last_edited_time: d.last_edited_time.clone(),
        });
    }
    hits.truncate(limit);
    hits
}


pub(super) fn read_body_or_file(body: Option<String>, file: Option<PathBuf>) -> Result<Option<String>> {
    let file_content = match file {
        Some(path) => Some(
            fs::read_to_string(&path)
                .with_context(|| format!("Failed to read input file: {}", path.display()))?,
        ),
        None => None,
    };
    match (body, file_content) {
        (Some(b), Some(f)) if !b.is_empty() && !f.is_empty() => {
            Ok(Some(format!("{}\n{}", b.trim_end_matches('\n'), f)))
        }
        (Some(b), Some(_)) if !b.is_empty() => Ok(Some(b)),
        (_, Some(f)) => Ok(Some(f)),
        (Some(b), None) => Ok(Some(b)),
        (None, None) => Ok(None),
    }
}

pub(super) fn print_json<T: serde::Serialize>(val: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(val)?);
    Ok(())
}

pub(super) fn warn_incomplete_blocks(blocks: &[NotionBlock]) {
    let ids = incomplete_block_ids(blocks);
    if !ids.is_empty() {
        eprintln!(
            "Warning: {} block(s) have children that were not loaded: {}. Read them with: notion blocks list <block-id>",
            ids.len(),
            ids.join(", ")
        );
    }
}

pub(super) fn warn_more_results<T>(page: &ListPage<T>) {
    if page.has_more {
        if let Some(next) = &page.next_cursor {
            eprintln!("More results available: rerun with --json --cursor {next}");
        } else if let Some(hint) = &page.hint {
            eprintln!("{hint}");
        }
    }
}

