//! Background Notion workspace sync engine (`src/sync.rs`):
//! - Syncs workspace tree (pages & databases via `POST /v1/search`) into SQLite WAL + FTS5
//! - Polls active page (blocks + comments) or active database rows based on TUI `FocusTarget`
//! - Exposes `SyncController` for on-demand refresh nudges and clean shutdown.

use std::{
    collections::{BTreeMap, HashSet},
    io::BufWriter,
    time::Duration,
};

use anyhow::{Context, Result};
use serde::{ser::SerializeMap, Deserialize, Serialize, Serializer};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::{
    config::AppConfig,
    models::{
        CoreEvent, NotionBlock, NotionDatabase, NotionPage, NotionPageDetail, NotionPageSummary,
        NotionPropertyValue, POLL_RETRY_SUMMARY,
    },
    notion::{NotionClient, SearchFilterObject},
    parser::{blocks_to_markdown, collect_media_refs},
    storage::Storage,
};

/// Controls whether `sync_page_detail` may reuse cached blocks/comments when
/// `detail_synced_edited_time` matches the server's `last_edited_time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailSyncMode {
    /// Reuse cached body when `detail_synced_edited_time == page.last_edited_time`.
    Conditional,
    /// Fetch blocks and comments directly regardless of parent `last_edited_time`.
    Force,
}

/// Active TUI target used to prioritize background sync polling.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub page_id: Option<String>,
    pub database_id: Option<String>,
}

