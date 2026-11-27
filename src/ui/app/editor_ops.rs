//! Inline page editor, block-live editor mutations, optimistic updates, and diff sync.

use std::borrow::Cow;

use crate::{
    models::{NotionBlock, NotionBlockKind, NotionColor, NotionPageDetail},
    parser::apply_optimistic_markdown_to_detail,
};

use super::{
    editor_state::{
        apply_text_to_block_kind, is_block_inline_editable, BlockCursorInit, InlineBlockEditor,
        InlinePageEditor,
    },
    state::{AppState, CanvasView, Focus, NarrowPane, UiMsg, UiState},
    views::{
        all_blocks_have_server_ids, build_optimistic_append_blocks, find_parent_block_id_in_tree,
        insert_sibling_after_in_tree, set_todo_in_tree, update_block_in_tree,
    },
};

impl AppState {
    /// Opens the in-place inline Markdown editor directly inside the Page Canvas pane.
    pub fn open_inline_editor(&mut self, ui: &mut UiState) -> bool {
        if ui.block_editor.is_some() {
            self.commit_and_sync_block_editor(ui, true);
        }
        if self.active_view != CanvasView::Page {
            return false;
        }
        let Some(detail) = self.active_page_detail.as_ref() else {
            return false;
        };
        let page_id = detail.page.id.clone();
        let page_title = detail.page.display_title().to_string();
        let current_gen = ui.page_edit_generations.get(&page_id).copied().unwrap_or(0);
        let synced_gen = ui
            .page_synced_generations
            .get(&page_id)
            .copied()
            .unwrap_or(current_gen);
        let in_flight = ui.page_sync_in_flight.contains(&page_id);
        let remote_blocks = if !detail.blocks.is_empty()
            && detail
                .blocks
                .iter()
                .all(|b| !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_"))
        {
            detail.blocks.clone()
        } else {
            Vec::new()
        };

        let last_synced_detail = Some(detail.clone());
        let initial_markdown = crate::page_edit::editor_markdown_for_detail(detail);

        ui.inline_editor = Some(InlinePageEditor::new(
            page_id,
            page_title,
            &initial_markdown,
            ui.canvas_scroll,
            current_gen,
            synced_gen,
            in_flight,
            remote_blocks,
            last_synced_detail,
        ));
        ui.focus = Focus::Canvas;
        ui.narrow_pane = NarrowPane::Canvas;
        true
    }

    /// Positions the active `InlinePageEditor` cursor near `block_idx` (preferring an exact
    /// line match on `target_snippet` closest to the proportional block line) and enables
    /// `follow_cursor` so the viewport scrolls straight to that block.
    pub fn position_inline_editor_near_block(
        &self,
        ui: &mut UiState,
        block_idx: usize,
        visible_len: usize,
        target_snippet: &str,
    ) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        let text = ed.composer.text();
        let mut lines: Vec<(usize, usize)> = Vec::new();
        let mut start = 0usize;
        for (i, ch) in text.char_indices() {
            if ch == '\n' {
                lines.push((start, i));
                start = i + 1;
            }
        }
        lines.push((start, text.len()));
        if lines.is_empty() {
            return;
        }

        let approx_line = if visible_len > 0 {
            block_idx
                .saturating_mul(lines.len())
                .checked_div(visible_len)
                .unwrap_or(0)
                .min(lines.len().saturating_sub(1))
        } else {
            0
        };

        let needle = target_snippet
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("");

        let target_line = if !needle.is_empty() {
            lines
                .iter()
                .enumerate()
                .filter(|(_, &(s, e))| text.get(s..e).is_some_and(|line| line.contains(needle)))
                .min_by_key(|(idx, _)| idx.abs_diff(approx_line))
                .map(|(idx, _)| idx)
                .unwrap_or(approx_line)
        } else {
            approx_line
        };

        let (_, line_end) = lines[target_line];
        ed.composer.set_cursor(line_end);
        ed.follow_cursor = true;
    }

