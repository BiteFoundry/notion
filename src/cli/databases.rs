//! Notion database CLI subcommands and shared database domain operations.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{
    models::{coded, NotionDatabase, NotionPage},
    notion::NotionClient,
    resolver,
    safe_text::sanitize_terminal_inline,
    storage::Storage,
    sync,
};

use super::{
    args::DbCommand,
    format::{
        list_envelope, offset_page, parse_offset_cursor, print_json, require_limit,
        warn_more_results, ListPage,
    },
    pages::{is_notion_validation_error, op_create_page, page_property_columns},
};

pub(super) async fn handle_db_cmd(
    client: &NotionClient,
    storage: &Storage,
    action: DbCommand,
) -> Result<()> {
    match action {
        DbCommand::List {
            refresh,
            offline,
            limit,
            cursor,
            json: as_json,
        } => {
            let effective_limit = limit.unwrap_or(if as_json { 50 } else { usize::MAX });
            let list = op_list_databases(
                client,
                storage,
                refresh,
                offline,
                effective_limit,
                cursor.as_deref(),
            )
            .await?;
            if as_json {
                print_json(&list_envelope("databases", &list))?;
            } else {
                for db in &list.items {
                    println!(
                        "{} {} ({})",
                        sanitize_terminal_inline(db.display_icon()),
                        sanitize_terminal_inline(db.display_title()),
                        sanitize_terminal_inline(&db.id)
                    );
                }
                warn_more_results(&list);
            }
            Ok(())
        }
        DbCommand::Get {
            database,
            offline,
            json: as_json,
        } => {
            let db = op_get_database(client, storage, &database, offline).await?;
            if as_json {
                print_json(&db)?;
            } else {
                println!(
                    "{} {} ({})",
                    sanitize_terminal_inline(db.display_icon()),
                    sanitize_terminal_inline(db.display_title()),
                    sanitize_terminal_inline(&db.id)
                );
                for (name, prop) in &db.schema {
                    println!(
                        "  - {}: {}",
                        sanitize_terminal_inline(name),
                        sanitize_terminal_inline(&prop.prop_type)
                    );
                }
            }
            Ok(())
        }
        DbCommand::Query {
            database,
            filter,
            sorts,
            limit,
            cursor,
            offline,
            json: as_json,
        } => {
            let filter_val = filter
                .as_deref()
                .map(|raw| {
                    serde_json::from_str::<Value>(raw)
                        .map_err(|e| coded("invalid_input", format!("Invalid --filter JSON: {e}")))
                })
                .transpose()?;
            let sorts_val = sorts
                .as_deref()
                .map(|raw| {
                    serde_json::from_str::<Value>(raw)
                        .map_err(|e| coded("invalid_input", format!("Invalid --sorts JSON: {e}")))
                })
                .transpose()?;
            let list = op_query_database(
                client,
                storage,
                &database,
                filter_val,
                sorts_val,
                limit,
                cursor.as_deref(),
                offline,
            )
            .await?;
            if as_json {
                print_json(&list_envelope("rows", &list))?;
            } else {
                for row in &list.items {
                    println!(
                        "{} {}  [{}]",
                        sanitize_terminal_inline(row.display_icon()),
                        sanitize_terminal_inline(row.display_title()),
                        sanitize_terminal_inline(&row.properties_summary())
                    );
                }
                warn_more_results(&list);
            }
            Ok(())
        }
        DbCommand::CreateItem {
            database,
            title,
            status,
            properties,
            body,
            json: as_json,
        } => {
            let extra_props = properties
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("Invalid --properties JSON")?;
            let created = op_create_database_item(
                client,
                storage,
                &database,
                &title,
                status.as_deref(),
                extra_props,
                body.as_deref(),
            )
            .await?;
            if as_json {
                print_json(&created)?;
            } else {
                println!(
                    "Created DB item '{}' ({}).",
                    sanitize_terminal_inline(&created.title),
                    sanitize_terminal_inline(&created.id)
                );
            }
            Ok(())
        }
        DbCommand::UpdateItem {
            page_id,
            title,
            status,
            properties,
            json: as_json,
        } => {
            let extra_props = properties
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()
                .context("Invalid --properties JSON")?;
            let updated = op_update_database_item(
                client,
                storage,
                &page_id,
                title.as_deref(),
                status.as_deref(),
                extra_props,
            )
            .await?;
            if as_json {
                print_json(&updated)?;
            } else {
                println!(
                    "Updated DB item '{}' ({}).",
                    sanitize_terminal_inline(&updated.title),
                    sanitize_terminal_inline(&updated.id)
                );
            }
            Ok(())
        }
    }
}