/// On-demand sync commands accepted by the sync engine and IPC daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncCommand {
    RefreshAll,
    RefreshTree,
    RefreshPage { page_id: String },
    RefreshDatabase { database_id: String },
    NotifyUserActivity,
    Shutdown,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    cmd_tx: mpsc::UnboundedSender<SyncCommand>,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        cmd_tx: mpsc::UnboundedSender<SyncCommand>,
    ) -> Self {
        Self { focus_tx, cmd_tx }
    }

    pub fn set_focus(&self, target: FocusTarget) {
        let _ = self.focus_tx.send(target);
    }

    pub fn request(&self, cmd: SyncCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn notify_activity(&self) {
        self.request(SyncCommand::NotifyUserActivity);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyncOutcome {
    /// Objects received from Notion Search and upserted in this run.
    pub fetched: usize,
}

struct StreamedSearch {
    fetched: usize,
    newest_edited: Option<String>,
    /// Page and database IDs seen in this pass; collected only for a full rescan.
    seen: Option<(HashSet<String>, HashSet<String>)>,
}

async fn stream_workspace_search<F>(
    client: &NotionClient,
    storage: &Storage,
    watermark: Option<&str>,
    collect_seen: bool,
    on_batch: &mut F,
) -> std::result::Result<StreamedSearch, (usize, anyhow::Error)>
where
    F: FnMut(usize, usize, usize),
{
    let mut cursor: Option<String> = None;
    let mut pages_total = 0usize;
    let mut dbs_total = 0usize;
    let mut batch_num = 0usize;
    let mut newest_edited: Option<String> = None;
    let mut seen = collect_seen.then(|| (HashSet::new(), HashSet::new()));

    loop {
        let fetched_so_far = pages_total + dbs_total;
        batch_num += 1;
        let batch = client
            .search(None, SearchFilterObject::All, 100, cursor.as_deref())
            .await
            .map_err(|err| (fetched_so_far, err))?;

        for p in &batch.pages {
            if let Some(ts) = p.last_edited_time.as_deref().filter(|s| !s.is_empty()) {
                if newest_edited.as_deref().map_or(true, |cur| ts > cur) {
                    newest_edited = Some(ts.to_string());
                }
            }
            if let Some((ref mut seen_pages, _)) = seen {
                seen_pages.insert(p.id.clone());
            }
        }
        for d in &batch.databases {
            if let Some(ts) = d.last_edited_time.as_deref().filter(|s| !s.is_empty()) {
                if newest_edited.as_deref().map_or(true, |cur| ts > cur) {
                    newest_edited = Some(ts.to_string());
                }
            }
            if let Some((_, ref mut seen_dbs)) = seen {
                seen_dbs.insert(d.id.clone());
            }
        }

        let hit_watermark = if let Some(wm) = watermark.filter(|s| !s.is_empty()) {
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

        let batch_pages_len = batch.pages.len();
        let batch_dbs_len = batch.databases.len();
        let batch_pages = batch.pages;
        let batch_dbs = batch.databases;
        let st = storage.clone();
        let upsert_res = tokio::task::spawn_blocking(move || -> Result<()> {
            st.upsert_pages(&batch_pages)?;
            st.upsert_databases(&batch_dbs)?;
            Ok(())
        })
        .await
        .map_err(|err| (fetched_so_far, anyhow::Error::from(err)))
        .and_then(|inner| inner.map_err(|err| (fetched_so_far, err)));
        upsert_res?;

        pages_total += batch_pages_len;
        dbs_total += batch_dbs_len;
        let fetched = pages_total + dbs_total;
        on_batch(batch_num, pages_total, dbs_total);

        if hit_watermark || !batch.has_more {
            return Ok(StreamedSearch {
                fetched,
                newest_edited,
                seen,
            });
        }
        let next = match batch.next_cursor.as_deref().filter(|c| !c.is_empty()) {
            Some(c) => c,
            None => {
                return Err((
                    fetched,
                    anyhow::anyhow!("Notion search response is missing its next cursor"),
                ));
            }
        };
        if cursor.as_deref() == Some(next) {
            return Err((
                fetched,
                anyhow::anyhow!("Notion search response repeated its pagination cursor"),
            ));
        }
        cursor = Some(next.to_owned());
    }
}

/// Performs an incremental workspace tree sync (`POST /v1/search`) into SQLite.
/// Uses the stored `last_edited_time` watermark to stop early on unchanged batches.
pub async fn sync_workspace_tree(
    client: &NotionClient,
    storage: &Storage,
) -> Result<(String, Vec<NotionPage>, Vec<NotionDatabase>)> {
    let (workspace_id, pages, databases, _) =
        sync_workspace_tree_with_options(client, storage, false, |_, _, _| {}).await?;
    Ok((workspace_id, pages, databases))
}

/// Performs a workspace tree sync with optional full-rescan override and batch progress callback.
pub async fn sync_workspace_tree_with_options<F>(
    client: &NotionClient,
    storage: &Storage,
    full_rescan: bool,
    on_batch: F,
) -> Result<(String, Vec<NotionPage>, Vec<NotionDatabase>, SyncOutcome)>
where
    F: FnMut(usize, usize, usize),
{
    let ((workspace_id, _, databases), outcome) =
        sync_workspace_tree_summaries_with_options(client, storage, full_rescan, on_batch).await?;
    let st = storage.clone();
    let ws = workspace_id.clone();
    let pages = tokio::task::spawn_blocking(move || st.list_pages(&ws, true)).await??;
    Ok((workspace_id, pages, databases, outcome))
}

pub(crate) async fn sync_workspace_tree_summaries(
    client: &NotionClient,
    storage: &Storage,
) -> Result<WorkspaceTree> {
    sync_workspace_tree_summaries_with_options(client, storage, false, |_, _, _| {})
        .await
        .map(|(tree, _)| tree)
}

async fn sync_workspace_tree_summaries_with_options<F>(
    client: &NotionClient,
    storage: &Storage,
    full_rescan: bool,
    mut on_batch: F,
) -> Result<(WorkspaceTree, SyncOutcome)>
where
    F: FnMut(usize, usize, usize),
{
    let workspace_id = client.auth().active_workspace_id().await;
    let scope_key = format!("workspace:{workspace_id}");
    let complete_key = format!("workspace-complete:{workspace_id}");

    let (stored_wm, complete) = {
        let st_wm = storage.clone();
        let sk = scope_key.clone();
        let ck = complete_key.clone();
        tokio::task::spawn_blocking(move || {
            let wm = st_wm.get_sync_cursor(&sk);
            let done = st_wm.get_sync_cursor(&ck).is_some();
            (wm, done)
        })
        .await
        .unwrap_or((None, false))
    };

    let watermark = if full_rescan || !complete {
        None
    } else {
        stored_wm.clone()
    };

    let streamed = match stream_workspace_search(
        client,
        storage,
        watermark.as_deref(),
        full_rescan,
        &mut on_batch,
    )
    .await
    {
        Ok(s) => s,
        Err((fetched, err)) => {
            return Err(err).with_context(|| {
                format!(
                    "Workspace sync stopped after {fetched} objects; they stay cached and the next sync starts again from the newest item"
                )
            });
        }
    };

    let newest = match (stored_wm.as_deref(), streamed.newest_edited.as_deref()) {
        (Some(a), Some(b)) => Some(if b > a { b.to_string() } else { a.to_string() }),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(b)) => Some(b.to_string()),
        (None, None) => None,
    };

    let seen_ids = streamed.seen;
    let outcome = SyncOutcome {
        fetched: streamed.fetched,
    };

    let st = storage.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        st.set_sync_cursor(&scope_key, newest.as_deref())?;
        st.set_sync_cursor(&complete_key, Some("1"))?;
        Ok(())
    })
    .await??;

    // Notion Search is not guaranteed to enumerate every accessible object even when `has_more=false`.
    // Never bulk-archive items solely because they are absent from Search. Only on an explicit
    // `full_rescan`, verify a bounded set of missing candidates individually via direct GET by ID
    // and hide entries solely on authoritative `archived`/`in_trash` or HTTP 404 `object_not_found`.
    if let Some((seen_page_ids, seen_db_ids)) = seen_ids {
        const MAX_FULL_RECONCILE_PROBES: usize = 32;

        let st_cand = storage.clone();
        let ws_cand = workspace_id.clone();
        let (missing_page_ids, missing_db_ids) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<String>, Vec<String>)> {
                let mut missing_pages: Vec<String> = st_cand
                    .list_page_summaries(&ws_cand)?
                    .into_iter()
                    .filter(|p| !p.archived && !seen_page_ids.contains(&p.id))
                    .map(|p| p.id)
                    .collect();
                let mut missing_dbs: Vec<String> = st_cand
                    .list_databases(&ws_cand)?
                    .into_iter()
                    .filter(|d| !seen_db_ids.contains(&d.id))
                    .map(|d| d.id)
                    .collect();
                let db_budget = missing_dbs.len().min(MAX_FULL_RECONCILE_PROBES);
                missing_dbs.truncate(db_budget);
                let page_budget = MAX_FULL_RECONCILE_PROBES.saturating_sub(missing_dbs.len());
                missing_pages.truncate(page_budget);
                Ok((missing_pages, missing_dbs))
            })
            .await??;

        let mut confirmed_pages: Vec<NotionPage> = Vec::new();
        let mut not_found_pages: Vec<String> = Vec::new();
        for chunk in missing_page_ids.chunks(3) {
            let futs = chunk.iter().map(|pid| async move {
                let res = client.get_page(pid).await;
                (pid.clone(), res)
            });
            for (pid, probe_res) in futures::future::join_all(futs).await {
                match probe_res {
                    Ok(page) => confirmed_pages.push(page),
                    Err(err) if NotionClient::is_authoritative_not_found(&err) => {
                        not_found_pages.push(pid);
                    }
                    Err(_) => {
                        // Preserve cached entry & FTS on transient 5xx / 429 / ambiguous errors.
                    }
                }
            }
        }

        let mut confirmed_dbs: Vec<NotionDatabase> = Vec::new();
        let mut not_found_dbs: Vec<String> = Vec::new();
        for chunk in missing_db_ids.chunks(3) {
            let futs = chunk.iter().map(|did| async move {
                let res = client.get_database(did).await;
                (did.clone(), res)
            });
            for (did, probe_res) in futures::future::join_all(futs).await {
                match probe_res {
                    Ok(db) => confirmed_dbs.push(db),
                    Err(err) if NotionClient::is_authoritative_not_found(&err) => {
                        not_found_dbs.push(did);
                    }
                    Err(_) => {
                        // Preserve cached entry & FTS on transient 5xx / 429 / ambiguous errors.
                    }
                }
            }
        }

        if !confirmed_pages.is_empty()
            || !not_found_pages.is_empty()
            || !confirmed_dbs.is_empty()
            || !not_found_dbs.is_empty()
        {
            let st_apply = storage.clone();
            let ws_apply = workspace_id.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                st_apply.upsert_pages(&confirmed_pages)?;
                for pid in &not_found_pages {
                    st_apply.mark_page_archived(&ws_apply, pid)?;
                }
                st_apply.upsert_databases(&confirmed_dbs)?;
                for did in &not_found_dbs {
                    st_apply.mark_database_archived(&ws_apply, did)?;
                }
                Ok(())
            })
            .await??;
        }
    }

    let st_read = storage.clone();
    let ws_read = workspace_id.clone();
    let (pages, databases, existing_owners) = tokio::task::spawn_blocking(move || -> Result<_> {
        let pages = st_read.list_page_summaries(&ws_read)?;
        let databases = st_read.list_databases(&ws_read)?;
        let (owners, _) = st_read
            .load_page_parent_hints(&ws_read)
            .unwrap_or_default();
        Ok((pages, databases, owners))
    })
    .await??;

    // Resolve `parent_type == "block_id"` container blocks up to their owning page (capped at 25/sync)
    // so sub-pages nested inside toggleable headings/toggles/columns appear under their parent page.
    let mut unresolved_blocks: Vec<String> = Vec::new();
    let mut seen_blocks = std::collections::HashSet::new();
    for p in &pages {
        if p.archived || p.parent_type != "block_id" {
            continue;
        }
        if existing_owners.contains_key(&p.id) {
            continue;
        }
        if let Some(ref bid) = p.parent_id {
            if !bid.trim().is_empty()
                && !existing_owners.contains_key(bid)
                && seen_blocks.insert(bid.clone())
            {
                unresolved_blocks.push(bid.clone());
                if unresolved_blocks.len() >= 25 {
                    break;
                }
            }
        }
    }

    if !unresolved_blocks.is_empty() {
        let mut resolved_map = existing_owners;
        let mut newly_resolved: Vec<(String, String)> = Vec::new();

        for orig_bid in unresolved_blocks {
            if resolved_map.contains_key(&orig_bid) {
                continue;
            }
            let mut cur_id = orig_bid.clone();
            let mut visited_hops: Vec<String> = vec![cur_id.clone()];
            let mut found_owner: Option<String> = None;

            for _ in 0..6 {
                let Ok((ptype, pid)) = client.get_block_parent_raw(&cur_id).await else {
                    break;
                };
                match (ptype.as_str(), pid) {
                    ("page_id", Some(owner_page_id)) if !owner_page_id.trim().is_empty() => {
                        found_owner = Some(owner_page_id);
                        break;
                    }
                    ("block_id", Some(next_bid))
                        if !next_bid.trim().is_empty()
                            && !visited_hops.iter().any(|h| h == &next_bid) =>
                    {
                        if let Some(cached_owner) = resolved_map.get(&next_bid).cloned() {
                            found_owner = Some(cached_owner);
                            break;
                        }
                        visited_hops.push(next_bid.clone());
                        cur_id = next_bid;
                    }
                    _ => break,
                }
            }

            if let Some(owner_page_id) = found_owner {
                for hop_id in visited_hops {
                    if !resolved_map.contains_key(&hop_id) {
                        resolved_map.insert(hop_id.clone(), owner_page_id.clone());
                        newly_resolved.push((hop_id, owner_page_id.clone()));
                    }
                }
            }
        }

        if !newly_resolved.is_empty() {
            let st_owners = storage.clone();
            let ws_owners = workspace_id.clone();
            let _ = tokio::task::spawn_blocking(move || {
                st_owners.upsert_block_owners(&ws_owners, &newly_resolved)
            })
            .await;
        }
    }

    Ok(((workspace_id, pages, databases), outcome))
}

