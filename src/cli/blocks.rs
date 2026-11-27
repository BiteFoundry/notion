//! Notion block CLI subcommands and shared block/page-edit domain operations.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{
    models::{coded, normalize_notion_id, NotionBlock, NotionBlockKind, NotionPageDetail},
    notion::NotionClient,
    parser::{
        blocks_to_markdown, incomplete_block_ids, notion::rich_text_to_markdown,
        parse_notion_block, parse_parent, parse_rich_text_array, parse_slash_command_blocks,
    },
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    storage::Storage,
    sync,
};

use super::{
    args::BlocksCommand,
    format::{
        list_envelope, offset_page, print_json, read_body_or_file, require_limit,
        warn_incomplete_blocks, warn_more_results, ListPage,
    },
};

pub(super) async fn handle_blocks_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: BlocksCommand,
) -> Result<()> {
    match action {
        BlocksCommand::List {
            block_id,
            offline,
            limit,
            cursor,
            json: as_json,
        } => {
            let effective_limit = limit.unwrap_or(if as_json { 50 } else { usize::MAX });
            let list = op_list_blocks(
                client,
                storage,
                &block_id,
                offline,
                effective_limit,
                cursor.as_deref(),
            )
            .await?;
            if as_json {
                let mut val = list_envelope("blocks", &list);
                if let Some(map) = val.as_object_mut() {
                    map.insert(
                        "incomplete_blocks".into(),
                        json!(incomplete_block_ids(&list.items)),
                    );
                }
                print_json(&val)?;
            } else {
                println!("{}", sanitize_terminal_text(&blocks_to_markdown(&list.items)));
                warn_incomplete_blocks(&list.items);
                warn_more_results(&list);
            }
            Ok(())
        }
        BlocksCommand::Append {
            block_id,
            body,
            file,
            json: as_json,
            full,
        } => {
            let text = read_body_or_file(body, file)?
                .ok_or_else(|| anyhow::anyhow!("Provide content via positional body or --file"))?;
            let blocks = op_append_blocks(client, storage, &block_id, &text).await?;
            if as_json {
                if full {
                    print_json(&json!({ "ok": true, "appended": blocks.len(), "blocks": blocks }))?;
                } else {
                    let block_ids: Vec<&str> = blocks.iter().map(|b| b.id.as_str()).collect();
                    print_json(&json!({
                        "ok": true,
                        "appended": blocks.len(),
                        "block_ids": block_ids,
                    }))?;
                }
            } else {
                println!("Appended {} block(s) to {block_id}.", blocks.len());
            }
            Ok(())
        }
        BlocksCommand::Edit { block_id, text, json: as_json } => {
            let block = op_update_block_text(client, storage, &block_id, None, &text, None).await?;
            if as_json {
                print_json(&block)?;
            } else {
                println!("Updated block {}.", block.id);
            }
            Ok(())
        }
        BlocksCommand::Toggle {
            block_id,
            checked,
            json: as_json,
        } => {
            let blk = op_toggle_todo(client, storage, &block_id, checked).await?;
            if as_json {
                print_json(&blk)?;
            } else {
                let state = match blk.kind {
                    NotionBlockKind::ToDo { checked, .. } => {
                        if checked {
                            "[x]"
                        } else {
                            "[ ]"
                        }
                    }
                    _ => "[?]",
                };
                println!(
                    "{state} {} ({})",
                    sanitize_terminal_inline(&blk.plain_text()),
                    sanitize_terminal_inline(&blk.id)
                );
            }
            Ok(())
        }
        BlocksCommand::Delete {
            block_id,
            json: as_json,
        } => {
            let resp = op_delete_block(client, storage, &block_id).await?;
            if as_json {
                print_json(&json!({ "ok": true, "deleted": block_id, "response": resp }))?;
            } else {
                println!("Deleted block {block_id}.");
            }
            Ok(())
        }
    }
}


