//! Notion page CLI subcommands and shared page domain operations.

use std::fs;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{
    config,
    models::{coded, NotionDatabase, NotionPage, NotionPageDetail, NotionPropertyValue},
    notion::{NotionApiError, NotionClient, DETAIL_COMMENTS_LIMIT},
    parser::{incomplete_block_ids, parse_slash_command_blocks},
    resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    storage::{notion::CachedComments, Storage},
    sync,
};

use super::{
    args::PagesCommand,
    blocks::{apply_page_edit, op_append_blocks},
    databases::{database_item_properties, op_get_database, property_map},
    format::{
        list_envelope, offset_page, parse_offset_cursor, print_json, read_body_or_file,
        require_limit, warn_incomplete_blocks, warn_more_results, ListPage,
    },
};

pub(super) async fn handle_pages_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: PagesCommand,
) -> Result<()> {
    match action {
        PagesCommand::List {
            archived,
            refresh,
            offline,
            limit,
            cursor,
            json: as_json,
        } => {
            let list = op_list_pages(
                client,
                storage,
                archived,
                refresh,
                offline,
                limit,
                cursor.as_deref(),
            )
            .await?;
            if as_json {
                print_json(&list_envelope("pages", &list))?;
            } else {
                for p in &list.items {
                    let fav = if p.is_favorite { "★ " } else { "" };
                    println!(
                        "{fav}{} {} ({})",
                        sanitize_terminal_inline(p.display_icon()),
                        sanitize_terminal_inline(p.display_title()),
                        sanitize_terminal_inline(&p.id)
                    );
                }
                warn_more_results(&list);
            }
            Ok(())
        }
        PagesCommand::Get {
            page,
            format,
            refresh,
            offline,
            json: as_json,
            full,
            compact,
            fields,
        } => {
            let _ = compact;
            if as_json && !fields.is_empty() {
                let val =
                    op_get_page_fields(client, storage, &page, refresh, offline, &fields).await?;
                print_json(&val)?;
            } else if as_json && !full {
                let val = op_get_page_compact(client, storage, &page, refresh, offline, |id| {
                    format!("notion pages get {id} --json --full")
                })
                .await?;
                print_json(&val)?;
            } else if as_json {
                let (detail, comments_has_more) =
                    op_get_page_for_output(client, storage, &page, refresh, offline).await?;
                print_json(&detail.full_json(comments_has_more))?;
            } else {
                let detail =
                    op_get_page_with_options(client, storage, &page, refresh, offline).await?;
                if let Some(error) = &detail.comments_error {
                    eprintln!("Could not load comments: {error}");
                }
                match format.as_str() {
                    "plain" => println!("{}", sanitize_terminal_text(&detail.page.plain_text)),
                    "blocks" => print_json(&detail.blocks)?,
                    _ => {
                        println!(
                            "# {} {}\n",
                            sanitize_terminal_inline(detail.page.display_icon()),
                            sanitize_terminal_inline(detail.page.display_title())
                        );
                        println!("{}", sanitize_terminal_text(&detail.markdown));
                    }
                }
                warn_incomplete_blocks(&detail.blocks);
            }
            Ok(())
        }
        PagesCommand::Create {
            parent,
            title,
            icon,
            body,
            file,
            database,
            json: as_json,
        } => {
            let md = read_body_or_file(body, file)?;
            let created = op_create_page(
                client,
                storage,
                &parent,
                &title,
                icon.as_deref(),
                md.as_deref(),
                database,
                None,
            )
            .await?;
            if as_json {
                print_json(&created)?;
            } else {
                println!(
                    "Created page '{}' ({}) -> {}",
                    sanitize_terminal_inline(&created.title),
                    sanitize_terminal_inline(&created.id),
                    sanitize_terminal_inline(&created.url)
                );
            }
            Ok(())
        }
        PagesCommand::Append {
            page,
            body,
            file,
            json: as_json,
            full,
        } => {
            let text = read_body_or_file(body, file)?
                .ok_or_else(|| anyhow::anyhow!("Provide content via positional body or --file"))?;
            let blocks = op_append_blocks(client, storage, &page, &text).await?;
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
                println!("Appended {} block(s).", blocks.len());
            }
            Ok(())
        }
        PagesCommand::Edit {
            page,
            title,
            icon,
            editor,
            json: as_json,
        } => {
            let ws_id = client.auth().active_workspace_id().await;
            let page_id =
                resolver::resolve_page_id_for_write(storage, client, &ws_id, &page, false).await?;
            let edited_page = if editor {
                let detail = sync::sync_page_detail_force(client, storage, &page_id).await?;
                let initial = crate::page_edit::editor_markdown_for_detail(&detail);
                let edited = crate::ui::composer::external_edit(&initial)
                    .context("Failed to edit page in external $EDITOR")?;
                if edited.trim_end() == initial.trim_end() {
                    Some(detail.page)
                } else {
                    let save_result: Result<NotionPage> = async {
                        let _ = crate::page_edit::PageEdit::plan(&page_id, &detail.blocks, &edited)?;
                        let fresh = client.fetch_block_tree(&page_id, None).await?;
                        if !crate::page_edit::same_edit_baseline(&detail.blocks, &fresh) {
                            bail!("Page blocks changed while the editor was open. Reopen the page before saving.");
                        }
                        Ok(apply_page_edit(client, storage, &page_id, &edited, Some(fresh), false)
                            .await?
                            .page)
                    }
                    .await;
                    match save_result {
                        Ok(page) => Some(page),
                        Err(err) => match crate::page_edit::save_edit_recovery(&page_id, &edited) {
                            Ok(path) => {
                                bail!(
                                    "{err}\nEdited text saved for recovery at: {}",
                                    path.display()
                                );
                            }
                            Err(_) => return Err(err),
                        },
                    }
                }
            } else {
                None
            };
            let icon_opt = icon
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(Some);
            let updated = if title.is_some() || icon_opt.is_some() {
                op_update_page_metadata(
                    client,
                    storage,
                    &page_id,
                    title.as_deref().map(Some),
                    icon_opt,
                )
                .await?
            } else if let Some(page) = edited_page {
                page
            } else {
                return Err(coded(
                    "invalid_input",
                    "Provide --title, --icon, or --editor",
                ));
            };
            if as_json {
                print_json(&updated)?;
            } else {
                println!(
                    "Updated page '{}' ({}).",
                    sanitize_terminal_inline(&updated.title),
                    sanitize_terminal_inline(&updated.id)
                );
            }
            Ok(())
        }
        PagesCommand::Export {
            page,
            out,
            offline,
            json: as_json,
        } => {
            let detail = op_get_page(client, storage, &page, offline).await?;
            let full_md = format!("# {}\n\n{}\n", detail.page.display_title(), detail.markdown);
            if let Some(ref path) = out {
                config::ensure_parent(path)?;
                fs::write(path, full_md.as_bytes())?;
            }
            if as_json {
                if let Some(path) = out {
                    print_json(&json!({
                        "ok": true,
                        "page_id": detail.page.id,
                        "title": detail.page.title,
                        "path": path.to_string_lossy().into_owned(),
                        "bytes": full_md.len(),
                        "incomplete_blocks": incomplete_block_ids(&detail.blocks),
                    }))?;
                } else {
                    print_json(&json!({
                        "ok": true,
                        "page_id": detail.page.id,
                        "title": detail.page.title,
                        "path": Value::Null,
                        "markdown": full_md,
                        "incomplete_blocks": incomplete_block_ids(&detail.blocks),
                    }))?;
                }
            } else {
                if let Some(path) = out {
                    println!(
                        "Exported '{}' -> {}",
                        sanitize_terminal_inline(&detail.page.title),
                        path.display()
                    );
                } else {
                    print!("{}", sanitize_terminal_text(&full_md));
                }
                warn_incomplete_blocks(&detail.blocks);
            }
            Ok(())
        }
        PagesCommand::Archive {
            page,
            unarchive,
            json: as_json,
        } => {
            let updated = op_archive_page(client, storage, &page, !unarchive).await?;
            if as_json {
                print_json(&updated)?;
            } else {
                let verb = if unarchive { "Restored" } else { "Archived" };
                println!(
                    "{verb} page '{}' ({}).",
                    sanitize_terminal_inline(&updated.title),
                    sanitize_terminal_inline(&updated.id)
                );
            }
            Ok(())
        }
    }
}