pub async fn op_list_databases(
    client: &NotionClient,
    storage: &Storage,
    refresh: bool,
    offline: bool,
    limit: usize,
    cursor: Option<&str>,
) -> Result<ListPage<NotionDatabase>> {
    require_limit(limit)?;
    let _ = parse_offset_cursor(cursor)?;
    let ws_id = client.auth().active_workspace_id().await;
    let all = if !offline {
        let ws_c = ws_id.clone();
        let complete = if refresh {
            false
        } else {
            storage
                .blocking(move |st| Ok(sync::workspace_cache_complete(st, &ws_c)))
                .await
                .unwrap_or(false)
        };
        if refresh || !complete {
            let (_, _, dbs) = sync::sync_workspace_tree(client, storage).await?;
            dbs
        } else {
            let ws_c = ws_id.clone();
            storage.blocking(move |st| st.list_databases(&ws_c)).await?
        }
    } else {
        let ws_c = ws_id.clone();
        storage.blocking(move |st| st.list_databases(&ws_c)).await?
    };
    offset_page(all, limit, cursor)
}

pub async fn op_get_database(
    client: &NotionClient,
    storage: &Storage,
    database: &str,
    offline: bool,
) -> Result<NotionDatabase> {
    let ws_id = client.auth().active_workspace_id().await;
    let id =
        resolver::resolve_database_id(storage, (!offline).then_some(client), &ws_id, database)
            .await?;
    if offline {
        let ws_c = ws_id.clone();
        let id_c = id.clone();
        return storage
            .blocking(move |st| st.get_database(&ws_c, &id_c))
            .await?
            .ok_or_else(|| {
                coded(
                    "not_cached",
                    format!("Database '{database}' not found in local cache"),
                )
            });
    }
    let db = match client.get_database(&id).await {
        Ok(d) => d,
        Err(err) => {
            if NotionClient::is_authoritative_not_found(&err) {
                let ws_c = ws_id.clone();
                let id_c = id.clone();
                let _ = storage
                    .blocking(move |st| st.mark_database_archived(&ws_c, &id_c))
                    .await;
            }
            return Err(err);
        }
    };
    let db_c = db.clone();
    let _ = storage.blocking(move |st| st.upsert_database(&db_c)).await;
    Ok(db)
}

#[allow(clippy::too_many_arguments)]
pub async fn op_query_database(
    client: &NotionClient,
    storage: &Storage,
    database: &str,
    filter: Option<Value>,
    sorts: Option<Value>,
    limit: usize,
    cursor: Option<&str>,
    offline: bool,
) -> Result<ListPage<NotionPage>> {
    require_limit(limit)?;
    if offline && (filter.is_some() || sorts.is_some()) {
        return Err(coded(
            "invalid_input",
            "Invalid offline query: --filter/--sorts cannot be evaluated against the local cache; rerun without --offline, or drop them to list cached rows",
        ));
    }
    let ws_id = client.auth().active_workspace_id().await;
    let id =
        resolver::resolve_database_id(storage, (!offline).then_some(client), &ws_id, database)
            .await?;
    if offline {
        let ws_c = ws_id.clone();
        let id_c = id.clone();
        let rows = storage
            .blocking(move |st| st.list_database_rows(&ws_c, &id_c))
            .await?;
        return offset_page(rows, limit, cursor);
    }
    let unfiltered = filter.is_none();
    let (rows, has_more, next_cursor) = client
        .query_database_all_with_meta(&id, filter, sorts, limit, cursor)
        .await?;
    let rows_c = rows.clone();
    let ws_c = ws_id.clone();
    let id_c = id.clone();
    let should_reconcile = unfiltered && cursor.is_none() && !has_more;
    let _ = storage
        .blocking(move |st| {
            let _ = st.upsert_pages(&rows_c);
            if should_reconcile {
                let active_row_ids: HashSet<String> = rows_c.iter().map(|r| r.id.clone()).collect();
                let _ = st.reconcile_database_rows_archived(&ws_c, &id_c, &active_row_ids);
            }
            Ok(())
        })
        .await;
    Ok(ListPage {
        items: rows,
        has_more,
        next_cursor,
        hint: None,
    })
}

