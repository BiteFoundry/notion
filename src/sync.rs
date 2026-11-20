//! Background Notion workspace sync engine (`src/sync.rs`):
//! - Syncs workspace tree (pages & databases via `POST /v1/search`) into SQLite WAL + FTS5
//! - Polls active page (blocks + comments) or active database rows based on TUI `FocusTarget`
//! - Exposes `SyncController` for on-demand refresh nudges and clean shutdown.

use std::{collections::HashSet, time::Duration};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::{
    config::AppConfig,
    models::{CoreEvent, NotionDatabase, NotionPage, POLL_RETRY_SUMMARY},
    notion::{NotionClient, SearchFilterObject},
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

/// Performs an incremental workspace tree sync (`POST /v1/search`) into SQLite.
/// Uses the stored `last_edited_time` watermark to stop early on unchanged batches.
pub async fn sync_workspace_tree(
    client: &NotionClient,
    storage: &Storage,
) -> Result<(String, Vec<NotionPage>, Vec<NotionDatabase>)> {
    sync_workspace_tree_with_options(client, storage, false, |_, _, _| {}).await
}

/// Performs a workspace tree sync with optional full-rescan override and batch progress callback.
pub async fn sync_workspace_tree_with_options<F>(
    client: &NotionClient,
    storage: &Storage,
    full_rescan: bool,
    on_batch: F,
) -> Result<(String, Vec<NotionPage>, Vec<NotionDatabase>)>
where
    F: FnMut(usize, usize, usize),
{
    let workspace_id = client.auth().active_workspace_id().await;
    let scope_key = format!("workspace:{workspace_id}");

    let watermark = if full_rescan {
        None
    } else {
        let st_wm = storage.clone();
        let sk = scope_key.clone();
        tokio::task::spawn_blocking(move || st_wm.get_sync_cursor(&sk))
            .await
            .ok()
            .flatten()
    };

    let res = client
        .search_incremental(
            None,
            SearchFilterObject::All,
            500,
            watermark.as_deref(),
            on_batch,
        )
        .await?;

    // Compute newest `last_edited_time` across synced items for the next incremental watermark
    let mut newest_edited: Option<String> = watermark;
    for p in &res.pages {
        if let Some(ts) = p.last_edited_time.as_deref().filter(|s| !s.is_empty()) {
            if newest_edited.as_deref().map_or(true, |cur| ts > cur) {
                newest_edited = Some(ts.to_string());
            }
        }
    }
    for d in &res.databases {
        if let Some(ts) = d.last_edited_time.as_deref().filter(|s| !s.is_empty()) {
            if newest_edited.as_deref().map_or(true, |cur| ts > cur) {
                newest_edited = Some(ts.to_string());
            }
        }
    }

    let st = storage.clone();
    let pages_clone = res.pages.clone();
    let dbs_clone = res.databases.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        st.upsert_pages(&pages_clone)?;
        st.upsert_databases(&dbs_clone)?;
        st.set_sync_cursor(&scope_key, newest_edited.as_deref())?;
        Ok(())
    })
    .await??;

    // Notion Search is not guaranteed to enumerate every accessible object even when `has_more=false`.
    // Never bulk-archive items solely because they are absent from Search. Only on an explicit
    // `full_rescan`, verify a bounded set of missing candidates individually via direct GET by ID
    // and hide entries solely on authoritative `archived`/`in_trash` or HTTP 404 `object_not_found`.
    if full_rescan {
        const MAX_FULL_RECONCILE_PROBES: usize = 32;

        let seen_page_ids: HashSet<String> = res.pages.iter().map(|p| p.id.clone()).collect();
        let seen_db_ids: HashSet<String> = res.databases.iter().map(|d| d.id.clone()).collect();
        let st_cand = storage.clone();
        let ws_cand = workspace_id.clone();
        let (missing_page_ids, missing_db_ids) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<String>, Vec<String>)> {
                let mut missing_pages: Vec<String> = st_cand
                    .list_pages(&ws_cand, false)?
                    .into_iter()
                    .filter(|p| !seen_page_ids.contains(&p.id))
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
        let pages = st_read.list_pages(&ws_read, true)?;
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

    Ok((workspace_id, pages, databases))
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

/// Syncs a single page detail using `mode`.
pub async fn sync_page_detail_with_mode(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
    mode: DetailSyncMode,
) -> Result<crate::models::NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = crate::models::normalize_notion_id(page_id);

    if mode == DetailSyncMode::Force {
        let detail = match client.fetch_page_detail(&norm_id).await {
            Ok(d) => d,
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
        let st = storage.clone();
        let detail_clone = detail.clone();
        tokio::task::spawn_blocking(move || st.save_page_detail(&detail_clone)).await??;
        return Ok(detail);
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
            return Ok(cached_detail);
        }
        client.fetch_page_detail_with_page(fresh_page).await?
    } else {
        // Cold cache, migrated pre-v14 row (`detail_synced_edited_time IS NULL`), or invalidated detail.
        client.fetch_page_detail(&norm_id).await?
    };

    let st = storage.clone();
    let detail_clone = detail.clone();
    tokio::task::spawn_blocking(move || st.save_page_detail(&detail_clone)).await??;
    Ok(detail)
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
        client.query_database_all_with_meta(&norm_db_id, None, None, 250),
    );
    let db = db_res.ok();
    let (rows, has_more) = rows_res?;
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

