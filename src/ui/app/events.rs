//! Background `CoreEvent` ingestion into `AppState` and `UiState`.

use std::collections::HashMap;

use crate::{
    models::{CoreEvent, POLL_RETRY_SUMMARY},
    ui::composer::normalize,
};

use super::{
    state::{AppState, UiState},
    views::all_blocks_have_server_ids,
};

impl AppState {
    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) -> bool {
        match ev {
            CoreEvent::ProfileLoaded(user) => {
                if let Some(ref ws_name) = user.workspace_name {
                    if !ws_name.trim().is_empty() {
                        self.workspace_name = ws_name.clone();
                    }
                }
                self.current_user = Some(user);
                true
            }
            CoreEvent::WorkspaceTreeUpdated {
                workspace_id,
                pages,
                databases,
            } => {
                if self
                    .expected_workspace_id
                    .as_ref()
                    .is_some_and(|expected| *expected != workspace_id)
                {
                    return false;
                }
                self.workspace_id = workspace_id;
                self.pages = pages;
                self.databases = databases;
                self.bump_pages_gen();
                self.bump_dbs_gen();
                self.reload_page_parent_hints();
                if self.active_page_id.is_none() && self.active_database_id.is_none() {
                    if let Some(first) = self.pages.iter().find(|p| !p.archived).cloned() {
                        self.open_page(&first.id, ui, false);
                    } else if let Some(first_db) = self.databases.first().cloned() {
                        self.open_database(&first_db.id, ui, false);
                    }
                }
                self.snap_sidebar_selection(ui);
                true
            }
            CoreEvent::PageDetailUpdated(detail) => {
                if let Some(expected) = self.expected_workspace_id.as_deref() {
                    if !detail.page.workspace_id.is_empty() && detail.page.workspace_id != expected
                    {
                        return false;
                    }
                    if detail.page.workspace_id.is_empty()
                        && self.active_page_id.as_deref() != Some(detail.page.id.as_str())
                        && !self.pages.iter().any(|p| p.id == detail.page.id)
                    {
                        return false;
                    }
                }
                let syncing_own_edit = ui.page_sync_in_flight.contains(&detail.page.id)
                    || ui.pending_page_sync.contains_key(&detail.page.id);
                if syncing_own_edit {
                    return false;
                }
                if ui
                    .block_editor
                    .as_ref()
                    .is_some_and(|b| b.page_id == detail.page.id)
                {
                    if all_blocks_have_server_ids(&detail.blocks) {
                        ui.page_confirmed_blocks
                            .insert(detail.page.id.clone(), detail.blocks.clone());
                    }
                    return self.merge_page_summary(&detail.page);
                }
                let summary_changed = self.merge_page_summary(&detail.page);
                let active_editor_matches = ui
                    .inline_editor
                    .as_ref()
                    .is_some_and(|ed| ed.page_id == detail.page.id);
                if active_editor_matches {
                    let incoming_md =
                        normalize(&crate::page_edit::editor_markdown_for_detail(&detail));
                    let mut visual_changed = summary_changed;
                    if let Some(ed) = ui.inline_editor.as_mut() {
                        let next_title = detail.page.display_title().to_string();
                        if ed.page_title != next_title {
                            ed.page_title = next_title;
                            visual_changed = true;
                        }
                        let prev_ts = ed
                            .last_synced_detail
                            .as_ref()
                            .and_then(|d| d.page.last_edited_time.as_deref());
                        let remote_changed = incoming_md != ed.last_synced_markdown
                            || detail.page.last_edited_time.as_deref() != prev_ts;
                        if !ed.is_dirty() {
                            if remote_changed {
                                ed.composer.load_text_cursor_start(&incoming_md);
                                ed.last_synced_markdown = incoming_md;
                                visual_changed = true;
                            }
                            ed.remote_blocks = detail.blocks.clone();
                            ed.last_synced_detail = Some((*detail).clone());
                            ed.remote_conflict = None;
                            ed.confirm_overwrite = false;
                            if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                                let detail_changed = self.active_page_detail.as_ref().is_none_or(|cur| {
                                    !crate::sync::page_detail_semantically_equal(cur, &detail)
                                });
                                if detail_changed {
                                    self.reload_page_parent_hints();
                                    self.active_page_detail = Some(*detail);
                                    self.bump_blocks_gen();
                                    visual_changed = true;
                                }
                            }
                        } else if remote_changed {
                            let same_conflict = ed.remote_conflict.as_ref().is_some_and(|prev| {
                                normalize(&crate::page_edit::editor_markdown_for_detail(prev))
                                    == incoming_md
                                    && prev.page.last_edited_time == detail.page.last_edited_time
                                    && prev.comments == detail.comments
                            });
                            if !same_conflict {
                                // Preserve the user's active draft buffer; record conflict for Ctrl+S guard
                                // and update `last_synced_detail` so discarding restores latest remote.
                                ed.remote_conflict = Some((*detail).clone());
                                ed.last_synced_detail = Some(*detail);
                                ed.confirm_overwrite = false;
                                ui.warning(
                                    "Remote page changed — draft preserved (Ctrl+S twice to overwrite, Esc x2 to reload)",
                                );
                                visual_changed = true;
                            }
                        } else if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                            if let Some(active) = self.active_page_detail.as_mut() {
                                if active.page.title != detail.page.title
                                    || active.page.icon != detail.page.icon
                                    || active.comments != detail.comments
                                {
                                    active.page.title = detail.page.title;
                                    active.page.icon = detail.page.icon;
                                    active.comments = detail.comments;
                                    self.bump_blocks_gen();
                                    visual_changed = true;
                                }
                            }
                        }
                    }
                    return visual_changed;
                }
                if self.active_page_id.as_deref() == Some(detail.page.id.as_str()) {
                    let detail_changed = self.active_page_detail.as_ref().is_none_or(|cur| {
                        !crate::sync::page_detail_semantically_equal(cur, &detail)
                    });
                    if !detail_changed && !summary_changed {
                        return false;
                    }
                    self.reload_page_parent_hints();
                    self.active_page_detail = Some(*detail);
                    self.bump_blocks_gen();
                    self.clamp_canvas_selection(ui);
                    return true;
                }
                if summary_changed {
                    self.reload_page_parent_hints();
                }
                summary_changed
            }
            CoreEvent::DatabaseRowsUpdated {
                database_id,
                database,
                rows,
                rows_truncated,
            } => {
                if let Some(expected) = self.expected_workspace_id.as_deref() {
                    let event_ws = database
                        .as_ref()
                        .map(|d| d.workspace_id.as_str())
                        .filter(|s| !s.is_empty())
                        .or_else(|| {
                            rows.first()
                                .map(|r| r.workspace_id.as_str())
                                .filter(|s| !s.is_empty())
                        });
                    if let Some(ws) = event_ws {
                        if ws != expected {
                            return false;
                        }
                    } else if self.active_database_id.as_deref() != Some(database_id.as_str())
                        && !self.databases.iter().any(|d| d.id == database_id)
                    {
                        return false;
                    }
                }
                let mut changed = false;
                if let Some(db) = database {
                    if let Some(existing) = self.databases.iter_mut().find(|d| d.id == db.id) {
                        if !crate::sync::database_semantically_equal(existing, &db) {
                            *existing = db;
                            self.bump_dbs_gen();
                            changed = true;
                        }
                    } else {
                        self.databases.push(db);
                        self.bump_dbs_gen();
                        changed = true;
                    }
                }
                if self.active_database_id.as_deref() == Some(database_id.as_str()) {
                    let fetched_rows = rows.len();
                    if rows_truncated && self.active_database_rows.len() > rows.len() {
                        // Preserve already-loaded rows beyond a truncated background poll slice
                        // while refreshing any rows present in the poll window.
                        let mut by_id: HashMap<String, usize> = self
                            .active_database_rows
                            .iter()
                            .enumerate()
                            .map(|(i, r)| (r.id.clone(), i))
                            .collect();
                        for incoming in rows {
                            if let Some(&idx) = by_id.get(&incoming.id) {
                                if !crate::sync::page_semantically_equal(
                                    &self.active_database_rows[idx],
                                    &incoming,
                                ) {
                                    self.active_database_rows[idx] = incoming;
                                    changed = true;
                                }
                            } else {
                                by_id.insert(incoming.id.clone(), self.active_database_rows.len());
                                self.active_database_rows.push(incoming);
                                changed = true;
                            }
                        }
                    } else {
                        let rows_same = self.active_database_rows.len() == rows.len()
                            && self
                                .active_database_rows
                                .iter()
                                .zip(rows.iter())
                                .all(|(a, b)| crate::sync::page_semantically_equal(a, b));
                        if !rows_same {
                            self.active_database_rows = rows;
                            changed = true;
                        }
                    }
                    if self.active_database_rows_truncated != rows_truncated {
                        self.active_database_rows_truncated = rows_truncated;
                        changed = true;
                    }
                    if self.active_database_rows_fetched != fetched_rows {
                        self.active_database_rows_fetched = fetched_rows;
                        changed |= rows_truncated;
                    }
                    self.clamp_db_selection(ui);
                }
                changed
            }
            CoreEvent::CommentsUpdated { page_id, comments } => {
                if self.active_page_id.as_deref() != Some(page_id.as_str()) {
                    return false;
                }
                if let Some(ref mut detail) = self.active_page_detail {
                    if let Some(expected) = self.expected_workspace_id.as_deref() {
                        if !detail.page.workspace_id.is_empty()
                            && detail.page.workspace_id != expected
                        {
                            return false;
                        }
                    }
                    if detail.page.id == page_id && detail.comments != comments {
                        detail.comments = comments;
                        self.bump_blocks_gen();
                        return true;
                    }
                }
                false
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                let next_retry = summary == POLL_RETRY_SUMMARY;
                let changed = self.syncing != busy
                    || ui.poll_retrying != next_retry
                    || self.sync_summary != summary;
                self.syncing = busy;
                ui.poll_retrying = next_retry;
                self.sync_summary = summary;
                changed
            }
            CoreEvent::SyncError(err) => {
                ui.error(format!("Sync: {err}"));
                true
            }
        }
    }

}