pub(super) fn property_map(extra: Option<Value>) -> Result<serde_json::Map<String, Value>> {
    match extra {
        Some(Value::Object(map)) => Ok(map),
        None => Ok(serde_json::Map::new()),
        Some(_) => bail!("properties must be a JSON object"),
    }
}

fn explicit_property_compatible_with_status(existing: &Value, kind: &str, status: &str) -> bool {
    let Some(obj) = existing.as_object() else {
        return false;
    };
    if obj.is_empty() {
        return true;
    }
    if obj.len() != 1 {
        return false;
    }
    let Some(inner) = obj.get(kind).and_then(Value::as_object) else {
        return false;
    };
    if inner.is_empty() {
        return true;
    }
    inner.len() == 1
        && inner
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            == Some(status)
}

pub(super) fn database_item_properties<'a>(
    columns: impl Iterator<Item = (&'a str, &'a str)>,
    title: Option<&str>,
    status: Option<&str>,
    extra: Option<Value>,
) -> Result<serde_json::Map<String, Value>> {
    let mut props = property_map(extra)?;
    let columns: Vec<_> = columns.collect();
    if let Some(title) = title {
        let (name, _) = columns.iter().copied().find(|(_, kind)| *kind == "title")
            .context("Database does not have a title property")?;
        props.insert(name.to_owned(), json!({ "title": [{ "type": "text", "text": { "content": title } }] }));
    }
    if let Some(status) = status.map(str::trim).filter(|s| !s.is_empty()) {
        let status_columns: Vec<_> = columns
            .iter()
            .copied()
            .filter(|(_, kind)| *kind == "status")
            .collect();
        let choice = if !status_columns.is_empty() {
            let explicit_status: Vec<_> = status_columns
                .iter()
                .copied()
                .filter(|(name, _)| props.contains_key(*name))
                .collect();
            match (status_columns.as_slice(), explicit_status.as_slice()) {
                ([choice], _) | (_, [choice]) => *choice,
                _ => bail!("Cannot choose a status property. Specify a single status property in properties."),
            }
        } else {
            let select_columns: Vec<_> = columns
                .iter()
                .copied()
                .filter(|(_, kind)| *kind == "select")
                .collect();
            let explicit_select: Vec<_> = select_columns
                .iter()
                .copied()
                .filter(|(name, _)| props.contains_key(*name))
                .collect();
            match (select_columns.as_slice(), explicit_select.as_slice()) {
                ([choice], _) | (_, [choice]) => *choice,
                _ => bail!("Cannot choose a status or select property. Specify the property in properties."),
            }
        };
        let (name, kind) = choice;
        if let Some(existing) = props.get(name) {
            if !explicit_property_compatible_with_status(existing, kind, status) {
                bail!(
                    "Conflicting value for property '{name}': explicit properties value does not match --status '{status}'"
                );
            }
        }
        props.insert(name.to_owned(), json!({ (kind): { "name": status } }));
    }
    Ok(props)
}