/// Spawns the background sync loop returning a `SyncController`.
pub fn spawn_sync_engine(
    config: AppConfig,
    client: NotionClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<SyncCommand>();

    let active_ms = config.active_poll_ms.max(3_000);
    let tree_ms = config.sync_poll_ms.max(15_000);

    tokio::spawn(async move {
        let mut active_tick = tokio::time::interval(Duration::from_millis(active_ms));
        active_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tree_tick = tokio::time::interval(Duration::from_millis(tree_ms));
        tree_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Consume initial immediate ticks so bootstrap controls first fetch
        active_tick.tick().await;
        tree_tick.tick().await;

        loop {
            tokio::select! {
                biased;
                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        None | Some(SyncCommand::Shutdown) => break,
                        Some(SyncCommand::NotifyUserActivity) => {}
                        Some(SyncCommand::RefreshAll) => {
                            run_tree_refresh(&client, &storage, &event_tx).await;
                            // Consume any pending focus change so RefreshAll fetches the
                            // active page's block tree exactly once (no duplicate detail fetch).
                            let focus = focus_rx.borrow_and_update().clone();
                            run_focus_refresh(&client, &storage, &event_tx, &focus, DetailSyncMode::Force).await;
                        }
                        Some(SyncCommand::RefreshTree) => {
                            run_tree_refresh(&client, &storage, &event_tx).await;
                        }
                        Some(SyncCommand::RefreshPage { page_id }) => {
                            if focus_rx.has_changed().unwrap_or(false)
                                && focus_rx.borrow().page_id.as_deref() == Some(page_id.as_str())
                            {
                                let _ = focus_rx.borrow_and_update();
                            }
                            run_page_refresh(&client, &storage, &event_tx, &page_id, DetailSyncMode::Force).await;
                        }
                        Some(SyncCommand::RefreshDatabase { database_id }) => {
                            if focus_rx.has_changed().unwrap_or(false)
                                && focus_rx.borrow().database_id.as_deref() == Some(database_id.as_str())
                            {
                                let _ = focus_rx.borrow_and_update();
                            }
                            run_db_refresh(&client, &storage, &event_tx, &database_id).await;
                        }
                    }
                }
                Ok(()) = focus_rx.changed() => {
                    let focus = focus_rx.borrow_and_update().clone();
                    // Coalesce a back-to-back RefreshPage/RefreshDatabase/RefreshAll sent alongside
                    // set_focus so a single page open or refresh never fetches the block tree twice.
                    let mut deferred_cmd = None;
                    let mut mode = DetailSyncMode::Conditional;
                    if let Ok(cmd) = cmd_rx.try_recv() {
                        match (&cmd, &focus.page_id, &focus.database_id) {
                            (SyncCommand::RefreshPage { page_id }, Some(fp), _) if page_id == fp => {
                                mode = DetailSyncMode::Force;
                            }
                            (SyncCommand::RefreshDatabase { database_id }, _, Some(fd)) if database_id == fd => {}
                            _ => {
                                deferred_cmd = Some(cmd);
                            }
                        }
                    }
                    if let Some(SyncCommand::RefreshAll) = deferred_cmd {
                        run_tree_refresh(&client, &storage, &event_tx).await;
                        let latest_focus = focus_rx.borrow_and_update().clone();
                        run_focus_refresh(&client, &storage, &event_tx, &latest_focus, DetailSyncMode::Force).await;
                    } else {
                        run_focus_refresh(&client, &storage, &event_tx, &focus, mode).await;
                        if let Some(other) = deferred_cmd {
                            match other {
                                SyncCommand::Shutdown => break,
                                SyncCommand::NotifyUserActivity => {}
                                SyncCommand::RefreshTree => {
                                    run_tree_refresh(&client, &storage, &event_tx).await;
                                }
                                SyncCommand::RefreshPage { page_id } => {
                                    run_page_refresh(&client, &storage, &event_tx, &page_id, DetailSyncMode::Force).await;
                                }
                                SyncCommand::RefreshDatabase { database_id } => {
                                    run_db_refresh(&client, &storage, &event_tx, &database_id).await;
                                }
                                SyncCommand::RefreshAll => unreachable!(),
                            }
                        }
                    }
                }
                _ = active_tick.tick() => {
                    let focus = focus_rx.borrow().clone();
                    if focus.page_id.is_some() || focus.database_id.is_some() {
                        run_focus_refresh(&client, &storage, &event_tx, &focus, DetailSyncMode::Conditional).await;
                    }
                }
                _ = tree_tick.tick() => {
                    run_tree_refresh(&client, &storage, &event_tx).await;
                    // Existing slow sync cycle: recheck the single currently open page directly
                    // so nested-block and comment updates with an unchanged parent timestamp sync.
                    let focus = focus_rx.borrow_and_update().clone();
                    if let Some(ref pid) = focus.page_id {
                        run_page_refresh(&client, &storage, &event_tx, pid, DetailSyncMode::Force).await;
                    }
                }
            }
        }
    });

    SyncController::from_channels(focus_tx, cmd_tx)
}