/// Syncs a single page's metadata, recursive block tree, and comments into SQLite.
/// Skips re-fetching blocks when the cached page's `detail_synced_edited_time` already matches Notion.
pub async fn sync_page_detail(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
) -> Result<crate::models::NotionPageDetail> {
    sync_page_detail_with_mode(client, storage, page_id, DetailSyncMode::Conditional).await
}

/// Explicitly forces a fresh fetch of a page's metadata, block tree, and comments,
/// catching nested-block and comment updates even when the parent page timestamp did not change.
pub async fn sync_page_detail_force(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
) -> Result<crate::models::NotionPageDetail> {
    sync_page_detail_with_mode(client, storage, page_id, DetailSyncMode::Force).await
}

/// Page metadata + blocks for compact reads. Issues no /v1/comments request and never writes comment state.
pub async fn sync_page_body(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    force: bool,
) -> Result<crate::models::NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = crate::models::normalize_notion_id(page_id);

    let (page, blocks) = if !force {
        let st_check = storage.clone();
        let ws_check = ws_id.clone();
        let id_check = norm_id.clone();
        let (cached, has_body, detail_synced_ts, body_synced_ts) =
            tokio::task::spawn_blocking(move || {
                let cached = st_check
                    .load_page_detail(&ws_check, &id_check)
                    .ok()
                    .flatten();
                let has_body = st_check
                    .load_cached_children(&ws_check, &id_check)
                    .ok()
                    .flatten()
                    .is_some();
                let detail_ts = st_check
                    .get_page_detail_synced_edited_time(&ws_check, &id_check)
                    .ok()
                    .flatten();
                let body_ts = st_check
                    .get_page_body_synced_edited_time(&ws_check, &id_check)
                    .ok()
                    .flatten();
                (cached, has_body, detail_ts, body_ts)
            })
            .await
            .unwrap_or((None, false, None, None));

        if let Some(mut cached_detail) =
            cached.filter(|_| has_body && (detail_synced_ts.is_some() || body_synced_ts.is_some()))
        {
            let mut fresh_page = match client.get_page(&norm_id).await {
                Ok(p) => p,
                Err(err) => {
                    if NotionClient::is_authoritative_not_found(&err) {
                        let st_nf = storage.clone();
                        let ws_nf = ws_id.clone();
                        let id_nf = norm_id.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st_nf.mark_page_archived(&ws_nf, &id_nf)
                        })
                        .await;
                    }
                    return Err(err);
                }
            };
            let matched = fresh_page.last_edited_time.as_deref().is_some_and(|ts| {
                detail_synced_ts.as_deref() == Some(ts) || body_synced_ts.as_deref() == Some(ts)
            });
            if matched {
                if fresh_page.plain_text.trim().is_empty() {
                    fresh_page.plain_text = cached_detail.page.plain_text.clone();
                }
                let meta_changed = cached_detail.page.title != fresh_page.title
                    || cached_detail.page.icon != fresh_page.icon
                    || cached_detail.page.archived != fresh_page.archived
                    || cached_detail.page.properties != fresh_page.properties
                    || cached_detail.page.parent_id != fresh_page.parent_id
                    || cached_detail.page.parent_type != fresh_page.parent_type;
                cached_detail.page = fresh_page;
                if meta_changed {
                    let st_meta = storage.clone();
                    let page_meta = cached_detail.page.clone();
                    let _ = tokio::task::spawn_blocking(move || st_meta.upsert_page(&page_meta))
                        .await;
                }
                return Ok(NotionPageDetail {
                    page: cached_detail.page,
                    blocks: cached_detail.blocks,
                    comments: Vec::new(),
                    comments_error: None,
                    markdown: cached_detail.markdown,
                    media: cached_detail.media,
                });
            }
            client.fetch_page_body(&norm_id, Some(fresh_page)).await?
        } else {
            match client.fetch_page_body(&norm_id, None).await {
                Ok(pair) => pair,
                Err(err) => {
                    if NotionClient::is_authoritative_not_found(&err) {
                        let st_nf = storage.clone();
                        let ws_nf = ws_id.clone();
                        let id_nf = norm_id.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            st_nf.mark_page_archived(&ws_nf, &id_nf)
                        })
                        .await;
                    }
                    return Err(err);
                }
            }
        }
    } else {
        match client.fetch_page_body(&norm_id, None).await {
            Ok(pair) => pair,
            Err(err) => {
                if NotionClient::is_authoritative_not_found(&err) {
                    let st_nf = storage.clone();
                    let ws_nf = ws_id.clone();
                    let id_nf = norm_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        st_nf.mark_page_archived(&ws_nf, &id_nf)
                    })
                    .await;
                }
                return Err(err);
            }
        }
    };

    let st = storage.clone();
    let page_clone = page.clone();
    let blocks_clone = blocks.clone();
    tokio::task::spawn_blocking(move || st.save_page_body(&page_clone, &blocks_clone)).await??;

    let markdown = blocks_to_markdown(&blocks);
    let media = collect_media_refs(&blocks);
    Ok(NotionPageDetail {
        page,
        blocks,
        comments: Vec::new(),
        comments_error: None,
        markdown,
        media,
    })
}