pub async fn op_list_pages(
    client: &NotionClient,
    storage: &Storage,
    include_archived: bool,
    refresh: bool,
    offline: bool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<ListPage<NotionPage>> {
    require_limit(limit)?;
    let _ = parse_offset_cursor(cursor)?;
    let ws_id = client.auth().active_workspace_id().await;
    let all = if !offline {
        if refresh || !sync::workspace_cache_complete(storage, &ws_id) {
            let (_, pages, _) = sync::sync_workspace_tree(client, storage).await?;
            pages
                .into_iter()
                .filter(|p| include_archived || !p.archived)
                .collect()
        } else {
            storage.list_pages(&ws_id, include_archived)?
        }
    } else {
        storage.list_pages(&ws_id, include_archived)?
    };
    offset_page(all, limit, cursor)
}

pub async fn op_get_page(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    offline: bool,
) -> Result<NotionPageDetail> {
    op_get_page_with_options(client, storage, page, false, offline).await
}

pub async fn op_get_page_with_options(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    refresh: bool,
    offline: bool,
) -> Result<NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;
    if offline {
        let mut detail = storage
            .load_page_detail(&ws_id, &id)?
            .ok_or_else(|| coded("not_cached", format!("Page '{page}' not found in local cache")))?;
        if storage.load_cached_children(&ws_id, &id)?.is_none() {
            return Err(coded(
                "not_cached",
                format!(
                    "Page '{page}' body not found in offline cache (not fetched); read it online once"
                ),
            ));
        }
        if detail.comments_error.is_none()
            && matches!(
                storage.load_cached_comments(&ws_id, &id)?,
                CachedComments::NotFetched
            )
        {
            detail.comments_error = Some("Comments not fetched into the offline cache".into());
        }
        return Ok(detail);
    }
    if refresh {
        sync::sync_page_detail_force(client, storage, &id).await
    } else {
        sync::sync_page_detail(client, storage, &id).await
    }
}