async fn run_tree_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Syncing workspace...".to_string(),
    });
    if let Ok(me) = client.get_me().await {
        let _ = event_tx.send(CoreEvent::ProfileLoaded(me));
    }
    match sync_workspace_tree(client, storage).await {
        Ok((workspace_id, pages, databases)) => {
            let _ = event_tx.send(CoreEvent::WorkspaceTreeUpdated {
                workspace_id,
                pages,
                databases,
            });
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Synced".to_string(),
            });
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(err.to_string()));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
        }
    }
}

async fn run_focus_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    focus: &FocusTarget,
    mode: DetailSyncMode,
) {
    if let Some(ref pid) = focus.page_id {
        run_page_refresh(client, storage, event_tx, pid, mode).await;
    } else if let Some(ref db_id) = focus.database_id {
        run_db_refresh(client, storage, event_tx, db_id).await;
    }
}

async fn run_page_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    page_id: &str,
    mode: DetailSyncMode,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Loading page...".to_string(),
    });
    match sync_page_detail_with_mode(client, storage, page_id, mode).await {
        Ok(detail) => {
            let _ = event_tx.send(CoreEvent::PageDetailUpdated(Box::new(detail)));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Synced".to_string(),
            });
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(err.to_string()));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
        }
    }
}

async fn run_db_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    database_id: &str,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Querying database...".to_string(),
    });
    match sync_database_rows(client, storage, database_id).await {
        Ok((database, rows)) => {
            let _ = event_tx.send(CoreEvent::DatabaseRowsUpdated {
                database_id: database_id.to_string(),
                database,
                rows,
            });
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: "Synced".to_string(),
            });
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(err.to_string()));
            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                busy: false,
                summary: POLL_RETRY_SUMMARY.to_string(),
            });
        }
    }
}