/// Syncs a single page detail using `mode`.
pub async fn sync_page_detail_with_mode(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    mode: DetailSyncMode,
) -> Result<crate::models::NotionPageDetail> {
    sync_page_detail_with_mode_tracked(client, storage, page_id, mode)
        .await
        .map(|(detail, _)| detail)
}

async fn sync_page_detail_with_mode_tracked(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    mode: DetailSyncMode,
) -> Result<(crate::models::NotionPageDetail, bool)> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = crate::models::normalize_notion_id(page_id);

    if mode == DetailSyncMode::Force {
        return fetch_and_save_forced_page_detail(client, storage, &norm_id, false)
            .await
            .map(|detail| (detail, true));
    }

    let st_check = storage.clone();
    let ws_check = ws_id.clone();
    let id_check = norm_id.clone();
    let (cached, detail_synced_ts) = tokio::task::spawn_blocking(move || {
        let cached = st_check
            .load_page_detail(&ws_check, &id_check)
            .ok()
            .flatten();
        let synced_ts = st_check
            .get_page_detail_synced_edited_time(&ws_check, &id_check)
            .ok()
            .flatten();
        (cached, synced_ts)
    })
    .await
    .unwrap_or((None, None));

    let detail = if let (Some(mut cached_detail), Some(synced_ts)) = (cached, detail_synced_ts) {
        let mut fresh_page = match client.get_page(&norm_id).await {
            Ok(p) => p,
            Err(err) => {
                if NotionClient::is_authoritative_not_found(&err) {
                    let st_nf = storage.clone();
                    let ws_nf = ws_id.clone();
                    let id_nf = norm_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        st_nf.mark_page_archived(&ws_nf, &id_nf)
                    })
                    .await;
                }
                return Err(err);
            }
        };
        if fresh_page.last_edited_time.as_deref() == Some(synced_ts.as_str()) {
            // Body version matches: reuse cached blocks & comments (0 block/comment GETs),
            // while still applying any metadata changes (title/icon/properties/archived).
            if fresh_page.plain_text.trim().is_empty() {
                fresh_page.plain_text = cached_detail.page.plain_text.clone();
            }
            let meta_changed = cached_detail.page.title != fresh_page.title
                || cached_detail.page.icon != fresh_page.icon
                || cached_detail.page.archived != fresh_page.archived
                || cached_detail.page.properties != fresh_page.properties
                || cached_detail.page.parent_id != fresh_page.parent_id
                || cached_detail.page.parent_type != fresh_page.parent_type;
            cached_detail.page = fresh_page;
            if meta_changed {
                let st_meta = storage.clone();
                let page_meta = cached_detail.page.clone();
                let _ = tokio::task::spawn_blocking(move || st_meta.upsert_page(&page_meta)).await;
            }
            return Ok((cached_detail, false));
        }
        client.fetch_page_detail_with_page(fresh_page).await?
    } else {
        // Cold cache, migrated pre-v14 row (`detail_synced_edited_time IS NULL`), or invalidated detail.
        client.fetch_page_detail(&norm_id).await?
    };

    let st = storage.clone();
    let detail_clone = detail.clone();
    tokio::task::spawn_blocking(move || st.save_page_detail(&detail_clone)).await??;
    Ok((detail, true))
}

async fn fetch_and_save_forced_page_detail(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    check_current: bool,
) -> Result<NotionPageDetail> {
    let detail = match client.fetch_page_detail(page_id).await {
        Ok(detail) => detail,
        Err(err) => {
            if NotionClient::is_authoritative_not_found(&err) {
                let st = storage.clone();
                let ws_id = client.auth().active_workspace_id().await;
                let id = page_id.to_owned();
                let _ = tokio::task::spawn_blocking(move || st.mark_page_archived(&ws_id, &id)).await;
            }
            return Err(err);
        }
    };
    let st = storage.clone();
    tokio::task::spawn_blocking(move || {
        if check_current {
            st.save_page_detail_if_changed(&detail)?;
        } else {
            st.save_page_detail(&detail)?;
        }
        Ok(detail)
    })
    .await?
}


/// Syncs a database schema and all its rows into SQLite (in parallel).
pub async fn sync_database_rows(
    client: &NotionClient,
    storage: &Storage,
    database_id: &str,
) -> Result<(Option<NotionDatabase>, Vec<NotionPage>)> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_db_id = crate::models::normalize_notion_id(database_id);
    let (db_res, rows_res) = tokio::join!(
        client.get_database(&norm_db_id),
        client.query_database_all_with_meta(&norm_db_id, None, None, 250, None),
    );
    let db = db_res.ok();
    let (rows, has_more, _) = rows_res?;
    let st = storage.clone();
    let db_clone = db.clone();
    let rows_clone = rows.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(ref d) = db_clone {
            st.upsert_database(d)?;
        }
        st.upsert_pages(&rows_clone)?;
        if !has_more {
            let active_row_ids: HashSet<String> = rows_clone.iter().map(|r| r.id.clone()).collect();
            st.reconcile_database_rows_archived(&ws_id, &norm_db_id, &active_row_ids)?;
        }
        Ok(())
    })
    .await??;
    Ok((db, rows))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActiveTarget {
    Page(String),
    Database(String),
}

impl ActiveTarget {
    fn from_focus(focus: &FocusTarget) -> Option<Self> {
        if let Some(ref pid) = focus.page_id {
            Some(Self::Page(pid.clone()))
        } else {
            focus.database_id.clone().map(Self::Database)
        }
    }
}

struct InFlightActive {
    id: u64,
    target: ActiveTarget,
    mode: DetailSyncMode,
    show_busy: bool,
    emit_unchanged: bool,
    handle: tokio::task::JoinHandle<()>,
}

struct PendingActive {
    target: ActiveTarget,
    mode: DetailSyncMode,
    show_busy: bool,
    emit_unchanged: bool,
}

enum ActiveOutcome {
    Page {
        detail: Box<NotionPageDetail>,
        changed: bool,
        fetched_body: bool,
        http_requests: usize,
    },
    Database {
        database_id: String,
        database: Option<NotionDatabase>,
        rows: Vec<NotionPage>,
        changed: bool,
    },
    Err,
}

struct ActiveDone {
    id: u64,
    outcome: ActiveOutcome,
}

type WorkspaceTree = (String, Vec<NotionPageSummary>, Vec<NotionDatabase>);
type TreeDigest = [u8; 32];

struct TreeDone {
    loud: bool,
    profile_loaded: bool,
    result: Option<(TreeDigest, Option<WorkspaceTree>)>,
}

const DEEP_PAGE_REQUEST_THRESHOLD: usize = 5;
const DEEP_PAGE_MAX_DELAY_MS: u64 = 30_000;