/// Compact page JSON. Fetches metadata + blocks only; comment facts come from cache markers.
pub async fn op_get_page_compact(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    refresh: bool,
    offline: bool,
    hint_for: impl Fn(&str) -> String,
) -> Result<Value> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;
    let detail = if offline {
        let detail = storage
            .load_page_detail(&ws_id, &id)?
            .ok_or_else(|| coded("not_cached", format!("Page '{page}' not found in local cache")))?;
        if storage.load_cached_children(&ws_id, &id)?.is_none() {
            return Err(coded(
                "not_cached",
                format!(
                    "Page '{page}' body not found in offline cache (not fetched); read it online once"
                ),
            ));
        }
        detail
    } else {
        sync::sync_page_body(client, storage, &id, refresh).await?
    };
    let summary = storage.cached_comments_summary(&ws_id, &detail.page.id)?;
    Ok(detail.compact(
        hint_for(&detail.page.id),
        summary.state,
        summary.count,
        summary.error.as_deref(),
    ))
}

/// Selective field/property projection for a Notion page.
/// Avoids fetching comments unless `"comments"` is explicitly requested, and avoids
/// fetching block trees when only page metadata/properties are requested.
pub async fn op_get_page_fields(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    refresh: bool,
    offline: bool,
    fields: &[String],
) -> Result<Value> {
    let (needs_blocks, needs_comments) = NotionPageDetail::requested_fields_needs(fields);
    if needs_comments {
        let (detail, comments_has_more) =
            op_get_page_for_output(client, storage, page, refresh, offline).await?;
        return Ok(detail.project_fields(fields, Some(comments_has_more)));
    }

    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id(storage, (!offline).then_some(client), &ws_id, page).await?;

    if needs_blocks {
        let detail = if offline {
            let detail = storage
                .load_page_detail(&ws_id, &id)?
                .ok_or_else(|| coded("not_cached", format!("Page '{page}' not found in local cache")))?;
            if storage.load_cached_children(&ws_id, &id)?.is_none() {
                return Err(coded(
                    "not_cached",
                    format!(
                        "Page '{page}' body not found in offline cache (not fetched); read it online once"
                    ),
                ));
            }
            detail
        } else {
            sync::sync_page_body(client, storage, &id, refresh).await?
        };
        return Ok(detail.project_fields(fields, None));
    }

    let loaded_page = if offline {
        storage
            .get_page(&ws_id, &id)?
            .ok_or_else(|| coded("not_cached", format!("Page '{page}' not found in local cache")))?
    } else {
        let fresh = client.get_page(&id).await?;
        let _ = storage.upsert_page(&fresh);
        fresh
    };
    let detail = NotionPageDetail {
        page: loaded_page,
        ..Default::default()
    };
    Ok(detail.project_fields(fields, None))
}