pub async fn op_create_database_item(
    client: &NotionClient,
    storage: &Storage,
    database: &str,
    title: &str,
    status: Option<&str>,
    extra_properties: Option<Value>,
    body: Option<&str>,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let db_id = resolver::resolve_database_id_for_write(storage, client, &ws_id, database).await?;
    let mut used_cached_schema = false;
    let cached_db = {
        let ws_c = ws_id.clone();
        let db_id_c = db_id.clone();
        storage
            .blocking(move |st| st.get_database(&ws_c, &db_id_c))
            .await?
            .filter(|db| !db.schema.is_empty())
    };
    let props = if let Some(cached_db) = cached_db {
        let cols = cached_db
            .schema
            .iter()
            .map(|(name, prop)| (name.as_str(), prop.prop_type.as_str()));
        match database_item_properties(cols, Some(title), status, extra_properties.clone()) {
            Ok(p) => {
                used_cached_schema = true;
                p
            }
            Err(_) => {
                let db = op_get_database(client, storage, &db_id, false).await?;
                let cols = db
                    .schema
                    .iter()
                    .map(|(name, prop)| (name.as_str(), prop.prop_type.as_str()));
                database_item_properties(cols, Some(title), status, extra_properties.clone())?
            }
        }
    } else {
        let db = op_get_database(client, storage, &db_id, false).await?;
        let cols = db
            .schema
            .iter()
            .map(|(name, prop)| (name.as_str(), prop.prop_type.as_str()));
        database_item_properties(cols, Some(title), status, extra_properties.clone())?
    };

    match op_create_page(
        client,
        storage,
        &db_id,
        title,
        None,
        body,
        true,
        Some(Value::Object(props)),
    )
    .await
    {
        Ok(created) => Ok(created),
        Err(err) if used_cached_schema && is_notion_validation_error(&err) => {
            let db = op_get_database(client, storage, &db_id, false).await?;
            let cols = db
                .schema
                .iter()
                .map(|(name, prop)| (name.as_str(), prop.prop_type.as_str()));
            let fresh_props =
                database_item_properties(cols, Some(title), status, extra_properties)?;
            op_create_page(
                client,
                storage,
                &db_id,
                title,
                None,
                body,
                true,
                Some(Value::Object(fresh_props)),
            )
            .await
        }
        Err(err) => Err(err),
    }
}

pub async fn op_update_database_item(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    title: Option<&str>,
    status: Option<&str>,
    extra_properties: Option<Value>,
) -> Result<NotionPage> {
    let ws_id = client.auth().active_workspace_id().await;
    let id = resolver::resolve_page_id_for_write(storage, client, &ws_id, page_id, false).await?;
    let status = status.map(str::trim).filter(|s| !s.is_empty());
    let mut used_cached_meta = false;
    let props = if title.is_some() || status.is_some() {
        let cached_page = {
            let ws_c = ws_id.clone();
            let id_c = id.clone();
            storage.blocking(move |st| st.get_page(&ws_c, &id_c)).await?
        };
        let cached_props = cached_page
            .filter(|p| !p.properties.is_empty())
            .and_then(|page| {
                let cols = page_property_columns(&page);
                database_item_properties(cols.into_iter(), title, status, extra_properties.clone())
                    .ok()
            });
        if let Some(p) = cached_props {
            used_cached_meta = true;
            p
        } else {
            let page = client.get_page(&id).await?;
            let page_c = page.clone();
            let _ = storage.blocking(move |st| st.upsert_page(&page_c)).await;
            let cols = page_property_columns(&page);
            database_item_properties(cols.into_iter(), title, status, extra_properties.clone())?
        }
    } else {
        property_map(extra_properties.clone())?
    };
    if props.is_empty() {
        bail!("Provide a title, status, or properties to update");
    }
    let updated = match client
        .update_page(&id, json!({ "properties": Value::Object(props) }))
        .await
    {
        Ok(u) => u,
        Err(err) if used_cached_meta && is_notion_validation_error(&err) => {
            let page = client.get_page(&id).await?;
            let page_c = page.clone();
            let _ = storage.blocking(move |st| st.upsert_page(&page_c)).await;
            let cols = page_property_columns(&page);
            let fresh_props =
                database_item_properties(cols.into_iter(), title, status, extra_properties)?;
            client
                .update_page(&id, json!({ "properties": Value::Object(fresh_props) }))
                .await?
        }
        Err(err) => return Err(err),
    };
    let updated_c = updated.clone();
    let _ = storage.blocking(move |st| st.upsert_page(&updated_c)).await;
    Ok(updated)
}