/// Estimates the HTTP request count of a direct page detail refresh (`GET /pages/{id}` +
/// root `GET /blocks/{id}/children` + `GET /comments` + recursive nested container fetches).
pub(crate) fn estimate_page_detail_http_requests(blocks: &[NotionBlock]) -> usize {
    fn count_containers(blocks: &[NotionBlock]) -> usize {
        blocks
            .iter()
            .map(|b| {
                let self_req = usize::from(
                    b.has_children
                        && !matches!(
                            b.block_type.as_str(),
                            "child_page" | "child_database" | "unsupported"
                        ),
                );
                self_req + count_containers(&b.children)
            })
            .sum()
    }
    3 + count_containers(blocks)
}

/// Computes the post-completion delay before the next automatic visible-item check.
/// Small pages use `base_ms`; deep nested pages space checks proportionally to request cost
/// (~1s per HTTP call, capped at 30s unless `base_ms` is larger), always honoring `Retry-After`.
pub(crate) fn compute_next_active_delay(
    base_ms: u64,
    http_requests: usize,
    rate_limit_remaining: Duration,
) -> Duration {
    let paced_ms = if http_requests > DEEP_PAGE_REQUEST_THRESHOLD {
        let scaled = (http_requests as u64)
            .saturating_mul(1_000)
            .min(DEEP_PAGE_MAX_DELAY_MS);
        base_ms.max(scaled)
    } else {
        base_ms
    };
    Duration::from_millis(paced_ms).max(rate_limit_remaining)
}

#[derive(PartialEq)]
struct SemanticPageProperties<'a>(&'a BTreeMap<String, NotionPropertyValue>);

impl Serialize for SemanticPageProperties<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, property) in self.0 {
            if matches!(property, NotionPropertyValue::Number(Some(value)) if *value == 0.0) {
                // The semantic comparison treats positive and negative zero as equal.
                map.serialize_entry(name, &NotionPropertyValue::Number(Some(0.0)))?;
            } else {
                map.serialize_entry(name, property)?;
            }
        }
        map.end()
    }
}

fn page_semantic_fields(page: &NotionPage) -> impl PartialEq + Serialize + '_ {
    let NotionPage {
        id,
        workspace_id,
        parent_type,
        parent_id,
        title,
        icon,
        cover_url,
        url,
        archived,
        properties,
        plain_text,
        created_time,
        last_edited_time,
        is_favorite: _,
        synced_at: _,
    } = page;
    (
        (
            id, workspace_id, parent_type, parent_id, title, icon, cover_url, url, archived,
        ),
        (
            SemanticPageProperties(properties),
            plain_text,
            created_time,
            last_edited_time,
        ),
    )
}

fn database_semantic_fields(database: &NotionDatabase) -> impl PartialEq + Serialize + '_ {
    let NotionDatabase {
        id,
        workspace_id,
        parent_type,
        parent_id,
        title,
        icon,
        description,
        schema,
        url,
        archived,
        last_edited_time,
        is_favorite: _,
        synced_at: _,
    } = database;
    (
        id, workspace_id, parent_type, parent_id, title, icon, description, schema, url, archived,
        last_edited_time,
    )
}

pub(crate) fn page_semantically_equal(a: &NotionPage, b: &NotionPage) -> bool {
    page_semantic_fields(a) == page_semantic_fields(b)
}

pub(crate) fn database_semantically_equal(a: &NotionDatabase, b: &NotionDatabase) -> bool {
    database_semantic_fields(a) == database_semantic_fields(b)
}

pub(crate) fn page_detail_semantically_equal(a: &NotionPageDetail, b: &NotionPageDetail) -> bool {
    page_semantically_equal(&a.page, &b.page)
        && a.blocks == b.blocks
        && a.comments == b.comments
        && a.comments_error == b.comments_error
        && a.markdown == b.markdown
}

fn tree_digest(
    workspace_id: &str,
    pages: &[NotionPageSummary],
    databases: &[NotionDatabase],
) -> Result<TreeDigest> {
    let mut writer = BufWriter::new(Sha256::new());
    // Frame ordered page and database sequences without building another tree or JSON payload.
    serde_json::to_writer(&mut writer, &(workspace_id, pages.len(), databases.len()))?;
    for page in pages {
        serde_json::to_writer(&mut writer, page)?;
    }
    for database in databases {
        serde_json::to_writer(
            &mut writer,
            &(database_semantic_fields(database), database.is_favorite),
        )?;
    }
    let hasher = writer.into_inner().map_err(|err| err.into_error())?;
    Ok(hasher.finalize().into())
}