pub async fn op_list_blocks(
    client: &NotionClient,
    storage: &Storage,
    block_or_page: &str,
    offline: bool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<ListPage<NotionBlock>> {
    require_limit(limit)?;
    let ws_id = client.auth().active_workspace_id().await;
    let trimmed_target = block_or_page.trim();
    let id = if storage.has_cached_block(&ws_id, trimmed_target) {
        trimmed_target.to_string()
    } else {
        resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, block_or_page)
            .await?
    };
    if offline {
        let cached = storage.load_cached_children(&ws_id, &id)?.ok_or_else(|| {
            coded(
                "not_cached",
                format!("Children of '{block_or_page}' not found in offline cache (not fetched)"),
            )
        })?;
        return offset_page(cached, limit, cursor);
    }
    let (blocks, has_more, next_cursor) = client.fetch_block_tree_page(&id, limit, cursor).await?;
    if cursor.is_none() && !has_more {
        let _ = storage.replace_page_blocks(&ws_id, &id, &blocks);
    }
    Ok(ListPage {
        items: blocks,
        has_more,
        next_cursor,
        hint: None,
    })
}

pub async fn op_append_blocks(
    client: &NotionClient,
    storage: &Storage,
    page_or_block: &str,
    markdown_or_slash: &str,
) -> Result<Vec<NotionBlock>> {
    let ws_id = client.auth().active_workspace_id().await;
    let trimmed_target = page_or_block.trim();
    let id = if storage.has_cached_block(&ws_id, trimmed_target) {
        trimmed_target.to_string()
    } else {
        resolver::resolve_page_id_for_write(storage, client, &ws_id, page_or_block, false).await?
    };
    let children = parse_slash_command_blocks(markdown_or_slash);
    if children.is_empty() {
        bail!("No blocks produced from input text");
    }
    let appended = client.append_block_children(&id, children).await;
    match appended {
        Ok(mut blocks) => {
            if blocks.iter().any(|b| b.has_children) {
                let _ = client.hydrate_block_subtrees(&mut blocks, &id).await;
            }
            let _ = storage.append_cached_blocks(&ws_id, &id, &blocks);
            Ok(blocks)
        }
        Err(err) => {
            let _ = storage.invalidate_page_detail(&ws_id, &id);
            if let Ok(full_tree) = client.fetch_block_tree(&id, None).await {
                let _ = storage.replace_page_blocks(&ws_id, &id, &full_tree);
            }
            Err(err)
        }
    }
}

/// Apply a Markdown edit using the hierarchy and rendered ranges of the original blocks.
pub async fn op_replace_page_markdown(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
) -> Result<NotionPageDetail> {
    op_diff_sync_page_markdown(client, storage, page, edited_markdown, None).await
}

pub async fn op_diff_sync_page_markdown(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
    known_existing: Option<Vec<NotionBlock>>,
) -> Result<NotionPageDetail> {
    apply_page_edit(client, storage, page, edited_markdown, known_existing, true).await
}

pub(super) async fn apply_page_edit(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    edited_markdown: &str,
    known_existing: Option<Vec<NotionBlock>>,
    retry_stale: bool,
) -> Result<NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id_for_write(storage, client, &ws_id, page, false).await?;
    let known = known_existing.filter(|blocks| blocks.iter().all(|b| {
        !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_")
    }));
    let existing = match known.as_ref() {
        Some(blocks) => blocks.clone(),
        None => client.fetch_block_tree(&id, None).await?,
    };
    let plan = crate::page_edit::PageEdit::plan(&id, &existing, edited_markdown)?;
    if plan.is_empty() {
        return sync::sync_page_detail_force(client, storage, &id).await;
    }
    let column_guarded = crate::parser::has_column_blocks(&existing)
        || crate::page_edit::contains_column_marker(edited_markdown);
    if retry_stale && known.is_some() && column_guarded {
        let fresh = client.fetch_block_tree(&id, None).await?;
        if !crate::page_edit::same_edit_baseline(&existing, &fresh) {
            bail!("Page blocks changed while editing. Reload the page before saving.");
        }
    }
    if let Err(error) = plan.apply(client, &id).await {
        // Refresh the cache even on a mid-save failure. A subsequent save can match
        // already appended content at its actual position and reuse its server ID.
        let _ = storage.invalidate_page_detail(&ws_id, &id);
        let recovered = sync::sync_page_detail_force(client, storage, &id).await;
        if let Ok(fresh) = recovered {
            if retry_stale
                && !column_guarded
                && known.is_some()
                && NotionClient::is_authoritative_not_found(&error)
            {
                // Re-planning against the fresh tree would overwrite a teammate's change with the
                // stale text in the user's buffer; only retry when nothing else changed.
                if !crate::page_edit::retry_is_safe(&existing, &fresh.blocks, edited_markdown) {
                    bail!("Page blocks changed while editing. Reload the page before saving.");
                }
                let retry = crate::page_edit::PageEdit::plan(&id, &fresh.blocks, edited_markdown)?;
                if let Err(error) = retry.apply(client, &id).await {
                    let _ = storage.invalidate_page_detail(&ws_id, &id);
                    let _ = sync::sync_page_detail_force(client, storage, &id).await;
                    return Err(error);
                }
                return sync::sync_page_detail_force(client, storage, &id).await;
            }
        } else if let Ok(blocks) = client.fetch_block_tree(&id, None).await {
            let _ = storage.replace_page_blocks(&ws_id, &id, &blocks);
        }
        return Err(error);
    }
    let _ = storage.invalidate_page_detail(&ws_id, &id);
    sync::sync_page_detail_force(client, storage, &id).await
}

