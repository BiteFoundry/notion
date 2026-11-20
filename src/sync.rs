//! Background Notion workspace sync engine (`src/sync.rs`):
//! - Syncs workspace tree (pages & databases via `POST /v1/search`) into SQLite WAL + FTS5
//! - Polls active page (blocks + comments) or active database rows based on TUI `FocusTarget`
//! - Exposes `SyncController` for on-demand refresh nudges and clean shutdown.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::{
    config::AppConfig,
    models::{CoreEvent, NotionDatabase, NotionPage, POLL_RETRY_SUMMARY},
    notion::{NotionClient, SearchFilterObject},
    storage::Storage,
};

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

    let st_read = storage.clone();
    let ws_read = workspace_id.clone();
    let (pages, databases) = tokio::task::spawn_blocking(move || -> Result<_> {
        Ok((
            st_read.list_pages(&ws_read, true)?,
            st_read.list_databases(&ws_read)?,
        ))
    })
    .await??;

    Ok((workspace_id, pages, databases))
}

/// Syncs a single page's metadata, recursive block tree, and comments into SQLite.
/// Skips re-fetching blocks when the cached page's `last_edited_time` already matches Notion.
pub async fn sync_page_detail(
    client: &NotionClient,
    storage: &Storage,
    page_id: &str,
) -> Result<crate::models::NotionPageDetail> {
    let ws_id = client.auth().active_workspace_id().await;
    let norm_id = crate::models::normalize_notion_id(page_id);

    let st_check = storage.clone();
    let ws_check = ws_id.clone();
    let id_check = norm_id.clone();
    let cached = tokio::task::spawn_blocking(move || st_check.load_page_detail(&ws_check, &id_check))
        .await
        .ok()
        .and_then(|r| r.ok())
        .flatten();

    let detail = if let Some(cached_detail) = cached {
        let cached_ts = cached_detail
            .page
            .last_edited_time
            .as_deref()
            .filter(|s| !s.is_empty());
        if !cached_detail.blocks.is_empty() && cached_ts.is_some() {
            let fresh_page = client.get_page(&norm_id).await?;
            if fresh_page.last_edited_time.as_deref() == cached_ts {
                return Ok(cached_detail);
            }
            client.fetch_page_detail_with_page(fresh_page).await?
        } else {
            client.fetch_page_detail(&norm_id).await?
        }
    } else {
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
    let (db_res, rows_res) = tokio::join!(
        client.get_database(database_id),
        client.query_database_all(database_id, None, None, 250),
    );
    let db = db_res.ok();
    let rows = rows_res?;
    let st = storage.clone();
    let db_clone = db.clone();
    let rows_clone = rows.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(ref d) = db_clone {
            st.upsert_database(d)?;
        }
        st.upsert_pages(&rows_clone)?;
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
                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        None | Some(SyncCommand::Shutdown) => break,
                        Some(SyncCommand::NotifyUserActivity) => {}
                        Some(SyncCommand::RefreshAll) => {
                            run_tree_refresh(&client, &storage, &event_tx).await;
                            let focus = focus_rx.borrow().clone();
                            run_focus_refresh(&client, &storage, &event_tx, &focus).await;
                        }
                        Some(SyncCommand::RefreshTree) => {
                            run_tree_refresh(&client, &storage, &event_tx).await;
                        }
                        Some(SyncCommand::RefreshPage { page_id }) => {
                            run_page_refresh(&client, &storage, &event_tx, &page_id).await;
                        }
                        Some(SyncCommand::RefreshDatabase { database_id }) => {
                            run_db_refresh(&client, &storage, &event_tx, &database_id).await;
                        }
                    }
                }
                Ok(()) = focus_rx.changed() => {
                    let focus = focus_rx.borrow_and_update().clone();
                    run_focus_refresh(&client, &storage, &event_tx, &focus).await;
                }
                _ = active_tick.tick() => {
                    let focus = focus_rx.borrow().clone();
                    if focus.page_id.is_some() || focus.database_id.is_some() {
                        run_focus_refresh(&client, &storage, &event_tx, &focus).await;
                    }
                }
                _ = tree_tick.tick() => {
                    run_tree_refresh(&client, &storage, &event_tx).await;
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
) {
    if let Some(ref pid) = focus.page_id {
        run_page_refresh(client, storage, event_tx, pid).await;
    } else if let Some(ref db_id) = focus.database_id {
        run_db_refresh(client, storage, event_tx, db_id).await;
    }
}

async fn run_page_refresh(
    client: &NotionClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    page_id: &str,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Loading page...".to_string(),
    });
    match sync_page_detail(client, storage, page_id).await {
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