/// Spawns the background sync loop returning a `SyncController`.
pub fn spawn_sync_engine(
    config: AppConfig,
    client: NotionClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();

    let active_base_ms = config.effective_active_poll_ms().max(3_000);
    let tree_ms = config.sync_poll_ms.max(15_000);

    tokio::spawn(async move {
        let active_sleep = tokio::time::sleep(Duration::from_millis(active_base_ms));
        tokio::pin!(active_sleep);
        let mut active_timer_armed = false;

        let mut tree_tick = tokio::time::interval(Duration::from_millis(tree_ms));
        tree_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume initial immediate tick so bootstrap controls first tree fetch.
        tree_tick.tick().await;

        let (active_done_tx, mut active_done_rx) = mpsc::unbounded_channel::<ActiveDone>();
        let (tree_done_tx, mut tree_done_rx) = mpsc::unbounded_channel::<TreeDone>();

        let mut in_flight_active: Option<InFlightActive> = None;
        let mut pending_active: Option<PendingActive> = None;
        let mut in_flight_tree: Option<tokio::task::JoinHandle<()>> = None;
        let mut pending_tree: Option<bool> = None;
        let mut loud_active_busy = false;
        let mut loud_tree_busy = false;
        let mut retry_active = false;
        let mut profile_loaded = false;
        let mut last_tree_digest: Option<TreeDigest> = None;
        let mut next_active_id: u64 = 0;

        let start_active = |target: ActiveTarget,
                            mode: DetailSyncMode,
                            show_busy: bool,
                            emit_unchanged: bool,
                            in_flight_active: &mut Option<InFlightActive>,
                            pending_active: &mut Option<PendingActive>,
                            loud_active_busy: &mut bool,
                            loud_tree_busy: bool,
                            active_timer_armed: &mut bool,
                            next_active_id: &mut u64| {
            if let Some(cur) = in_flight_active.as_mut() {
                if cur.target == target {
                    if cur.mode == DetailSyncMode::Force || mode == DetailSyncMode::Conditional {
                        // Share the single in-flight network fetch and upgrade delivery/busy semantics.
                        cur.emit_unchanged |= emit_unchanged;
                        if show_busy && !cur.show_busy {
                            cur.show_busy = true;
                            *loud_active_busy = true;
                            let summary = match target {
                                ActiveTarget::Page(_) => "Loading page...",
                                ActiveTarget::Database(_) => "Querying database...",
                            };
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: summary.to_string(),
                            });
                        }
                        if pending_active
                            .as_ref()
                            .is_some_and(|p| p.target != target)
                        {
                            *pending_active = None;
                        }
                        return;
                    }
                    // Upgrade an in-flight Conditional check for the same item to Force if needed.
                    cur.emit_unchanged |= emit_unchanged;
                    let merged_busy = pending_active
                        .as_ref()
                        .filter(|p| p.target == target)
                        .map_or(show_busy, |p| p.show_busy || show_busy);
                    let merged_emit = pending_active
                        .as_ref()
                        .filter(|p| p.target == target)
                        .map_or(emit_unchanged, |p| p.emit_unchanged || emit_unchanged);
                    *pending_active = Some(PendingActive {
                        target,
                        mode: DetailSyncMode::Force,
                        show_busy: merged_busy,
                        emit_unchanged: merged_emit,
                    });
                    return;
                }
                // Target changed: cancel stale in-flight fetch so rapid focus churn never drops the latest target.
                cur.handle.abort();
                let was_busy = cur.show_busy;
                *in_flight_active = None;
                *pending_active = None;
                if was_busy {
                    *loud_active_busy = false;
                    if !show_busy && !loud_tree_busy {
                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                            busy: false,
                            summary: "Synced".to_string(),
                        });
                    }
                }
            }

            *active_timer_armed = false;
            *next_active_id = next_active_id.wrapping_add(1);
            let req_id = *next_active_id;
            if show_busy {
                *loud_active_busy = true;
                let summary = match target {
                    ActiveTarget::Page(_) => "Loading page...",
                    ActiveTarget::Database(_) => "Querying database...",
                };
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: true,
                    summary: summary.to_string(),
                });
            }

            let c = client.clone();
            let s = storage.clone();
            let tx = event_tx.clone();
            let done_tx = active_done_tx.clone();
            let target_spawn = target.clone();
            let handle = tokio::spawn(async move {
                let outcome = match target_spawn {
                    ActiveTarget::Page(ref pid) => {
                        run_page_refresh(&c, &s, &tx, pid, mode, emit_unchanged).await
                    }
                    ActiveTarget::Database(ref did) => {
                        run_db_refresh(&c, &s, &tx, did, emit_unchanged).await
                    }
                };
                let _ = done_tx.send(ActiveDone {
                    id: req_id,
                    outcome,
                });
            });

            *in_flight_active = Some(InFlightActive {
                id: req_id,
                target,
                mode,
                show_busy,
                emit_unchanged,
                handle,
            });
        };

        let start_tree = |loud: bool,
                          fetch_profile: bool,
                          previous_digest: Option<TreeDigest>,
                          in_flight_tree: &mut Option<tokio::task::JoinHandle<()>>,
                          pending_tree: &mut Option<bool>,
                          loud_tree_busy: &mut bool| {
            if in_flight_tree.is_some() {
                let merged_loud = pending_tree.unwrap_or(false) || loud;
                *pending_tree = Some(merged_loud);
                return;
            }
            if loud {
                *loud_tree_busy = true;
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: true,
                    summary: "Syncing workspace...".to_string(),
                });
            }
            let c = client.clone();
            let s = storage.clone();
            let tx = event_tx.clone();
            let done_tx = tree_done_tx.clone();
            let handle = tokio::spawn(async move {
                let done =
                    run_tree_refresh(&c, &s, &tx, loud, fetch_profile, previous_digest).await;
                let _ = done_tx.send(done);
            });
            *in_flight_tree = Some(handle);
        };

        loop {
            tokio::select! {
                biased;
                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        None | Some(SyncCommand::Shutdown) => break,
                        Some(SyncCommand::NotifyUserActivity) => {}
                        Some(SyncCommand::RefreshAll) => {
                            start_tree(
                                true,
                                true,
                                None,
                                &mut in_flight_tree,
                                &mut pending_tree,
                                &mut loud_tree_busy,
                            );
                            // Consume any pending focus change so RefreshAll fetches the
                            // active page's block tree exactly once (no duplicate detail fetch).
                            let focus = focus_rx.borrow_and_update().clone();
                            if let Some(target) = ActiveTarget::from_focus(&focus) {
                                start_active(
                                    target,
                                    DetailSyncMode::Force,
                                    true,
                                    true,
                                    &mut in_flight_active,
                                    &mut pending_active,
                                    &mut loud_active_busy,
                                    loud_tree_busy,
                                    &mut active_timer_armed,
                                    &mut next_active_id,
                                );
                            }
                        }
                        Some(SyncCommand::RefreshTree) => {
                            start_tree(
                                true,
                                !profile_loaded,
                                None,
                                &mut in_flight_tree,
                                &mut pending_tree,
                                &mut loud_tree_busy,
                            );
                        }
                        Some(SyncCommand::RefreshPage { page_id }) => {
                            if focus_rx.has_changed().unwrap_or(false)
                                && focus_rx.borrow().page_id.as_deref() == Some(page_id.as_str())
                            {
                                let _ = focus_rx.borrow_and_update();
                            }
                            start_active(
                                ActiveTarget::Page(page_id),
                                DetailSyncMode::Force,
                                false,
                                true,
                                &mut in_flight_active,
                                &mut pending_active,
                                &mut loud_active_busy,
                                loud_tree_busy,
                                &mut active_timer_armed,
                                &mut next_active_id,
                            );
                        }
                        Some(SyncCommand::RefreshDatabase { database_id }) => {
                            if focus_rx.has_changed().unwrap_or(false)
                                && focus_rx.borrow().database_id.as_deref() == Some(database_id.as_str())
                            {
                                let _ = focus_rx.borrow_and_update();
                            }
                            start_active(
                                ActiveTarget::Database(database_id),
                                DetailSyncMode::Force,
                                false,
                                true,
                                &mut in_flight_active,
                                &mut pending_active,
                                &mut loud_active_busy,
                                loud_tree_busy,
                                &mut active_timer_armed,
                                &mut next_active_id,
                            );
                        }
                    }
                }
                Ok(()) = focus_rx.changed() => {
                    let focus = focus_rx.borrow_and_update().clone();
                    // Coalesce a back-to-back RefreshPage/RefreshDatabase/RefreshAll sent alongside
                    // set_focus so a single page open or attach never fetches the block tree twice.
                    let mut deferred_cmd = None;
                    let mut mode = DetailSyncMode::Conditional;
                    if let Ok(cmd) = cmd_rx.try_recv() {
                        match (&cmd, &focus.page_id, &focus.database_id) {
                            (SyncCommand::RefreshPage { page_id }, Some(fp), _) if page_id == fp => {
                                mode = DetailSyncMode::Force;
                            }
                            (SyncCommand::RefreshDatabase { database_id }, _, Some(fd)) if database_id == fd => {
                                mode = DetailSyncMode::Force;
                            }
                            _ => {
                                deferred_cmd = Some(cmd);
                            }
                        }
                    }
                    if let Some(SyncCommand::RefreshAll) = deferred_cmd {
                        start_tree(
                            true,
                            true,
                            None,
                            &mut in_flight_tree,
                            &mut pending_tree,
                            &mut loud_tree_busy,
                        );
                        let latest_focus = focus_rx.borrow_and_update().clone();
                        if let Some(target) = ActiveTarget::from_focus(&latest_focus) {
                            start_active(
                                target,
                                DetailSyncMode::Force,
                                true,
                                true,
                                &mut in_flight_active,
                                &mut pending_active,
                                &mut loud_active_busy,
                                loud_tree_busy,
                                &mut active_timer_armed,
                                &mut next_active_id,
                            );
                        }
                    } else {
                        if let Some(target) = ActiveTarget::from_focus(&focus) {
                            start_active(
                                target,
                                mode,
                                false,
                                true,
                                &mut in_flight_active,
                                &mut pending_active,
                                &mut loud_active_busy,
                                loud_tree_busy,
                                &mut active_timer_armed,
                                &mut next_active_id,
                            );
                        } else {
                            // Focus cleared (e.g., last daemon TUI client detached): stop active polling.
                            if let Some(cur) = in_flight_active.take() {
                                cur.handle.abort();
                                if cur.show_busy {
                                    loud_active_busy = false;
                                    if !loud_tree_busy {
                                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                            }
                            pending_active = None;
                            active_timer_armed = false;
                        }
                        if let Some(other) = deferred_cmd {
                            match other {
                                SyncCommand::Shutdown => break,
                                SyncCommand::NotifyUserActivity => {}
                                SyncCommand::RefreshTree => {
                                    start_tree(
                                        true,
                                        !profile_loaded,
                                        None,
                                        &mut in_flight_tree,
                                        &mut pending_tree,
                                        &mut loud_tree_busy,
                                    );
                                }
                                SyncCommand::RefreshPage { page_id } => {
                                    start_active(
                                        ActiveTarget::Page(page_id),
                                        DetailSyncMode::Force,
                                        false,
                                        true,
                                        &mut in_flight_active,
                                        &mut pending_active,
                                        &mut loud_active_busy,
                                        loud_tree_busy,
                                        &mut active_timer_armed,
                                        &mut next_active_id,
                                    );
                                }
                                SyncCommand::RefreshDatabase { database_id } => {
                                    start_active(
                                        ActiveTarget::Database(database_id),
                                        DetailSyncMode::Force,
                                        false,
                                        true,
                                        &mut in_flight_active,
                                        &mut pending_active,
                                        &mut loud_active_busy,
                                        loud_tree_busy,
                                        &mut active_timer_armed,
                                        &mut next_active_id,
                                    );
                                }
                                SyncCommand::RefreshAll => unreachable!(),
                            }
                        }
                    }
                }
                Some(done) = active_done_rx.recv() => {
                    let matches_current = in_flight_active
                        .as_ref()
                        .is_some_and(|cur| cur.id == done.id);
                    if !matches_current {
                        continue;
                    }
                    let cur = in_flight_active.take().unwrap();
                    if cur.show_busy {
                        loud_active_busy = false;
                    }

                    // If a Conditional fetch already fetched a fresh body (e.g. cold cache / changed ts),
                    // it already satisfies a queued Force upgrade for the same target without a second GET.
                    let mut next_pending = pending_active.take();
                    let mut upgraded_emit = cur.emit_unchanged;
                    let mut upgraded_busy = cur.show_busy;
                    if let (
                        ActiveOutcome::Page { fetched_body: true, .. },
                        Some(p),
                    ) = (&done.outcome, next_pending.as_ref())
                    {
                        if p.target == cur.target {
                            upgraded_emit |= p.emit_unchanged;
                            upgraded_busy |= p.show_busy;
                            next_pending = None;
                        }
                    }
                    let will_run_pending_same_target = next_pending
                        .as_ref()
                        .is_some_and(|p| p.target == cur.target);

                    let (ok, http_requests) = match done.outcome {
                        ActiveOutcome::Page {
                            detail,
                            changed,
                            http_requests,
                            ..
                        } => {
                            if !will_run_pending_same_target && (upgraded_emit || changed) {
                                let _ = event_tx.send(CoreEvent::PageDetailUpdated(detail));
                            }
                            (true, http_requests)
                        }
                        ActiveOutcome::Database {
                            database_id,
                            database,
                            rows,
                            changed,
                        } => {
                            if !will_run_pending_same_target && (upgraded_emit || changed) {
                                let _ = event_tx.send(CoreEvent::DatabaseRowsUpdated {
                                    database_id,
                                    database,
                                    rows,
                                });
                            }
                            (true, 2)
                        }
                        ActiveOutcome::Err => (false, 1),
                    };

                    if ok {
                        if (upgraded_busy || retry_active) && !loud_tree_busy && !loud_active_busy {
                            retry_active = false;
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "Synced".to_string(),
                            });
                        }
                    } else {
                        retry_active = true;
                    }

                    if let Some(next) = next_pending {
                        start_active(
                            next.target,
                            next.mode,
                            next.show_busy,
                            next.emit_unchanged,
                            &mut in_flight_active,
                            &mut pending_active,
                            &mut loud_active_busy,
                            loud_tree_busy,
                            &mut active_timer_armed,
                            &mut next_active_id,
                        );
                    } else {
                        let focus = focus_rx.borrow().clone();
                        if ActiveTarget::from_focus(&focus).is_some() {
                            let delay = compute_next_active_delay(
                                active_base_ms,
                                http_requests,
                                client.rate_limit_remaining(),
                            );
                            active_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + delay);
                            active_timer_armed = true;
                        }
                    }
                }
                Some(done) = tree_done_rx.recv() => {
                    in_flight_tree = None;
                    if done.loud {
                        loud_tree_busy = false;
                    }
                    if done.profile_loaded {
                        profile_loaded = true;
                    }
                    if let Some((digest, update)) = done.result {
                        last_tree_digest = Some(digest);
                        if let Some((workspace_id, pages, databases)) = update {
                            let _ = event_tx.send(CoreEvent::WorkspaceTreeUpdated {
                                workspace_id,
                                pages,
                                databases,
                            });
                        }
                        if (done.loud || retry_active) && !loud_tree_busy && !loud_active_busy {
                            retry_active = false;
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: "Synced".to_string(),
                            });
                        }
                    } else {
                        retry_active = true;
                    }
                    if let Some(next_loud) = pending_tree.take() {
                        start_tree(
                            next_loud,
                            next_loud || !profile_loaded,
                            last_tree_digest,
                            &mut in_flight_tree,
                            &mut pending_tree,
                            &mut loud_tree_busy,
                        );
                    }
                }
                _ = &mut active_sleep, if active_timer_armed => {
                    active_timer_armed = false;
                    let focus = focus_rx.borrow().clone();
                    if let Some(target) = ActiveTarget::from_focus(&focus) {
                        start_active(
                            target,
                            DetailSyncMode::Force,
                            false,
                            false,
                            &mut in_flight_active,
                            &mut pending_active,
                            &mut loud_active_busy,
                            loud_tree_busy,
                            &mut active_timer_armed,
                            &mut next_active_id,
                        );
                    }
                }
                _ = tree_tick.tick() => {
                    start_tree(
                        false,
                        !profile_loaded,
                        last_tree_digest,
                        &mut in_flight_tree,
                        &mut pending_tree,
                        &mut loud_tree_busy,
                    );
                }
            }
        }

        if let Some(cur) = in_flight_active.take() {
            cur.handle.abort();
        }
        if let Some(handle) = in_flight_tree.take() {
            handle.abort();
        }
    });

    SyncController::from_channels(focus_tx, cmd_tx)
}