    /// Tracks a buffer mutation while the composer owns the draft and its undo history.
    pub fn record_inline_mutation(ui: &mut UiState) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        ed.edit_generation = ed.edit_generation.saturating_add(1);
        ed.last_sync_error = None;
        ed.confirm_discard = false;
        ed.confirm_overwrite = false;
        ui.page_edit_generations.insert(ed.page_id.clone(), ed.edit_generation);
    }

    /// Builds the optimistic canvas detail for a save or external editor handoff.
    pub fn apply_optimistic_local(&mut self, page_id: &str, edited_markdown: &str) {
        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id
                && detail.markdown != edited_markdown
                && !crate::parser::has_column_blocks(&detail.blocks)
                && !crate::page_edit::contains_column_marker(edited_markdown)
            {
                apply_optimistic_markdown_to_detail(detail, edited_markdown);
            }
        }
    }

    /// Immediately appends optimistic blocks to the active in-memory page detail so quick-append
    /// renders in 0ms while the API request runs. Never writes `__optimistic_*` IDs to SQLite.
    pub fn apply_optimistic_append_local(
        &mut self,
        page_id: &str,
        markdown_or_slash: &str,
        ui: &mut UiState,
    ) -> bool {
        let optimistic = build_optimistic_append_blocks(page_id, markdown_or_slash);
        if optimistic.is_empty() {
            return false;
        }
        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id {
                detail.blocks.extend(optimistic);
                detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
                detail.media = crate::parser::collect_media_refs(&detail.blocks);
                self.clamp_canvas_selection(ui);
                return true;
            }
        }
        false
    }

    /// Materializes the draft on demand for actions that read the page's blocks.
    pub fn detail_for_block_action(&self, ui: &UiState) -> Option<Cow<'_, NotionPageDetail>> {
        if self.active_view != CanvasView::Page {
            return None;
        }
        let detail = self.active_page_detail.as_ref()?;
        if let Some(ed) = ui.inline_editor.as_ref().filter(|ed| ed.page_id == detail.page.id) {
            let markdown = ed.composer.text();
            if markdown != detail.markdown
                && !crate::parser::has_column_blocks(&detail.blocks)
                && !crate::page_edit::contains_column_marker(markdown)
            {
                let mut draft = detail.clone();
                apply_optimistic_markdown_to_detail(&mut draft, markdown);
                return Some(Cow::Owned(draft));
            }
        }
        Some(Cow::Borrowed(detail))
    }

    /// Reflects the draft in the canvas and dispatches a background diff-sync, coalescing
    /// explicit saves while a previous save is in flight.
    pub fn flush_inline_editor_sync(&mut self, ui: &mut UiState, queue_if_in_flight: bool) -> bool {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return false;
        };
        ed.confirm_discard = false;
        let page_id = ed.page_id.clone();
        if !ed.is_dirty() {
            return false;
        }

        if ed.remote_conflict.is_some() && !ed.confirm_overwrite {
            ed.confirm_overwrite = true;
            ui.warning(
                "Remote page changed — press Ctrl+S again to save your draft, or Esc x2 to reload",
            );
            return false;
        }

        let snapshot = ed.composer.text().to_string();
        let baseline_blocks = if ed.remote_conflict.is_some() {
            ed.remote_conflict
                .as_ref()
                .map(|d| d.blocks.clone())
                .unwrap_or_default()
        } else {
            ed.remote_blocks.clone()
        };
        if !baseline_blocks.is_empty() {
            if let Err(err) = crate::page_edit::PageEdit::plan(&page_id, &baseline_blocks, &snapshot) {
                ed.last_sync_error = Some(err.to_string());
                ui.error(format!("Cannot save: {err}"));
                return false;
            }
        }

        let overwrite_after_conflict = ed.remote_conflict.take().is_some();
        ed.confirm_overwrite = false;

        let next_gen = ed
            .edit_generation
            .max(ed.synced_generation)
            .max(ui.page_edit_generations.get(&page_id).copied().unwrap_or(0))
            .saturating_add(1);
        ed.edit_generation = next_gen;
        ui.page_edit_generations.insert(page_id.clone(), next_gen);

        let in_flight = ed.sync_in_flight || ui.page_sync_in_flight.contains(&page_id);
        let remote_blocks = if overwrite_after_conflict {
            Vec::new()
        } else {
            ed.remote_blocks.clone()
        };

        // Update in-memory detail immediately on explicit save, but persist SQLite only after
        // server confirmation so failed saves never leave `__optimistic_*` IDs in SQLite.
        self.apply_optimistic_local(&page_id, &snapshot);

        if in_flight {
            if queue_if_in_flight {
                ui.pending_page_sync
                    .insert(page_id, (next_gen, snapshot, remote_blocks));
            }
            return false;
        }

        if let Some(ed) = ui.inline_editor.as_mut() {
            ed.sync_in_flight = true;
            ed.last_sync_error = None;
        }
        ui.page_sync_in_flight.insert(page_id.clone());
        ui.pending_page_sync.remove(&page_id);

        let known = (!remote_blocks.is_empty()).then_some(remote_blocks);
        self.spawn_page_diff_sync(ui, page_id, next_gen, snapshot, known);
        true
    }

    /// Handles `Esc` inside the inline editor without firing unwanted Notion API syncs:
    /// - Clean buffer: exits immediately to rendered Canvas at `ed.scroll_y`.
    /// - Dirty buffer (1st `Esc`): arms `confirm_discard` and warns user (`Ctrl+S Save · Esc again Discard`).
    /// - Dirty buffer (2nd `Esc`): discards unsaved edits, restores `active_page_detail` from
    ///   `last_synced_detail`, and exits cleanly with 0 API calls.
    pub fn handle_inline_editor_escape(&mut self, ui: &mut UiState) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        if !ed.is_dirty() {
            let scroll = ed.scroll_y;
            ui.inline_editor = None;
            ui.canvas_scroll = scroll;
            ui.canvas_follow_selected = false;
            ui.cursor_pos = None;
            return;
        }
        if !ed.confirm_discard {
            ed.confirm_discard = true;
            ui.warning("Unsaved changes — Ctrl+S Save · Esc again Discard");
            return;
        }
        self.discard_inline_editor(ui);
    }

    /// Discards unsaved inline edits, restores `active_page_detail` to `last_synced_detail`,
    /// and exits inline edit mode without calling Notion API.
    pub fn discard_inline_editor(&mut self, ui: &mut UiState) {
        let Some(ed) = ui.inline_editor.take() else {
            return;
        };
        if let Some(saved_detail) = ed.last_synced_detail {
            if self.active_page_id.as_deref() == Some(ed.page_id.as_str()) {
                self.active_page_detail = Some(saved_detail);
            }
        }
        ui.canvas_scroll = ed.scroll_y;
        ui.canvas_follow_selected = false;
        ui.cursor_pos = None;
        ui.info("Discarded unsaved changes");
    }

    /// Exits inline edit mode cleanly, saving any unsynced edits immediately while
    /// preserving the user's current Canvas scroll position. Returns `false` if the editor
    /// refused to close due to a save error or unresolved remote conflict.
    pub fn close_inline_editor_and_flush(&mut self, ui: &mut UiState) -> bool {
        if ui.inline_editor.is_none() {
            return true;
        }
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.last_sync_error.is_some())
        {
            return false;
        }
        let _ = self.flush_inline_editor_sync(ui, true);
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.last_sync_error.is_some() || ed.confirm_overwrite)
        {
            return false;
        }
        if let Some(ed) = ui.inline_editor.take() {
            ui.canvas_scroll = ed.scroll_y;
            let visible_len = self.visible_blocks(ui).len();
            if visible_len > 0 {
                let total_lines = ed.composer.text().lines().count().max(1);
                let cursor_line = ed.composer.cursor_line_col().0.saturating_sub(1);
                let target_blk = (cursor_line.saturating_mul(visible_len) / total_lines)
                    .min(visible_len.saturating_sub(1));
                ui.selected_block = target_blk;
                ui.canvas_follow_selected = true;
            } else {
                ui.canvas_follow_selected = false;
            }
        }
        ui.cursor_pos = None;
        true
    }

    // ------------------------------------------------------------------
    // Notion Native Block-Live Flow (Single-Block In-Place Editing)
    // ------------------------------------------------------------------

    /// Captures the server-confirmed block tree baseline for `page_id` before the first optimistic
    /// block-live mutation, if not already cached and if all block IDs are real Notion IDs.
    fn ensure_page_confirmed_baseline(&self, ui: &mut UiState, page_id: &str) {
        if ui.page_confirmed_blocks.contains_key(page_id) {
            return;
        }
        if let Some(detail) = self.active_page_detail.as_ref().filter(|d| d.page.id == page_id) {
            if !detail.blocks.is_empty() && all_blocks_have_server_ids(&detail.blocks) {
                ui.page_confirmed_blocks
                    .insert(page_id.to_string(), detail.blocks.clone());
            }
        }
    }

    /// Opens Notion Native Block-Live editing directly IN PLACE on visible block `visible_idx`,
    /// keeping all other blocks on the page 100% rendered.
    pub fn open_block_editor(
        &mut self,
        ui: &mut UiState,
        visible_idx: usize,
        cursor_init: BlockCursorInit,
    ) -> bool {
        if self.active_view != CanvasView::Page || ui.inline_editor.is_some() {
            return false;
        }
        let Some(page_id) = self.active_page_detail.as_ref().map(|d| d.page.id.clone()) else {
            return false;
        };
        let visible = self.visible_blocks(ui);
        let Some(vblk) = visible.get(visible_idx).cloned() else {
            return false;
        };
        if !is_block_inline_editable(&vblk.block.kind) {
            return false;
        }

        // If already editing this exact block, just reposition the cursor without losing undo state.
        if let Some(existing) = ui
            .block_editor
            .as_mut()
            .filter(|b| b.page_id == page_id && b.block_id == vblk.block.id)
        {
            ui.selected_block = visible_idx;
            ui.focus = Focus::Canvas;
            ui.narrow_pane = NarrowPane::Canvas;
            match cursor_init {
                BlockCursorInit::Start => existing.composer.set_cursor(0),
                BlockCursorInit::End => existing.composer.move_doc_end(false),
                BlockCursorInit::ScreenPos { col, row } => {
                    existing.place_cursor_at_screen_pos(col, row);
                }
            }
            return true;
        }

        // Commit any previously active block before switching to a different block.
        if ui.block_editor.is_some() {
            self.commit_and_sync_block_editor(ui, true);
        }

        self.ensure_page_confirmed_baseline(ui, &page_id);

        // Re-fetch visible block in case committing updated the tree.
        let visible = self.visible_blocks(ui);
        let target_idx = visible
            .iter()
            .position(|vb| vb.block.id == vblk.block.id)
            .unwrap_or_else(|| visible_idx.min(visible.len().saturating_sub(1)));
        let Some(target_vblk) = visible.get(target_idx) else {
            return false;
        };
        if !is_block_inline_editable(&target_vblk.block.kind) {
            return false;
        }

        let mut editor = InlineBlockEditor::from_visible(page_id, target_vblk);
        let layout = ui
            .canvas_block_layouts
            .iter()
            .find(|l| l.block_id == target_vblk.block.id || l.visible_idx == target_idx)
            .cloned();
        if let Some(ref lay) = layout {
            editor.last_text_x = lay.first_text_x;
            editor.last_first_screen_row = lay.text_first_screen_row;
            editor.last_wrap_width = lay.text_width.max(1);
        }
        match cursor_init {
            BlockCursorInit::Start => editor.composer.set_cursor(0),
            BlockCursorInit::End => editor.composer.move_doc_end(false),
            BlockCursorInit::ScreenPos { col, row } => {
                if let Some(ref lay) = layout {
                    editor.place_cursor_from_layout(col, row, lay);
                } else {
                    editor.composer.move_doc_end(false);
                }
            }
        }

        ui.selected_block = target_idx;
        ui.canvas_follow_selected = true;
        ui.block_editor = Some(editor);
        ui.focus = Focus::Canvas;
        ui.narrow_pane = NarrowPane::Canvas;
        true
    }

    /// Immediately reflects the active `InlineBlockEditor`'s text and kind into
    /// `self.active_page_detail` (0ms optimistic local state, never touching SQLite).
    pub fn sync_active_block_to_detail(&mut self, ui: &mut UiState) {
        let Some(bed) = ui.block_editor.as_mut() else {
            return;
        };
        let text = bed.composer.text().to_string();
        apply_text_to_block_kind(&mut bed.kind, &text);
        let page_id = bed.page_id.clone();
        let block_id = bed.block_id.clone();
        let new_type = bed.block_type.clone();
        let new_kind = bed.kind.clone();

        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id
                && update_block_in_tree(&mut detail.blocks, &block_id, &new_type, &new_kind)
            {
                detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
            }
        }
    }

    /// While editing a `Paragraph` block, converts live Markdown prefixes typed at the start
    /// (`# `, `## `, `### `, `- `, `* `, `[] `, `[ ] `, `> `) into the corresponding Notion block
    /// kind in 0ms, stripping the prefix and preserving any text after the cursor.
    pub fn try_convert_block_markdown_prefix(&mut self, ui: &mut UiState) -> bool {
        let converted = {
            let Some(bed) = ui.block_editor.as_mut() else {
                return false;
            };
            let NotionBlockKind::Paragraph { color, .. } = bed.kind else {
                return false;
            };
            let text = bed.composer.text();
            let cursor = bed.composer.cursor().min(text.len());
            if !text.is_char_boundary(cursor) {
                return false;
            }
            let prefix = &text[..cursor];
            let next_spec: Option<(&'static str, NotionBlockKind)> = match prefix {
                "# " => Some((
                    "heading_1",
                    NotionBlockKind::Heading1 {
                        rich_text: Vec::new(),
                        is_toggleable: false,
                        color,
                    },
                )),
                "## " => Some((
                    "heading_2",
                    NotionBlockKind::Heading2 {
                        rich_text: Vec::new(),
                        is_toggleable: false,
                        color,
                    },
                )),
                "### " => Some((
                    "heading_3",
                    NotionBlockKind::Heading3 {
                        rich_text: Vec::new(),
                        is_toggleable: false,
                        color,
                    },
                )),
                "- " | "* " => Some((
                    "bulleted_list_item",
                    NotionBlockKind::BulletedListItem {
                        rich_text: Vec::new(),
                        color,
                    },
                )),
                "[] " | "[ ] " => Some((
                    "to_do",
                    NotionBlockKind::ToDo {
                        rich_text: Vec::new(),
                        checked: false,
                        color,
                    },
                )),
                "> " => Some((
                    "quote",
                    NotionBlockKind::Quote {
                        rich_text: Vec::new(),
                        color,
                    },
                )),
                _ => None,
            };
            if let Some((new_type, mut new_kind)) = next_spec {
                let remainder = text[cursor..].to_string();
                apply_text_to_block_kind(&mut new_kind, &remainder);
                bed.block_type = new_type.to_string();
                bed.kind = new_kind;
                bed.composer.load_text_cursor_start(&remainder);
                bed.dirty = true;
                true
            } else {
                false
            }
        };
        if converted {
            self.sync_active_block_to_detail(ui);
        }
        converted
    }

    /// Pressing Backspace at cursor 0 of a Heading/List/To-Do/Quote converts it cleanly back to Paragraph.
    pub fn try_revert_block_to_paragraph_on_backspace(&mut self, ui: &mut UiState) -> bool {
        let reverted = {
            let Some(bed) = ui.block_editor.as_mut() else {
                return false;
            };
            if bed.composer.cursor() != 0 || bed.composer.has_selection() {
                return false;
            }
            let color = match bed.kind {
                NotionBlockKind::Heading1 { color, .. }
                | NotionBlockKind::Heading2 { color, .. }
                | NotionBlockKind::Heading3 { color, .. }
                | NotionBlockKind::BulletedListItem { color, .. }
                | NotionBlockKind::NumberedListItem { color, .. }
                | NotionBlockKind::ToDo { color, .. }
                | NotionBlockKind::Quote { color, .. } => color,
                _ => return false,
            };
            let text = bed.composer.text().to_string();
            let mut kind = NotionBlockKind::Paragraph {
                rich_text: Vec::new(),
                color,
            };
            apply_text_to_block_kind(&mut kind, &text);
            bed.block_type = "paragraph".to_string();
            bed.kind = kind;
            bed.dirty = true;
            true
        };
        if reverted {
            self.sync_active_block_to_detail(ui);
        }
        reverted
    }

    /// Commits the current non-Code block on `Enter`, splitting text after cursor (or inserting an
    /// empty sibling block below when at end) in 0ms, triggers coalesced background Notion sync,
    /// and moves inline editing focus directly into the newly created sibling block.
    pub fn split_active_block_on_enter(&mut self, ui: &mut UiState) -> bool {
        let Some(bed) = ui.block_editor.as_ref() else {
            return false;
        };
        if bed.is_code() {
            return false;
        }
        let page_id = bed.page_id.clone();
        let current_block_id = bed.block_id.clone();
        let current_kind = bed.kind.clone();
        let current_type = bed.block_type.clone();
        let text = bed.composer.text().to_string();
        let split_pos = bed.composer.cursor().min(text.len());
        let split_pos = if text.is_char_boundary(split_pos) {
            split_pos
        } else {
            text.len()
        };
        let before = text[..split_pos].to_string();
        let after = text[split_pos..].to_string();

        self.ensure_page_confirmed_baseline(ui, &page_id);

        let mut updated_current_kind = current_kind.clone();
        apply_text_to_block_kind(&mut updated_current_kind, &before);

        // Determine sibling block kind (Notion-native: headings/quotes/callouts continue as paragraph;
        // lists and to-dos continue with the same list/to-do kind).
        let (next_type, mut next_kind) = match &current_kind {
            NotionBlockKind::BulletedListItem { color, .. } if !before.is_empty() || !after.is_empty() => (
                "bulleted_list_item".to_string(),
                NotionBlockKind::BulletedListItem {
                    rich_text: Vec::new(),
                    color: *color,
                },
            ),
            NotionBlockKind::NumberedListItem { color, .. } if !before.is_empty() || !after.is_empty() => (
                "numbered_list_item".to_string(),
                NotionBlockKind::NumberedListItem {
                    rich_text: Vec::new(),
                    color: *color,
                },
            ),
            NotionBlockKind::ToDo { color, .. } if !before.is_empty() || !after.is_empty() => (
                "to_do".to_string(),
                NotionBlockKind::ToDo {
                    rich_text: Vec::new(),
                    checked: false,
                    color: *color,
                },
            ),
            _ => (
                "paragraph".to_string(),
                NotionBlockKind::Paragraph {
                    rich_text: Vec::new(),
                    color: NotionColor::Default,
                },
            ),
        };
        apply_text_to_block_kind(&mut next_kind, &after);

        ui.optimistic_block_seq = ui.optimistic_block_seq.saturating_add(1);
        let new_block_id = format!("__optimistic_live_{}_{}", page_id, ui.optimistic_block_seq);
        let parent_block_id = self
            .active_page_detail
            .as_ref()
            .and_then(|d| find_parent_block_id_in_tree(&d.blocks, &current_block_id));

        let new_block = NotionBlock {
            id: new_block_id.clone(),
            page_id: page_id.clone(),
            parent_block_id,
            block_type: next_type.clone(),
            has_children: false,
            archived: false,
            kind: next_kind.clone(),
            children: Vec::new(),
        };

        if let Some(ref mut detail) = self.active_page_detail {
            if detail.page.id == page_id {
                update_block_in_tree(
                    &mut detail.blocks,
                    &current_block_id,
                    &current_type,
                    &updated_current_kind,
                );
                if !insert_sibling_after_in_tree(&mut detail.blocks, &current_block_id, new_block) {
                    return false;
                }
                detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
                detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
            }
        } else {
            return false;
        }

        // Sync the committed edit + split to Notion in background (safely coalesced).
        self.sync_block_live_page(ui, &page_id);

        // Move inline editing focus directly into the newly inserted sibling block at cursor 0.
        let visible = self.visible_blocks(ui);
        if let Some(new_vis_idx) = visible.iter().position(|vb| vb.block.id == new_block_id) {
            let mut next_ed = InlineBlockEditor::from_visible(page_id, &visible[new_vis_idx]);
            next_ed.composer.set_cursor(0);
            next_ed.dirty = !after.is_empty();
            if let Some(prev_ed) = ui.block_editor.as_ref() {
                next_ed.last_wrap_width = prev_ed.last_wrap_width;
                next_ed.last_text_x = prev_ed.last_text_x;
            }
            ui.block_editor = Some(next_ed);
            ui.selected_block = new_vis_idx;
            ui.canvas_follow_selected = true;
            true
        } else {
            false
        }
    }

    /// Seamlessly moves inline block editing to the previous (`direction < 0`, cursor at end)
    /// or next (`direction > 0`, cursor at start) editable block, flushing/optimistically saving
    /// the current block first.
    pub fn navigate_block_editor(&mut self, ui: &mut UiState, direction: isize) -> bool {
        let Some((page_id, current_id, was_dirty)) = ui
            .block_editor
            .as_ref()
            .map(|b| (b.page_id.clone(), b.block_id.clone(), b.dirty))
        else {
            return false;
        };

        self.sync_active_block_to_detail(ui);
        let visible = self.visible_blocks(ui);
        let Some(cur_idx) = visible.iter().position(|vb| vb.block.id == current_id) else {
            return false;
        };

        let target_idx = if direction < 0 {
            (0..cur_idx)
                .rev()
                .find(|&i| is_block_inline_editable(&visible[i].block.kind))
        } else {
            ((cur_idx + 1)..visible.len())
                .find(|&i| is_block_inline_editable(&visible[i].block.kind))
        };

        let Some(next_idx) = target_idx else {
            if let Some(bed) = ui.block_editor.as_mut() {
                if direction < 0 {
                    bed.composer.set_cursor(0);
                } else {
                    bed.composer.move_doc_end(false);
                }
            }
            return false;
        };

        if was_dirty {
            self.sync_block_live_page(ui, &page_id);
        }

        let target_vblk = visible[next_idx].clone();
        let mut next_ed = InlineBlockEditor::from_visible(page_id, &target_vblk);
        if direction < 0 {
            next_ed.composer.move_doc_end(false);
        } else {
            next_ed.composer.set_cursor(0);
        }
        if let Some(lay) = ui
            .canvas_block_layouts
            .iter()
            .find(|l| l.block_id == target_vblk.block.id)
        {
            next_ed.last_text_x = lay.first_text_x;
            next_ed.last_first_screen_row = lay.text_first_screen_row;
            next_ed.last_wrap_width = lay.text_width.max(1);
        }
        ui.block_editor = Some(next_ed);
        ui.selected_block = next_idx;
        ui.canvas_follow_selected = true;
        true
    }

    /// Commits the active block editor's changes into `active_page_detail`, dispatches background
    /// sync if dirty, and optionally closes `ui.block_editor`.
    pub fn commit_and_sync_block_editor(&mut self, ui: &mut UiState, close_after: bool) -> bool {
        let Some((page_id, dirty)) = ui
            .block_editor
            .as_ref()
            .map(|b| (b.page_id.clone(), b.dirty))
        else {
            return false;
        };
        self.sync_active_block_to_detail(ui);
        let synced = if dirty {
            self.sync_block_live_page(ui, &page_id)
        } else {
            false
        };
        if close_after {
            ui.block_editor = None;
            ui.cursor_pos = None;
            ui.canvas_follow_selected = true;
        } else if let Some(bed) = ui.block_editor.as_mut() {
            bed.dirty = false;
        }
        synced
    }

    /// Coalesced background diff-sync for Notion Native Block-Live edits. Safe to call even while
    /// a previous sync is in flight or when new blocks still have `__optimistic_live_*` IDs.
    pub fn sync_block_live_page(&mut self, ui: &mut UiState, page_id: &str) -> bool {
        let Some(detail) = self.active_page_detail.as_ref().filter(|d| d.page.id == page_id) else {
            return false;
        };
        let edited_markdown = crate::page_edit::editor_markdown_for_detail(detail);
        let confirmed = ui.page_confirmed_blocks.get(page_id).cloned();

        // Skip redundant sync if markdown matches confirmed baseline exactly.
        if let Some(ref base_blocks) = confirmed {
            let base_md = if crate::parser::has_column_blocks(base_blocks) {
                crate::parser::blocks_to_editor_markdown(base_blocks)
            } else {
                crate::parser::blocks_to_markdown(base_blocks)
            };
            if base_md.trim() == edited_markdown.trim() {
                return false;
            }
        }

        let next_gen = ui
            .page_edit_generations
            .get(page_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        ui.page_edit_generations.insert(page_id.to_string(), next_gen);

        let known_blocks = confirmed
            .filter(|blocks| !blocks.is_empty() && all_blocks_have_server_ids(blocks));

        if ui.page_sync_in_flight.contains(page_id) {
            ui.pending_page_sync.insert(
                page_id.to_string(),
                (next_gen, edited_markdown, known_blocks.unwrap_or_default()),
            );
            return true;
        }

        if tokio::runtime::Handle::try_current().is_err() {
            return false;
        }

        ui.page_sync_in_flight.insert(page_id.to_string());
        ui.pending_page_sync.remove(page_id);
        self.spawn_page_diff_sync(
            ui,
            page_id.to_string(),
            next_gen,
            edited_markdown,
            known_blocks,
        );
        true
    }

    /// Submits an edited Markdown document (e.g. from external `$EDITOR`) with 0ms optimistic
    /// update + SQLite cache + fast diff-based background sync.
    pub fn submit_page_edit_markdown(&mut self, ui: &mut UiState, edited_markdown: String) -> bool {
        let Some(page_id) = self.active_page_id.clone() else {
            ui.warning("Open a page before saving edits");
            return false;
        };
        let current_md = self
            .active_page_detail
            .as_ref()
            .map(crate::page_edit::editor_markdown_for_detail)
            .unwrap_or_default();
        if edited_markdown.trim() == current_md.trim() {
            ui.info("No changes to save");
            return false;
        }
        let known_blocks = self.active_page_detail.as_ref().and_then(|d| {
            let valid = !d.blocks.is_empty()
                && d.blocks
                    .iter()
                    .all(|b| !b.id.trim().is_empty() && !b.id.starts_with("__optimistic_"));
            valid.then(|| d.blocks.clone())
        });
        if let Some(ref blocks) = known_blocks {
            if let Err(err) = crate::page_edit::PageEdit::plan(&page_id, blocks, &edited_markdown) {
                self.preserve_failed_page_edit(ui, &page_id, &edited_markdown, &err.to_string());
                return false;
            }
        }

        let next_gen = ui
            .page_edit_generations
            .get(&page_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        ui.page_edit_generations.insert(page_id.clone(), next_gen);

        self.apply_optimistic_local(&page_id, &edited_markdown);

        if ui.page_sync_in_flight.contains(&page_id) {
            ui.pending_page_sync.insert(
                page_id,
                (next_gen, edited_markdown, known_blocks.unwrap_or_default()),
            );
            return true;
        }

        ui.page_sync_in_flight.insert(page_id.clone());
        ui.pending_page_sync.remove(&page_id);
        self.spawn_page_diff_sync(ui, page_id, next_gen, edited_markdown, known_blocks);
        true
    }

    pub(super) fn preserve_failed_page_edit(
        &mut self,
        ui: &mut UiState,
        page_id: &str,
        edited_markdown: &str,
        error: &str,
    ) {
        let recovery_note = match crate::page_edit::save_edit_recovery(page_id, edited_markdown) {
            Ok(path) => format!(" (draft saved to {})", path.display()),
            Err(_) => String::new(),
        };
        if self.active_page_id.as_deref() == Some(page_id) && self.active_view == CanvasView::Page {
            if let Some(detail) = self.active_page_detail.as_ref() {
                let current_gen = ui.page_edit_generations.get(page_id).copied().unwrap_or(0);
                let synced_gen = ui
                    .page_synced_generations
                    .get(page_id)
                    .copied()
                    .unwrap_or(current_gen);
                let initial_md = crate::page_edit::editor_markdown_for_detail(detail);
                let mut editor = InlinePageEditor::new(
                    page_id,
                    detail.page.display_title(),
                    &initial_md,
                    ui.canvas_scroll,
                    current_gen.saturating_add(1),
                    synced_gen,
                    false,
                    detail.blocks.clone(),
                    Some(detail.clone()),
                );
                editor.composer.load_text_cursor_start(edited_markdown);
                editor.last_sync_error = Some(error.to_string());
                ui.inline_editor = Some(editor);
                ui.focus = Focus::Canvas;
                ui.narrow_pane = NarrowPane::Canvas;
            }
        }
        ui.error(format!("Cannot save: {error}{recovery_note}"));
    }

    pub(super) fn spawn_page_diff_sync(
        &self,
        ui: &mut UiState,
        page_id: String,
        generation: u64,
        edited_markdown: String,
        known_existing: Option<Vec<NotionBlock>>,
    ) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            ui.page_sync_in_flight.remove(&page_id);
            if let Some(ed) = ui.inline_editor.as_mut().filter(|e| e.page_id == page_id) {
                ed.sync_in_flight = false;
            }
            return;
        };
        let client = self.client.clone();
        let storage = self.storage.clone();
        let ui_tx = self.ui_tx.clone();
        ui.tasks.begin("edit");
        handle.spawn(async move {
            match crate::cli::op_diff_sync_page_markdown(
                &client,
                &storage,
                &page_id,
                &edited_markdown,
                known_existing,
            )
            .await
            {
                Ok(detail) => {
                    let _ = ui_tx.send(UiMsg::PageEdited {
                        page_id,
                        generation,
                        synced_markdown: edited_markdown,
                        detail: Box::new(detail),
                    });
                }
                Err(err) => {
                    let _ = ui_tx.send(UiMsg::PageEditFailed {
                        page_id,
                        generation,
                        error: err.to_string(),
                        edited_markdown,
                    });
                }
            }
        });
    }


    pub fn toggle_todo_block(&mut self, block_id: &str, next_checked: bool, ui: &mut UiState) {
        // Optimistic local update for instant feedback
        if let Some(ref mut detail) = self.active_page_detail {
            set_todo_in_tree(&mut detail.blocks, block_id, next_checked);
            detail.page.plain_text = crate::parser::blocks_to_plain_text(&detail.blocks);
            detail.markdown = crate::parser::blocks_to_markdown(&detail.blocks);
        }
        let client = self.client.clone();
        let storage = self.storage.clone();
        let ui_tx = self.ui_tx.clone();
        let bid = block_id.to_string();
        let page_id = self.active_page_id.clone().unwrap_or_default();
        let ws_id = self.workspace_id.clone();
        let req_seq = ui.next_page_refresh_seq(&page_id);
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        ui.tasks.begin("todo");
        handle.spawn(async move {
            match client.set_todo_checked(&bid, &page_id, next_checked).await {
                Ok(updated_block) => {
                    let st = storage.clone();
                    let ws_apply = ws_id.clone();
                    let blk_apply = updated_block.clone();
                    let applied = tokio::task::spawn_blocking(move || {
                        let res = st.apply_updated_block(&ws_apply, &blk_apply)?;
                        let detail = match &res {
                            Some((pid, false)) => st.load_page_detail(&ws_apply, pid)?,
                            _ => None,
                        };
                        Ok::<_, anyhow::Error>((res, detail))
                    })
                    .await;

                    if let Ok(Ok((Some((_, false)), Some(detail)))) = applied {
                        let _ = ui_tx.send(UiMsg::PageRefreshed {
                            detail: Box::new(detail),
                            seq: req_seq,
                            task_label: "todo",
                        });
                        return;
                    }

                    // Server response alone could not restore the cached tree: invalidate only this
                    // page's detail marker and re-fetch directly while open.
                    let st_inv = storage.clone();
                    let ws_inv = ws_id.clone();
                    let pid_inv = page_id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        st_inv.invalidate_page_detail(&ws_inv, &pid_inv)
                    })
                    .await;

                    if let Ok(detail) =
                        crate::sync::sync_page_detail_force(&client, &storage, &page_id).await
                    {
                        let _ = ui_tx.send(UiMsg::PageRefreshed {
                            detail: Box::new(detail),
                            seq: req_seq,
                            task_label: "todo",
                        });
                    } else {
                        let _ = ui_tx.send(UiMsg::TaskDone {
                            label: "todo".into(),
                            toast: "Updated to-do".into(),
                        });
                    }
                }
                Err(err) => {
                    let _ = ui_tx.send(UiMsg::TodoToggleFailed {
                        page_id,
                        block_id: bid,
                        revert_checked: !next_checked,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

}