/// Full page detail plus whether more comments exist than the detail carries.
pub async fn op_get_page_for_output(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    refresh: bool,
    offline: bool,
) -> Result<(NotionPageDetail, bool)> {
    let ws_id = client.auth().active_workspace_id().await;
    let detail = op_get_page_with_options(client, storage, page, refresh, offline).await?;
    let comments_has_more = if detail.comments_error.is_some() {
        false
    } else {
        detail.comments.len() > DETAIL_COMMENTS_LIMIT
            || storage.comments_partial(&ws_id, &detail.page.id)?
    };
    Ok((detail, comments_has_more))
}

pub(super) fn is_notion_validation_error(err: &anyhow::Error) -> bool {
    err.chain()
        .find_map(|c| c.downcast_ref::<NotionApiError>())
        .is_some_and(|e| e.status == 400 && e.code == "validation_error")
}

pub(super) fn title_key_from_database(db: &NotionDatabase) -> Option<String> {
    db.schema
        .iter()
        .find_map(|(name, prop)| (prop.prop_type == "title").then(|| name.clone()))
}

pub(super) fn title_key_from_page(page: &NotionPage) -> Option<String> {
    page.properties
        .iter()
        .find_map(|(name, prop)| matches!(prop, NotionPropertyValue::Title(_)).then(|| name.clone()))
}