async fn run_tree_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    loud: bool,
    fetch_profile: bool,
    previous_digest: Option<TreeDigest>,
) -> TreeDone {
    let previous_digest = if !loud && previous_digest.is_none() {
        let workspace_id = client.auth().active_workspace_id().await;
        let st = storage.clone();
        tokio::task::spawn_blocking(move || {
            let pages = st.list_page_summaries(&workspace_id).ok()?;
            let databases = st.list_databases(&workspace_id).ok()?;
            tree_digest(&workspace_id, &pages, &databases).ok()
        })
        .await
        .ok()
        .flatten()
    } else {
        previous_digest
    };

    let mut profile_loaded = false;
    if fetch_profile {
        if let Ok(me) = client.get_me().await {
            profile_loaded = true;
            let _ = event_tx.send(CoreEvent::ProfileLoaded(me));
        }
    }
    let result: Result<_> = async {
        let snapshot = sync_workspace_tree_summaries(client, storage).await?;
        tokio::task::spawn_blocking(move || {
            let digest = tree_digest(&snapshot.0, &snapshot.1, &snapshot.2)?;
            let update = (loud || Some(digest) != previous_digest).then_some(snapshot);
            Ok((digest, update))
        })
        .await?
    }
    .await;
    match result {
        Ok(result) => TreeDone {
            loud,
            profile_loaded,
            result: Some(result),
        },
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(err.to_string()));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
            TreeDone {
                loud,
                profile_loaded,
                result: None,
            }
        }
    }
}

