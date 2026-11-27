//! Asynchronous `UiMsg` dispatcher (thin router + domain handlers) and block hydration.

use crate::{
    img_preview::MAX_IMAGE_CACHE_BYTES,
    models::{NotionBlock, NotionPage, NotionPageDetail, NotionPageSummary},
    storage::Storage,
    sync::{FocusTarget, SyncCommand},
    ui::overlays::Overlay,
};

use super::{
    state::{AppState, UiMsg, UiState},
    views::{
        all_blocks_have_server_ids, attach_children_in_tree, reconcile_server_block_ids,
        set_todo_in_tree, update_block_media_url_in_tree,
    },
};

impl AppState {
    /// Routes asynchronous background task messages (`UiMsg`) to domain handlers.
    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::StartLocalBootstrap => {
                self.sync_ctrl.request(SyncCommand::RefreshAll);
            }
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.syncing = sync_busy;
                if let Some(p) = pid {
                    ui.info(format!("Attached to sync daemon (pid {p})"));
                }
            }
            UiMsg::DaemonDetached => {
                ui.warning("Sync daemon detached; switched to local sync");
            }
            UiMsg::PageRefreshed { detail, seq, task_label } => {
                self.handle_page_refreshed_msg(ui, detail, seq, task_label);
            }
            UiMsg::TodoToggleFailed { page_id, block_id, revert_checked, error } => {
                self.handle_todo_toggle_failed_msg(ui, &page_id, &block_id, revert_checked, error);
            }
            UiMsg::PageEdited { page_id, generation, synced_markdown, detail } => {
                self.handle_page_edited_msg(ui, page_id, generation, synced_markdown, detail);
            }
            UiMsg::PageEditFailed { page_id, generation: _, error, edited_markdown } => {
                self.handle_page_edit_failed_msg(ui, page_id, error, edited_markdown);
            }
            UiMsg::MediaUrlRefreshed { page_id, block_id, url } => {
                self.handle_media_url_refreshed_msg(ui, &page_id, &block_id, &url);
            }
            UiMsg::AppendFailed { page_id, error } => {
                self.handle_append_failed_msg(ui, &page_id, error);
            }
            UiMsg::PageCreated { page, preview_detail } => {
                self.handle_page_created_msg(ui, page, preview_detail);
            }
            UiMsg::WorkspaceTreeRefreshed { workspace_id, pages, databases, task_label, toast } => {
                ui.tasks.finish(task_label, None);
                if self.expected_workspace_id.as_ref().is_some_and(|e| *e != workspace_id) {
                    return;
                }
                self.workspace_id = workspace_id;
                self.pages = pages;
                self.databases = databases;
                self.bump_pages_gen();
                self.bump_dbs_gen();
                self.reload_page_parent_hints();
                self.snap_sidebar_selection(ui);
                ui.success(toast);
            }
            UiMsg::PageArchived(id) => self.handle_page_archived_msg(ui, &id),
            UiMsg::CommentAdded { page_id, comment } => {
                ui.tasks.finish("comment", None);
                if let Some(ref mut detail) = self.active_page_detail {
                    if detail.page.id == page_id {
                        detail.comments.push(comment);
                        self.bump_blocks_gen();
                    }
                }
                ui.success("Comment posted");
            }
            UiMsg::BoardCardMoved { database_id, updated, column_label } => {
                ui.tasks.finish("move card", None);
                if let Some(row) = self.active_database_rows.iter_mut().find(|r| r.id == updated.id) {
                    *row = updated;
                }
                self.clamp_db_selection(ui);
                if !database_id.is_empty() {
                    self.sync_ctrl.request(SyncCommand::RefreshDatabase { database_id });
                }
                ui.success(format!("Moved card -> {column_label}"));
            }
            UiMsg::BoardCardMoveFailed { database_id, prev_col, prev_row, error } => {
                ui.tasks.finish("move card", Some(error.clone()));
                if self.active_database_id.as_deref() == Some(database_id.as_str()) {
                    ui.db_board_col = prev_col;
                    ui.db_board_row = prev_row;
                    self.clamp_db_selection(ui);
                }
                ui.error(format!("move card: {error}"));
            }
            UiMsg::WorkspaceSwitched { name, workspace_id } => {
                self.handle_workspace_switched_msg(ui, name, workspace_id);
            }
            UiMsg::ImageRendered { cache_key, rendered } => {
                let sz = rendered.approx_heap_bytes();
                if self.image_cache_bytes + sz > MAX_IMAGE_CACHE_BYTES {
                    self.clear_image_cache();
                }
                self.image_cache_bytes += sz;
                self.image_cache.insert(cache_key, *rendered);
            }
            UiMsg::ImagePreviewFailed { seq, error } => {
                if seq == ui.preview_request_seq {
                    if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlays.last_mut() {
                        ov.error = Some(error);
                    }
                }
            }
            UiMsg::HtmlPreviewLoaded { seq, title, html, mime } => {
                self.handle_html_preview_loaded_msg(ui, seq, title, html, mime);
            }
            UiMsg::HtmlServed { url } => {
                ui.tasks.finish("html-serve", None);
                match open::that(&url) {
                    Ok(()) => ui.success(format!("Served HTML at {url}")),
                    Err(err) => ui.warning(format!("Served HTML at {url} (browser failed: {err})")),
                }
            }
            UiMsg::TaskDone { label, toast } => {
                ui.tasks.finish(&label, None);
                ui.success(toast);
            }
            UiMsg::TaskFailed { label, error } => {
                ui.tasks.finish(&label, Some(error.clone()));
                ui.error(format!("{label}: {error}"));
            }
            UiMsg::BlockChildrenHydrated { page_id, block_id, children } => {
                ui.hydrating_blocks.remove(&block_id);
                if ui.hydrating_blocks.is_empty() {
                    ui.tasks.finish("hydrate", None);
                }
                self.apply_hydrated_children(ui, &page_id, &block_id, children);
            }
            UiMsg::BlockChildrenHydrationFailed { block_id, error } => {
                ui.hydrating_blocks.remove(&block_id);
                ui.tasks.finish("hydrate", Some(error.clone()));
                ui.error(format!("Could not load toggle contents: {error}"));
            }
        }
    }

    fn handle_page_refreshed_msg(
        &mut self,
        ui: &mut UiState,
        detail: Box<NotionPageDetail>,
        seq: u64,
        task_label: &'static str,
    ) {
        ui.tasks.finish(task_label, None);
        if let Some(expected) = self.expected_workspace_id.as_deref() {
            if !detail.page.workspace_id.is_empty() && detail.page.workspace_id != expected
            {
                return;
            }
        }
        let latest_seq = ui
            .page_refresh_seq
            .get(&detail.page.id)
            .copied()
            .unwrap_or(0);
        if seq < latest_seq {
            return;
        }
        let syncing_own_edit = ui.page_sync_in_flight.contains(&detail.page.id)
            || ui.pending_page_sync.contains_key(&detail.page.id);
        if syncing_own_edit {
            return;
        }
        self.merge_page_summary(&detail.page);
        let editing_this_page = ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_id == detail.page.id)
            || ui
                .block_editor
                .as_ref()
                .is_some_and(|b| b.page_id == detail.page.id);
        if all_blocks_have_server_ids(&detail.blocks) {
            ui.page_confirmed_blocks
                .insert(detail.page.id.clone(), detail.blocks.clone());
        }
        if self.active_page_id.as_deref() == Some(detail.page.id.as_str())
            && !editing_this_page
        {
            self.active_page_detail = Some(*detail);
            self.bump_blocks_gen();
            self.clamp_canvas_selection(ui);
        }
        self.reload_page_parent_hints();
        ui.success("Synced page");
    }

    fn handle_todo_toggle_failed_msg(
        &mut self,
        ui: &mut UiState,
        page_id: &str,
        block_id: &str,
        revert_checked: bool,
        error: String,
    ) {
        ui.tasks.finish("todo", Some(error.clone()));
        let editing_this_page = ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_id == page_id);
        if self.active_page_id.as_deref() == Some(page_id) && !editing_this_page {
            if let Some(ref mut detail) = self.active_page_detail {
                set_todo_in_tree(&mut detail.blocks, block_id, revert_checked);
                detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
                self.bump_blocks_gen();
            }
        }
        ui.error(format!("todo failed: {error}"));
    }

    fn handle_page_edited_msg(
        &mut self,
        ui: &mut UiState,
        page_id: String,
        generation: u64,
        synced_markdown: String,
        detail: Box<NotionPageDetail>,
    ) {
        ui.tasks.finish("edit", None);
        ui.page_sync_in_flight.remove(&page_id);
        let prev_synced = ui
            .page_synced_generations
            .get(&page_id)
            .copied()
            .unwrap_or(0);
        ui.page_synced_generations
            .insert(page_id.clone(), prev_synced.max(generation));

        let current_gen = ui
            .page_edit_generations
            .get(&page_id)
            .copied()
            .unwrap_or(0);

        if generation >= current_gen {
            self.merge_page_summary(&detail.page);
        }

        let mut synced_detail = *detail;
        if !crate::parser::has_column_blocks(&synced_detail.blocks)
            && !crate::page_edit::contains_column_marker(&synced_markdown)
        {
            synced_detail.markdown = synced_markdown.clone();
        }

        if all_blocks_have_server_ids(&synced_detail.blocks) {
            ui.page_confirmed_blocks
                .insert(page_id.clone(), synced_detail.blocks.clone());
        }

        let active_inline_matches = ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_id == page_id);
        let active_block_matches = ui
            .block_editor
            .as_ref()
            .is_some_and(|b| b.page_id == page_id);

        if active_inline_matches {
            let mut still_dirty = false;
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.sync_in_flight = false;
                ed.last_sync_error = None;
                if generation >= ed.synced_generation {
                    ed.synced_generation = generation;
                    ed.remote_blocks = synced_detail.blocks.clone();
                    ed.last_synced_markdown = synced_markdown.clone();
                    ed.last_synced_detail = Some(synced_detail.clone());
                }
                still_dirty = ed.is_dirty();
            }

            // Only launch a follow-up sync if an explicit save was queued while in flight.
            if let Some((pending_gen, pending_md, _)) = ui.pending_page_sync.remove(&page_id)
            {
                if pending_gen > generation {
                    let known = (!synced_detail.blocks.is_empty())
                        .then_some(synced_detail.blocks);
                    if let Some(ed) = ui.inline_editor.as_mut() {
                        ed.sync_in_flight = true;
                    }
                    ui.page_sync_in_flight.insert(page_id.clone());
                    self.spawn_page_diff_sync(ui, page_id, pending_gen, pending_md, known);
                }
            } else if !still_dirty {
                if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                    self.active_page_detail = Some(synced_detail);
                    self.bump_blocks_gen();
                }
                ui.success("Saved to Notion");
            } else {
                // User typed newer unsaved edits while explicit save was in flight:
                // DO NOT overwrite local editor or optimistic detail, and do NOT auto-sync!
                ui.success("Saved to Notion");
            }
        } else if active_block_matches {
            if let Some((pending_gen, pending_md, _)) = ui.pending_page_sync.remove(&page_id)
            {
                if pending_gen > generation {
                    let known = (!synced_detail.blocks.is_empty())
                        .then_some(synced_detail.blocks.clone());
                    ui.page_sync_in_flight.insert(page_id.clone());
                    self.spawn_page_diff_sync(ui, page_id.clone(), pending_gen, pending_md, known);
                }
            }
            // Reconcile newly minted server IDs into the live optimistic tree without
            // losing any unsaved text or freshly created empty sibling block under the cursor.
            if let Some(ref mut local_detail) = self.active_page_detail {
                if local_detail.page.id == page_id {
                    reconcile_server_block_ids(
                        &mut local_detail.blocks,
                        &synced_detail.blocks,
                        ui.block_editor.as_mut(),
                    );
                    self.bump_blocks_gen();
                }
            }
            ui.success("Saved to Notion");
        } else if generation < current_gen {
            if let Some((pending_gen, pending_md, _)) = ui.pending_page_sync.remove(&page_id)
            {
                if pending_gen > generation {
                    let known = (!synced_detail.blocks.is_empty())
                        .then_some(synced_detail.blocks);
                    ui.page_sync_in_flight.insert(page_id.clone());
                    self.spawn_page_diff_sync(ui, page_id, pending_gen, pending_md, known);
                }
            }
        } else {
            ui.pending_page_sync.remove(&page_id);
            if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                self.active_page_detail = Some(synced_detail);
                self.bump_blocks_gen();
            }
            ui.success("Saved to Notion");
        }
    }

    fn handle_page_edit_failed_msg(
        &mut self,
        ui: &mut UiState,
        page_id: String,
        error: String,
        edited_markdown: String,
    ) {
        ui.tasks.finish("edit", Some(error.clone()));
        ui.page_sync_in_flight.remove(&page_id);
        let cached = self
            .storage
            .load_page_detail(&self.workspace_id, &page_id)
            .ok()
            .flatten();
        if let Some(ref actual) = cached {
            if self.active_page_id.as_deref() == Some(page_id.as_str()) {
                self.active_page_detail = Some(actual.clone());
                self.bump_blocks_gen();
            }
        }
        if let Some(ed) = ui.inline_editor.as_mut().filter(|ed| ed.page_id == page_id) {
            ed.sync_in_flight = false;
            ed.last_sync_error = Some(error.clone());
            if let Some(actual) = cached {
                ed.remote_blocks = actual.blocks.clone();
                ed.last_synced_detail = Some(actual);
            } else {
                ed.remote_blocks.clear();
            }
            ui.error(format!("Sync failed: {error}"));
        } else if ui
            .block_editor
            .as_ref()
            .is_some_and(|b| b.page_id == page_id)
        {
            ui.error(format!("Sync failed: {error}"));
        } else {
            self.preserve_failed_page_edit(ui, &page_id, &edited_markdown, &error);
        }
    }

    fn handle_media_url_refreshed_msg(
        &mut self,
        ui: &mut UiState,
        page_id: &str,
        block_id: &str,
        url: &str,
    ) {
        let page_id = page_id.to_string();
        let block_id = block_id.to_string();
        let url = url.to_string();
        if !block_id.trim().is_empty() && !url.trim().is_empty() {
            if let Some(ref mut detail) = self.active_page_detail {
                if page_id.is_empty() || detail.page.id == page_id {
                    for m in &mut detail.media {
                        if m.block_id == block_id {
                            m.url = url.clone();
                        }
                    }
                    update_block_media_url_in_tree(&mut detail.blocks, &block_id, &url);
                    self.bump_blocks_gen();
                }
            }
            for ov in &mut ui.overlays {
                if let Overlay::ImagePreview(ref mut preview) = ov {
                    for item in &mut preview.items {
                        if item.block_id == block_id
                            && (page_id.is_empty() || item.page_id == page_id)
                        {
                            item.url = url.clone();
                        }
                    }
                }
            }
        }
    }

    fn handle_append_failed_msg(&mut self, ui: &mut UiState, page_id: &str, error: String) {
        ui.tasks.finish("append", Some(error.clone()));
        if self.active_page_id.as_deref() == Some(page_id) {
            if let Ok(Some(cached)) = self.storage.load_page_detail(&self.workspace_id, page_id) {
                self.active_page_detail = Some(cached);
                self.bump_blocks_gen();
                self.clamp_canvas_selection(ui);
            }
        }
        ui.error(format!("append: {error}"));
    }

    fn handle_page_created_msg(
        &mut self,
        ui: &mut UiState,
        page: NotionPage,
        preview_detail: Option<Box<NotionPageDetail>>,
    ) {
        ui.tasks.finish("create", None);
        if let Some(expected) = self.expected_workspace_id.as_deref() {
            if !page.workspace_id.is_empty() && page.workspace_id != expected {
                return;
            }
        }
        let id = page.id.clone();
        let title = page.title.clone();
        let summary = NotionPageSummary::from(&page);
        if let Some(existing) = self.pages.iter_mut().find(|p| p.id == id) {
            *existing = summary;
        } else {
            self.pages.insert(0, summary);
        }
        self.bump_pages_gen();
        if page.parent_type == "database_id" {
            let target_db_id = page
                .parent_id
                .clone()
                .or_else(|| self.active_database_id.clone());
            if let Some(db_id) = target_db_id {
                if self.active_database_id.as_deref() == Some(db_id.as_str()) {
                    if let Some(existing) =
                        self.active_database_rows.iter_mut().find(|r| r.id == id)
                    {
                        *existing = page;
                    } else {
                        self.active_database_rows.insert(0, page);
                    }
                    self.clamp_db_selection(ui);
                }
                self.sync_ctrl
                    .request(SyncCommand::RefreshDatabase { database_id: db_id });
            }
        } else {
            let _ = self.try_open_page(&id, ui, true);
            if let Some(preview) = preview_detail {
                if self.active_page_id.as_deref() == Some(id.as_str()) {
                    self.active_page_detail = Some(*preview);
                    self.bump_blocks_gen();
                    self.clamp_canvas_selection(ui);
                }
            }
        }
        self.snap_sidebar_selection(ui);
        ui.success(format!("Created '{title}'"));
    }

    fn handle_page_archived_msg(&mut self, ui: &mut UiState, id: &str) {
        ui.tasks.finish("archive", None);
        if let Some(p) = self.pages.iter_mut().find(|p| p.id == id) {
            p.archived = true;
            self.bump_pages_gen();
        }
        for row in &mut self.active_database_rows {
            if row.id == id {
                row.archived = true;
            }
        }
        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == id {
                detail.page.archived = true;
                self.bump_blocks_gen();
            }
        }
        if ui.inline_editor.as_ref().is_some_and(|ed| ed.page_id == id) {
            ui.inline_editor = None;
            ui.cursor_pos = None;
        }
        if ui.block_editor.as_ref().is_some_and(|b| b.page_id == id) {
            ui.block_editor = None;
            ui.cursor_pos = None;
        }
        self.snap_sidebar_selection(ui);
        self.clamp_db_selection(ui);
        self.clamp_canvas_selection(ui);
        ui.success("Archived page");
    }

    fn handle_workspace_switched_msg(
        &mut self,
        ui: &mut UiState,
        name: String,
        workspace_id: String,
    ) {
        ui.tasks.finish("workspace", None);
        self.workspace_name = name.clone();
        self.workspace_id = workspace_id.clone();
        self.expected_workspace_id = Some(workspace_id.clone());
        self.config.set_active_workspace(name.clone());
        if let Ok(new_storage) = Storage::open(&crate::config::db_path(Some(&workspace_id))) {
            self.storage = new_storage;
        }
        self.active_page_id = None;
        self.active_page_detail = None;
        ui.inline_editor = None;
        ui.block_editor = None;
        ui.canvas_block_layouts.clear();
        ui.canvas_link_hits.clear();
        ui.page_confirmed_blocks.clear();
        ui.cursor_pos = None;
        ui.page_refresh_seq.clear();
        ui.bump_preview_request_seq();
        self.active_database_id = None;
        self.active_database_rows.clear();
        self.active_database_rows_truncated = false;
        self.active_database_rows_fetched = 0;
        self.block_owner_pages.clear();
        self.child_page_positions.clear();
        self.nav_stack.clear();
        // The sync engine must not keep polling a page of the workspace just left.
        self.sync_ctrl.set_focus(FocusTarget::default());
        self.load_cached_initial();
        self.snap_sidebar_selection(ui);
        let rebinding = self
            .workspace_switch_tx
            .as_ref()
            .is_some_and(|tx| tx.send(workspace_id.clone()).is_ok());
        if !rebinding {
            self.sync_ctrl.request(SyncCommand::RefreshAll);
        }
        ui.success(format!("Switched workspace -> {name}"));
    }

    fn handle_html_preview_loaded_msg(
        &mut self,
        ui: &mut UiState,
        seq: u64,
        title: String,
        html: String,
        mime: Option<String>,
    ) {
        ui.tasks.finish("html", None);
        if seq != ui.preview_request_seq {
            return;
        }
        if matches!(
            ui.overlays.last(),
            Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
        ) {
            ui.overlays.pop();
            ui.active_pixel_graphic = None;
            ui.pending_pixel_graphics.clear();
        }
        let kind = bitefoundry_html::sniff_content_kind(
            mime.as_deref(),
            Some(&title),
            Some(html.as_bytes()),
        );
        if kind == bitefoundry_html::ContentKind::Markdown {
            ui.overlays.push(Overlay::markdown_preview(title, html));
            ui.info("Opened Markdown viewer");
        } else {
            ui.overlays.push(Overlay::html_preview(title, html));
            ui.info("Opened interactive HTML viewer");
        }
    }

    /// Fetches, on demand, the children of a block that the recursive page fetch stopped short of
    /// (`has_children` with nothing loaded, e.g. a toggle nested past the tree depth cap), so
    /// expanding it shows its content instead of blank space.
    pub fn hydrate_block_children(&mut self, block_id: &str, ui: &mut UiState) {
        let Some(page_id) = self.active_page_detail.as_ref().map(|d| d.page.id.clone()) else {
            return;
        };
        if block_id.trim().is_empty() || block_id.starts_with("__optimistic_") {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if !ui.hydrating_blocks.insert(block_id.to_string()) {
            return;
        }
        let client = self.client.clone();
        let ui_tx = self.ui_tx.clone();
        let bid = block_id.to_string();
        ui.tasks.begin("hydrate");
        handle.spawn(async move {
            let fetched = async {
                let mut children = client.list_block_children(&bid, &page_id, Some(&bid)).await?;
                client.hydrate_block_subtrees(&mut children, &page_id).await?;
                Ok::<_, anyhow::Error>(children)
            }
            .await;
            let _ = ui_tx.send(match fetched {
                Ok(children) => UiMsg::BlockChildrenHydrated {
                    page_id,
                    block_id: bid,
                    children,
                },
                Err(err) => UiMsg::BlockChildrenHydrationFailed {
                    block_id: bid,
                    error: err.to_string(),
                },
            });
        });
    }

    /// Attaches lazily fetched children to the open page. The confirmed sync baseline gets the
    /// same children so a later diff-sync sees them as existing content rather than new blocks.
    /// Skipped while the full-page Markdown editor is open: its draft was rendered without them.
    pub fn apply_hydrated_children(
        &mut self,
        ui: &mut UiState,
        page_id: &str,
        block_id: &str,
        children: Vec<NotionBlock>,
    ) -> bool {
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_id == page_id)
        {
            return false;
        }
        let Some(detail) = self
            .active_page_detail
            .as_mut()
            .filter(|d| d.page.id == page_id)
        else {
            return false;
        };
        if !attach_children_in_tree(&mut detail.blocks, block_id, &children) {
            return false;
        }
        if let Some(confirmed) = ui.page_confirmed_blocks.get_mut(page_id) {
            attach_children_in_tree(confirmed, block_id, &children);
        }
        detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
        detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
        detail.media = crate::parser::collect_media_refs(&detail.blocks);
        self.bump_blocks_gen();
        self.clamp_canvas_selection(ui);
        true
    }

}