pub async fn op_update_block_text(
    client: &NotionClient,
    storage: &Storage,
    block_id: &str,
    block_type: Option<&str>,
    text: &str,
    checked: Option<bool>,
) -> Result<NotionBlock> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = normalize_notion_id(block_id);
    let original = client.get_block_json(&norm_id).await?;
    let current = parse_notion_block(&original, "", None);
    let btype = &current.block_type;
    if let Some(requested) = block_type.map(str::trim).filter(|t| !t.is_empty()) {
        if requested != btype {
            bail!("Block '{block_id}' is a {btype}, not a {requested}");
        }
    }
    if !crate::page_edit::patchable(btype) {
        bail!("Block '{block_id}' does not have editable rich text");
    }
    let mut inner = serde_json::Map::new();
    let raw = original[btype]["rich_text"].as_array().map(Vec::as_slice).unwrap_or_default();
    let parsed = parse_rich_text_array(original[btype].get("rich_text"));
    if btype == "code" {
        if crate::models::spans_plain_text(&parsed) != text {
            inner.insert("rich_text".into(), json!(crate::page_edit::code_rich_text(text)));
        }
    } else if rich_text_to_markdown(&parsed) != text {
        inner.insert("rich_text".into(), json!(crate::page_edit::edited_rich_text(raw, text)?));
    }
    if let (NotionBlockKind::ToDo { checked: actual, .. }, Some(checked)) = (&current.kind, checked) {
        if *actual != checked {
            inner.insert("checked".into(), json!(checked));
        }
    }
    if inner.is_empty() {
        return Ok(current);
    }
    let mut payload = serde_json::Map::new();
    payload.insert(btype.clone(), Value::Object(inner));
    let mut updated = client
        .update_block(&norm_id, "", Value::Object(payload))
        .await?;
    if let Ok(Some((pid, _))) = storage.apply_updated_block(&ws_id, &updated) {
        if updated.page_id.trim().is_empty() {
            updated.page_id = pid;
        }
    }
    Ok(updated)
}

pub async fn op_toggle_todo(
    client: &NotionClient,
    storage: &Storage,
    block_id: &str,
    explicit_checked: Option<bool>,
) -> Result<NotionBlock> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = normalize_notion_id(block_id);
    let target_checked = match explicit_checked {
        Some(val) => val,
        None => {
            let current = client.get_block(&norm_id, "").await?;
            match current.kind {
                NotionBlockKind::ToDo { checked, .. } => !checked,
                _ => bail!("Block '{block_id}' is not a to_do block"),
            }
        }
    };
    let mut updated = client.set_todo_checked(&norm_id, "", target_checked).await?;
    if let Ok(Some((pid, _))) = storage.apply_updated_block(&ws_id, &updated) {
        if updated.page_id.trim().is_empty() {
            updated.page_id = pid;
        }
    }
    Ok(updated)
}

pub async fn op_delete_block(
    client: &NotionClient,
    storage: &Storage,
    block_id: &str,
) -> Result<Value> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = normalize_notion_id(block_id);
    let resp = client.delete_block(&norm_id).await?;
    let deleted_page = storage.delete_cached_block(&ws_id, &norm_id).ok().flatten();
    if deleted_page.is_none() {
        let (ptype, pid) = parse_parent(resp.get("parent"));
        if ptype == "page_id" {
            if let Some(page_id) = pid {
                let _ = storage.invalidate_page_detail(&ws_id, &page_id);
            }
        }
    }
    Ok(resp)
}