async fn run_page_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    page_id: &str,
    mode: DetailSyncMode,
    emit_unchanged: bool,
) -> ActiveOutcome {
    let id = crate::models::normalize_notion_id(page_id);
    // Compare against the pre-fetch detail for delivery. Another process can save
    // the same payload while this request is in flight.
    let prev_cached = if !emit_unchanged {
        let ws_id = client.auth().active_workspace_id().await;
        let st = storage.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || st.load_page_detail(&ws_id, &id).ok().flatten())
            .await
            .ok()
            .flatten()
    } else {
        None
    };

    let result = if mode == DetailSyncMode::Force && !emit_unchanged {
        fetch_and_save_forced_page_detail(client, storage, &id, true)
            .await
            .map(|detail| (detail, true))
    } else {
        sync_page_detail_with_mode_tracked(client, storage, page_id, mode).await
    };

    match result {
        Ok((detail, fetched_body)) => {
            let http_requests = estimate_page_detail_http_requests(&detail.blocks);
            let changed = prev_cached
                .as_ref()
                .map_or(true, |prev| !page_detail_semantically_equal(prev, &detail));
            ActiveOutcome::Page {
                detail: Box::new(detail),
                changed,
                fetched_body,
                http_requests,
            }
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(err.to_string()));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
            ActiveOutcome::Err
        }
    }
}

async fn run_db_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    database_id: &str,
    emit_unchanged: bool,
) -> ActiveOutcome {
    let prev_cached = if !emit_unchanged {
        let ws_id = client.auth().active_workspace_id().await;
        let norm_db_id = crate::models::normalize_notion_id(database_id);
        let st = storage.clone();
        tokio::task::spawn_blocking(move || {
            let db = st
                .list_databases(&ws_id)
                .ok()
                .and_then(|dbs| dbs.into_iter().find(|d| d.id == norm_db_id));
            let rows = st.list_database_rows(&ws_id, &norm_db_id).ok();
            (db, rows)
        })
        .await
        .ok()
    } else {
        None
    };

    match sync_database_rows(client, storage, database_id).await {
        Ok((database, rows)) => {
            let changed = match prev_cached {
                Some((prev_db, Some(prev_rows))) => {
                    let db_same = match (&prev_db, &database) {
                        (Some(a), Some(b)) => database_semantically_equal(a, b),
                        (None, None) => true,
                        _ => false,
                    };
                    let rows_same = prev_rows.len() == rows.len()
                        && prev_rows
                            .iter()
                            .zip(rows.iter())
                            .all(|(a, b)| page_semantically_equal(a, b));
                    !(db_same && rows_same)
                }
                _ => true,
            };
            ActiveOutcome::Database {
                database_id: database_id.to_string(),
                database,
                rows,
                changed,
            }
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(err.to_string()));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
            ActiveOutcome::Err
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_digest_tracks_displayed_metadata_at_the_same_edit_time() {
        let mut page = NotionPage {
            id: "page".into(),
            last_edited_time: Some("2026-11-20T06:00:00Z".into()),
            ..Default::default()
        };
        let mut databases = vec![NotionDatabase {
            id: "db".into(),
            last_edited_time: page.last_edited_time.clone(),
            ..Default::default()
        }];
        let digest = |page: &NotionPage, dbs: &[NotionDatabase]| {
            tree_digest("workspace", &[NotionPageSummary::from(page)], dbs).unwrap()
        };
        let original = digest(&page, &databases);
        page.plain_text = "Changed body".into();
        page.synced_at = Some("local page sync".into());
        databases[0].synced_at = Some("local database sync".into());
        assert_eq!(digest(&page, &databases), original);

        page.title = "Updated title".into();
        assert_ne!(digest(&page, &databases), original);
        page.title.clear();
        page.icon = Some("🔔".into());
        assert_ne!(digest(&page, &databases), original);
        page.icon = None;
        page.is_favorite = true;
        assert_ne!(digest(&page, &databases), original);
        page.is_favorite = false;
        databases[0].description = "Updated description".into();
        assert_ne!(digest(&page, &databases), original);
    }

    #[test]
    fn test_compute_next_active_delay_scales_deep_pages_and_honors_retry_after() {
        // Small page (<= 5 HTTP requests) uses base interval (5s default)
        assert_eq!(
            compute_next_active_delay(5_000, 3, Duration::ZERO),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            compute_next_active_delay(5_000, 5, Duration::ZERO),
            Duration::from_millis(5_000)
        );
        // Deep page scales proportionally (~1s/request, capped at 30s)
        assert_eq!(
            compute_next_active_delay(5_000, 12, Duration::ZERO),
            Duration::from_millis(12_000)
        );
        assert_eq!(
            compute_next_active_delay(5_000, 45, Duration::ZERO),
            Duration::from_millis(30_000)
        );
        // Explicit slower user interval (> 30s) is respected
        assert_eq!(
            compute_next_active_delay(40_000, 20, Duration::ZERO),
            Duration::from_millis(40_000)
        );
        // Rate-limit Retry-After (including >120s) always wins when larger than paced delay
        assert_eq!(
            compute_next_active_delay(5_000, 4, Duration::from_secs(9)),
            Duration::from_secs(9)
        );
        assert_eq!(
            compute_next_active_delay(5_000, 4, Duration::from_secs(240)),
            Duration::from_secs(240)
        );
    }
}