pub(super) fn page_property_columns(page: &NotionPage) -> Vec<(&str, &'static str)> {
    page.properties
        .iter()
        .filter_map(|(name, value)| {
            let kind = match value {
                NotionPropertyValue::Title(_) => "title",
                NotionPropertyValue::Status(_) => "status",
                NotionPropertyValue::Select(_) => "select",
                _ => return None,
            };
            Some((name.as_str(), kind))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn op_create_page(
    client: &NotionClient,
    storage: &Storage,
    parent: &str,
    title: &str,
    icon: Option<&str>,
    markdown_body: Option<&str>,
    parent_is_database: bool,
    extra_properties: Option<Value>,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let parent_id = if parent_is_database {
        resolver::resolve_database_id_for_write(storage, client, &ws_id, parent).await?
    } else {
        resolver::resolve_page_id_for_write(storage, client, &ws_id, parent, false).await?
    };

    let parent_json = if parent_is_database {
        json!({ "database_id": parent_id })
    } else {
        json!({ "page_id": parent_id })
    };

    let mut props = match extra_properties {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    let mut cached_title_key: Option<String> = None;
    if !props.values().any(|value| value.get("title").is_some()) {
        let title_key = if parent_is_database {
            if let Some(key) = storage
                .get_database(&ws_id, &parent_id)?
                .and_then(|db| title_key_from_database(&db))
            {
                cached_title_key = Some(key.clone());
                key
            } else {
                let db = op_get_database(client, storage, &parent_id, false).await?;
                title_key_from_database(&db)
                    .context("Database does not have a title property")?
            }
        } else {
            "title".to_string()
        };
        if !props.contains_key(&title_key) {
            props.insert(
                title_key,
                json!({ "title": [{ "type": "text", "text": { "content": title } }] }),
            );
        }
    }

    let mut payload = json!({
        "parent": parent_json,
        "properties": Value::Object(props),
    });
    if let Some(ic) = icon.map(str::trim).filter(|s| !s.is_empty()) {
        payload["icon"] = json!({ "type": "emoji", "emoji": ic });
    }
    let mut overflow_blocks = Vec::new();
    let mut total_blocks = 0usize;
    if let Some(body) = markdown_body.map(str::trim).filter(|s| !s.is_empty()) {
        let mut blocks = parse_slash_command_blocks(body);
        total_blocks = blocks.len();
        if blocks.len() > 100 {
            overflow_blocks = blocks.split_off(100);
        }
        if !blocks.is_empty() {
            payload["children"] = Value::Array(blocks);
        }
    }

    let created = match client.create_page(payload.clone()).await {
        Ok(page) => page,
        Err(err) if is_notion_validation_error(&err) => {
            let Some(old_key) = cached_title_key else {
                return Err(err);
            };
            let db = op_get_database(client, storage, &parent_id, false).await?;
            let fresh_key = title_key_from_database(&db)
                .context("Database does not have a title property")?;
            if fresh_key != old_key {
                if let Some(map) = payload.get_mut("properties").and_then(Value::as_object_mut) {
                    if let Some(val) = map.remove(&old_key) {
                        map.insert(fresh_key, val);
                    }
                }
                client.create_page(payload).await?
            } else {
                return Err(err);
            }
        }
        Err(err) => return Err(err),
    };
    let _ = storage.upsert_page(&created);
    if !overflow_blocks.is_empty() {
        if let Err(err) = client
            .append_block_children(&created.id, overflow_blocks)
            .await
        {
            bail!(
                "Page created ({}) but appending blocks 101..{} failed: {err}. Append the rest with: notion pages append {} --file …",
                created.id,
                total_blocks,
                created.id
            );
        }
    }
    Ok(created)
}

/// `None` leaves the field untouched. `Some(None)` clears it.
pub(crate) async fn op_update_page_metadata(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    title: Option<Option<&str>>,
    icon: Option<Option<&str>>,
) -> Result<NotionPage> {
    if title.is_none() && icon.is_none() {
        bail!("Provide title or icon");
    }
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id_for_write(storage, client, &ws_id, page, false).await?;
    let mut used_cached_key = false;
    let title_key = if title.is_some() {
        if let Some(key) = storage
            .get_page(&ws_id, &id)?
            .and_then(|p| title_key_from_page(&p))
        {
            used_cached_key = true;
            key
        } else {
            let current = client.get_page(&id).await?;
            let _ = storage.upsert_page(&current);
            title_key_from_page(&current).context("Page has no accessible title property")?
        }
    } else {
        String::new()
    };
    let title_arg = title.map(|text| (title_key.as_str(), text));
    match patch_page_metadata(client, storage, &id, title_arg, icon).await {
        Ok(updated) => Ok(updated),
        Err(err) if used_cached_key && is_notion_validation_error(&err) => {
            let current = client.get_page(&id).await?;
            let _ = storage.upsert_page(&current);
            let fresh_key =
                title_key_from_page(&current).context("Page has no accessible title property")?;
            if fresh_key != title_key {
                let retry_title = title.map(|text| (fresh_key.as_str(), text));
                patch_page_metadata(client, storage, &id, retry_title, icon).await
            } else {
                Err(err)
            }
        }
        Err(err) => Err(err),
    }
}

async fn patch_page_metadata(
    client: &NotionClient,
    storage: &Storage,
    id: &str,
    title: Option<(&str, Option<&str>)>,
    icon: Option<Option<&str>>,
) -> Result<NotionPage> {
    let mut payload = serde_json::Map::new();
    if let Some((key, text)) = title {
        let spans = match text.filter(|text| !text.is_empty()) {
            Some(text) => json!([{ "type": "text", "text": { "content": text } }]),
            None => json!([]),
        };
        payload.insert("properties".into(), json!({ (key): { "title": spans } }));
    }
    if let Some(icon) = icon {
        let value = match icon {
            Some(emoji) => json!({ "type": "emoji", "emoji": emoji }),
            None => Value::Null,
        };
        payload.insert("icon".into(), value);
    }
    let updated = client.update_page(id, Value::Object(payload)).await?;
    let _ = storage.upsert_page(&updated);
    Ok(updated)
}

/// Fields of one `PATCH /api/v1/pages/{id}` request. `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct PagePatch<'a> {
    pub title: Option<&'a str>,
    pub icon: Option<&'a str>,
    pub archived: Option<bool>,
    pub properties: Option<Value>,
}

/// Applies every field of `patch` in one Notion `PATCH /v1/pages/{id}` call. A request with a
/// single field sends the same payload as the dedicated operation for that field.
pub async fn op_patch_page(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    patch: PagePatch<'_>,
) -> Result<NotionPage> {
    let PagePatch {
        title,
        icon,
        archived,
        properties,
    } = patch;
    if title.is_none() && icon.is_none() && archived.is_none() && properties.is_none() {
        bail!("At least one of title, icon, archived, properties is required");
    }
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id_for_write(storage, client, &ws_id, page, archived == Some(false))
        .await?;
    let build_patch_payload = |current: Option<&NotionPage>| -> Result<serde_json::Map<String, Value>> {
        let mut payload = serde_json::Map::new();
        if properties.is_some() {
            // Same property building as `op_update_database_item`.
            let props = if title.is_some() {
                let page_ref = current.context("Page metadata required to resolve title property")?;
                let cols = page_property_columns(page_ref);
                database_item_properties(cols.into_iter(), title, None, properties.clone())?
            } else {
                property_map(properties.clone())?
            };
            if !props.is_empty() {
                payload.insert("properties".into(), Value::Object(props));
            }
        } else if let Some(text) = title {
            // Same title payload as `op_update_page_metadata`.
            let page_ref = current.context("Page metadata required to resolve title property")?;
            let key = title_key_from_page(page_ref)
                .context("Page has no accessible title property")?;
            let spans = if text.is_empty() {
                json!([])
            } else {
                json!([{ "type": "text", "text": { "content": text } }])
            };
            payload.insert("properties".into(), json!({ (key): { "title": spans } }));
        }
        if let Some(emoji) = icon {
            payload.insert("icon".into(), json!({ "type": "emoji", "emoji": emoji }));
        }
        if let Some(archived) = archived {
            payload.insert("archived".into(), json!(archived));
        }
        if payload.is_empty() {
            bail!("At least one of title, icon, archived, properties is required");
        }
        Ok(payload)
    };

    let mut used_cached_meta = false;
    let payload = if title.is_some() {
        let cached_attempt = storage
            .get_page(&ws_id, &id)?
            .and_then(|cached| build_patch_payload(Some(&cached)).ok());
        if let Some(p) = cached_attempt {
            used_cached_meta = true;
            p
        } else {
            let current = client.get_page(&id).await?;
            let _ = storage.upsert_page(&current);
            build_patch_payload(Some(&current))?
        }
    } else {
        build_patch_payload(None)?
    };
    let updated = match client.update_page(&id, Value::Object(payload)).await {
        Ok(u) => u,
        Err(err) if used_cached_meta && is_notion_validation_error(&err) => {
            let current = client.get_page(&id).await?;
            let _ = storage.upsert_page(&current);
            let fresh_payload = build_patch_payload(Some(&current))?;
            client.update_page(&id, Value::Object(fresh_payload)).await?
        }
        Err(err) => return Err(err),
    };
    let _ = storage.upsert_page(&updated);
    Ok(updated)
}

pub async fn op_archive_page(
    client: &NotionClient,
    storage: &Storage,
    page: &str,
    archived: bool,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let id =
        resolver::resolve_page_id_for_write(storage, client, &ws_id, page, !archived).await?;
    let updated = client.set_page_archived(&id, archived).await?;
    let _ = storage.upsert_page(&updated);
    Ok(updated)
}

