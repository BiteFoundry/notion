//! Inline page editor, block-live editor, page/database mutation, and Kanban actions.

use serde_json::json;

use crate::ui::{
    app::{is_block_inline_editable, AppState, BlockCursorInit, CanvasView, UiMsg, UiState},
    overlays::{
        notion_form::{NotionFormMode, NotionFormOverlay},
        Overlay,
    },
};

use super::{dispatch, selected_db_row_page, Action};

pub(super) fn try_dispatch(
    action: Action,
    app: &mut AppState,
    ui: &mut UiState,
) -> Result<bool, Action> {
    match action {
        Action::OpenQuickAppend => {
            if app.active_view != CanvasView::Page || app.active_page_id.is_none() {
                ui.warning("Open a page before appending blocks");
                return Ok(false);
            }
            ui.overlays
                .push(Overlay::notion_form(NotionFormOverlay::new(
                    NotionFormMode::QuickAppend,
                )));
        }
        Action::OpenPageEditor => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            if ui.inline_editor.is_some() {
                let _ = app.close_inline_editor_and_flush(ui);
            } else if !app.open_inline_editor(ui) {
                ui.warning("Open a page to edit");
            }
        }
        Action::OpenBlockEditor => {
            if app.active_view != CanvasView::Page || app.active_page_detail.is_none() {
                ui.warning("Open a page to edit");
                return Ok(false);
            }
            if !app.open_block_editor(ui, ui.selected_block, BlockCursorInit::End) {
                let visible = app.visible_blocks(ui);
                if let Some(first_editable) = visible
                    .iter()
                    .position(|vb| is_block_inline_editable(&vb.block.kind))
                {
                    app.open_block_editor(ui, first_editable, BlockCursorInit::End);
                } else if !app.open_inline_editor(ui) {
                    ui.warning("Open a page to edit");
                }
            }
        }
        Action::ToggleEditorMode => {
            if ui.block_editor.is_some() {
                let selected_idx = ui.selected_block;
                let visible_len = app.visible_blocks(ui).len();
                let target_snippet = ui
                    .block_editor
                    .as_ref()
                    .map(|b| b.composer.text().to_string())
                    .unwrap_or_default();
                app.commit_and_sync_block_editor(ui, true);
                if app.open_inline_editor(ui) {
                    app.position_inline_editor_near_block(
                        ui,
                        selected_idx,
                        visible_len,
                        &target_snippet,
                    );
                } else {
                    ui.warning("Open a page to edit");
                }
            } else if ui.inline_editor.is_some() {
                if app.close_inline_editor_and_flush(ui)
                    && !app.open_block_editor(ui, ui.selected_block, BlockCursorInit::End)
                {
                    let visible = app.visible_blocks(ui);
                    if let Some(first_editable) = visible
                        .iter()
                        .position(|vb| is_block_inline_editable(&vb.block.kind))
                    {
                        app.open_block_editor(ui, first_editable, BlockCursorInit::End);
                    }
                }
            } else if app.active_view == CanvasView::Page && app.active_page_detail.is_some() {
                let selected_idx = ui.selected_block;
                let visible = app.visible_blocks(ui);
                let visible_len = visible.len();
                let target_snippet = visible
                    .get(selected_idx)
                    .map(|vb| crate::ui::app::block_editable_text(&vb.block.kind))
                    .unwrap_or_default();
                if app.open_inline_editor(ui) {
                    if selected_idx > 0 {
                        app.position_inline_editor_near_block(
                            ui,
                            selected_idx,
                            visible_len,
                            &target_snippet,
                        );
                    }
                } else {
                    ui.warning("Open a page to edit");
                }
            } else {
                ui.warning("Open a page to edit");
            }
        }
        Action::FinishInlineEdit => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            } else {
                let _ = app.close_inline_editor_and_flush(ui);
            }
        }
        Action::CancelInlineEdit => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            } else {
                app.handle_inline_editor_escape(ui);
            }
        }
        Action::SaveInlineEdit => {
            if let Some(pid) = ui.block_editor.as_ref().map(|b| b.page_id.clone()) {
                let dispatched = app.commit_and_sync_block_editor(ui, false);
                if !dispatched && !ui.page_sync_in_flight.contains(&pid) {
                    ui.info("No changes to save");
                }
            } else {
                if let Some(ed) = ui.inline_editor.as_mut() {
                    ed.confirm_discard = false;
                }
                if ui.inline_editor.is_some() {
                    let dispatched = app.flush_inline_editor_sync(ui, true);
                    if !dispatched
                        && !ui
                            .inline_editor
                            .as_ref()
                            .is_some_and(|ed| ed.sync_in_flight || ed.confirm_overwrite)
                    {
                        ui.info("No changes to save");
                    }
                }
            }
        }
        Action::OpenNewPageModal => {
            let mode = if app.active_view == CanvasView::Database && app.active_database_id.is_some()
            {
                NotionFormMode::NewDatabaseItem
            } else {
                NotionFormMode::NewChildPage
            };
            ui.overlays
                .push(Overlay::notion_form(NotionFormOverlay::new(mode)));
        }
        Action::OpenDbFilter => {
            let mut form = NotionFormOverlay::new(NotionFormMode::DatabaseFilter);
            form.title = ui.db_filter.clone();
            ui.overlays.push(Overlay::notion_form(form));
        }
        Action::ApplyDbFilter(q) => {
            ui.db_filter = q;
            ui.db_selected_row = 0;
            ui.db_board_col = 0;
            ui.db_board_row = 0;
            app.clamp_db_selection(ui);
        }
        Action::SubmitSlashAppend(text) => {
            if app.active_view != CanvasView::Page {
                ui.warning("Open a page before appending blocks");
                return Ok(false);
            }
            let Some(page_id) = app.active_page_id.clone() else {
                return Ok(false);
            };
            // Render the appended blocks in memory immediately (never persisted with synthetic IDs).
            app.apply_optimistic_append_local(&page_id, &text, ui);
            let req_seq = ui.next_page_refresh_seq(&page_id);
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            ui.tasks.begin("append");
            tokio::spawn(async move {
                let res = crate::cli::op_append_blocks(&client, &storage, &page_id, &text).await;
                match res {
                    Ok(_) => {
                        let ws_id = client.auth().active_workspace_id().await;
                        let st = storage.clone();
                        let pid = page_id.clone();
                        let cached_detail = tokio::task::spawn_blocking(move || {
                            if st.load_cached_children(&ws_id, &pid)?.is_some() {
                                st.load_page_detail(&ws_id, &pid)
                            } else {
                                Ok(None)
                            }
                        })
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten();
                        if let Some(detail) = cached_detail {
                            let _ = ui_tx.send(UiMsg::PageRefreshed {
                                detail: Box::new(detail),
                                seq: req_seq,
                                task_label: "append",
                            });
                        } else if let Ok(detail) =
                            crate::sync::sync_page_detail_force(&client, &storage, &page_id).await
                        {
                            let _ = ui_tx.send(UiMsg::PageRefreshed {
                                detail: Box::new(detail),
                                seq: req_seq,
                                task_label: "append",
                            });
                        } else {
                            let _ = ui_tx.send(UiMsg::TaskDone {
                                label: "append".into(),
                                toast: "Appended blocks".into(),
                            });
                        }
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::AppendFailed {
                            page_id,
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::SubmitPageEdit(edited_markdown) => {
            let _ = app.submit_page_edit_markdown(ui, edited_markdown);
        }
        Action::SubmitCreatePage {
            title,
            icon,
            body,
            in_database,
        } => {
            let parent_id = if in_database {
                app.active_database_id.clone()
            } else {
                app.active_page_id
                    .clone()
                    .or_else(|| app.pages.first().map(|p| p.id.clone()))
            };
            let Some(parent) = parent_id else {
                ui.warning("Select a parent page or database first");
                return Ok(false);
            };
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            ui.tasks.begin("create");
            tokio::spawn(async move {
                let has_body = !body.trim().is_empty();
                let res = crate::cli::op_create_page(
                    &client,
                    &storage,
                    &parent,
                    &title,
                    icon.as_deref(),
                    has_body.then_some(body.as_str()),
                    in_database,
                    None,
                )
                .await;
                match res {
                    Ok(created) => {
                        let preview_detail = (!in_database).then(|| {
                            Box::new(crate::ui::app::build_created_page_preview(
                                created.clone(),
                                &body,
                            ))
                        });
                        let _ = ui_tx.send(UiMsg::PageCreated {
                            page: created,
                            preview_detail,
                        });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "create".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::ConfirmArchivePage => match app.active_view {
            CanvasView::Page => {
                if let Some(detail) = app.active_page_detail.as_ref() {
                    ui.overlays.push(Overlay::confirm(
                        format!("Archive page '{}' in Notion?", detail.page.display_title()),
                        Some(Action::ArchiveActivePage),
                    ));
                } else {
                    ui.warning("Open a page to archive");
                }
            }
            CanvasView::Database => {
                if let Some(row) = selected_db_row_page(app, ui) {
                    ui.overlays.push(Overlay::confirm(
                        format!("Archive item '{}' in Notion?", row.display_title()),
                        Some(Action::ArchivePageById(row.id)),
                    ));
                } else {
                    ui.warning("Select a database item to archive");
                }
            }
        },
        Action::ConfirmArchivePageById { id, title } => {
            ui.overlays.push(Overlay::confirm(
                format!("Archive page '{title}' in Notion?"),
                Some(Action::ArchivePageById(id)),
            ));
        }
        Action::ArchiveActivePage => {
            if app.active_view != CanvasView::Page {
                return Ok(false);
            }
            let Some(page_id) = app.active_page_id.clone() else {
                return Ok(false);
            };
            return Ok(dispatch(Action::ArchivePageById(page_id), app, ui));
        }
        Action::ArchivePageById(page_id) => {
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            ui.tasks.begin("archive");
            tokio::spawn(async move {
                match crate::cli::op_archive_page(&client, &storage, &page_id, true).await {
                    Ok(updated) => {
                        let _ = ui_tx.send(UiMsg::PageArchived(updated.id));
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "archive".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::RequestExternalEditor => {
            if app.active_view != CanvasView::Page || app.active_page_id.is_none() {
                ui.external_editor_seed = None;
                ui.warning("Open a page to edit in $EDITOR");
                return Ok(false);
            }
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            if let Some(ed) = ui.inline_editor.take() {
                ui.external_editor_seed = Some(ed.composer.text().to_string());
                ui.canvas_scroll = ed.scroll_y;
                ui.cursor_pos = None;
            }
            ui.editor_requested = true;
        }
        Action::SetDbViewMode(mode) => {
            ui.db_view_mode = mode;
            app.clamp_db_selection(ui);
            ui.info(format!("Database view: {}", mode.label()));
        }
        Action::CycleDbSort => {
            ui.db_sort = ui.db_sort.next();
            app.clamp_db_selection(ui);
            ui.info(format!("Sort: {}", ui.db_sort.label()));
        }
        Action::BoardPrevColumn => {
            let count = app.build_board_columns(ui).len();
            if count > 0 {
                ui.db_board_col = ui.db_board_col.min(count - 1).saturating_sub(1);
                ui.db_board_row = 0;
            }
        }
        Action::BoardNextColumn => {
            let count = app.build_board_columns(ui).len();
            if count > 0 {
                ui.db_board_col = ui.db_board_col.min(count - 1);
                if ui.db_board_col + 1 < count {
                    ui.db_board_col += 1;
                    ui.db_board_row = 0;
                }
            }
        }
        Action::BoardMoveCardNextColumn => {
            let cols = app.build_board_columns(ui);
            if cols.len() < 2 {
                ui.info("Need at least 2 status columns to move card");
                return Ok(false);
            }
            app.clamp_db_selection(ui);
            let cur_col_idx = ui.db_board_col;
            let Some(cur_col) = cols.get(cur_col_idx) else {
                return Ok(false);
            };
            let Some(card) = cur_col.cards.get(ui.db_board_row).cloned() else {
                return Ok(false);
            };
            let Some(next_col_idx) = (1..cols.len())
                .map(|step| (cur_col_idx + step) % cols.len())
                .find(|&idx| !cols[idx].is_synthetic)
            else {
                ui.info("No other valid status columns to move card into");
                return Ok(false);
            };
            let target_col = &cols[next_col_idx];
            let prop_name = target_col.property_name.clone();
            let prop_type = target_col.property_type.clone();
            let target_status = target_col.label.clone();

            let prop_val = if prop_type == "status" {
                json!({ prop_name: { "status": { "name": target_status } } })
            } else {
                json!({ prop_name: { "select": { "name": target_status } } })
            };

            let prev_col = ui.db_board_col;
            let prev_row = ui.db_board_row;
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            let page_id = card.id.clone();
            let db_id = app.active_database_id.clone().unwrap_or_default();
            let dest_label = target_col.label.clone();
            ui.tasks.begin("move card");
            tokio::spawn(async move {
                match crate::cli::op_update_database_item(
                    &client,
                    &storage,
                    &page_id,
                    None,
                    None,
                    Some(prop_val),
                )
                .await
                {
                    Ok(updated) => {
                        let _ = ui_tx.send(UiMsg::BoardCardMoved {
                            database_id: db_id,
                            updated,
                            column_label: dest_label,
                        });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::BoardCardMoveFailed {
                            database_id: db_id,
                            prev_col,
                            prev_row,
                            error: err.to_string(),
                        });
                    }
                }
            });
            ui.db_board_col = next_col_idx;
            ui.db_board_row = 0;
        }
        other => return Err(other),
    }
    Ok(false)
}

pub(super) fn handle_block_editor_action(action: &Action, app: &mut AppState, ui: &mut UiState) -> bool {
    match action {
        Action::Input(ch) => {
            let changed = if let Some(bed) = ui.block_editor.as_mut() {
                let ok = bed.composer.insert_char(*ch);
                if ok {
                    bed.dirty = true;
                }
                ok
            } else {
                false
            };
            if changed {
                let converted = if *ch == ' ' {
                    app.try_convert_block_markdown_prefix(ui)
                } else {
                    false
                };
                if !converted {
                    app.sync_active_block_to_detail(ui);
                }
                ui.canvas_follow_selected = true;
            }
            true
        }
        Action::SoftNewline => {
            if let Some(bed) = ui.block_editor.as_mut() {
                bed.composer.newline();
                bed.dirty = true;
            }
            app.sync_active_block_to_detail(ui);
            ui.canvas_follow_selected = true;
            true
        }
        Action::Newline | Action::Activate => {
            let is_code = ui.block_editor.as_ref().is_some_and(|b| b.is_code());
            if is_code {
                if let Some(bed) = ui.block_editor.as_mut() {
                    bed.composer.newline();
                    bed.dirty = true;
                }
                app.sync_active_block_to_detail(ui);
                ui.canvas_follow_selected = true;
            } else {
                let _ = app.split_active_block_on_enter(ui);
            }
            true
        }
        Action::Tab => {
            if let Some(bed) = ui.block_editor.as_mut() {
                bed.composer.insert_str("  ");
                bed.dirty = true;
            }
            app.sync_active_block_to_detail(ui);
            ui.canvas_follow_selected = true;
            true
        }
        Action::Backspace => {
            if app.try_revert_block_to_paragraph_on_backspace(ui) {
                ui.canvas_follow_selected = true;
                return true;
            }
            let changed = if let Some(bed) = ui.block_editor.as_mut() {
                let before = bed.composer.text().len();
                bed.composer.backspace();
                let diff = bed.composer.text().len() != before;
                if diff {
                    bed.dirty = true;
                }
                diff
            } else {
                false
            };
            if changed {
                app.sync_active_block_to_detail(ui);
                ui.canvas_follow_selected = true;
            }
            true
        }
        Action::Delete => {
            let changed = if let Some(bed) = ui.block_editor.as_mut() {
                let before = bed.composer.text().len();
                bed.composer.delete_forward();
                let diff = bed.composer.text().len() != before;
                if diff {
                    bed.dirty = true;
                }
                diff
            } else {
                false
            };
            if changed {
                app.sync_active_block_to_detail(ui);
                ui.canvas_follow_selected = true;
            }
            true
        }
        Action::Paste(text) => {
            let changed = if let Some(bed) = ui.block_editor.as_mut() {
                let ok = bed.composer.insert_str(text);
                if ok {
                    bed.dirty = true;
                }
                ok
            } else {
                false
            };
            if changed {
                app.sync_active_block_to_detail(ui);
                ui.canvas_follow_selected = true;
            }
            true
        }
        Action::PasteClipboard => {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    if !text.is_empty() {
                        let changed = if let Some(bed) = ui.block_editor.as_mut() {
                            let ok = bed.composer.insert_str(&text);
                            if ok {
                                bed.dirty = true;
                            }
                            ok
                        } else {
                            false
                        };
                        if changed {
                            app.sync_active_block_to_detail(ui);
                            ui.canvas_follow_selected = true;
                        }
                    }
                }
            }
            true
        }
        Action::Undo => {
            let changed = if let Some(bed) = ui.block_editor.as_mut() {
                let ok = bed.composer.undo();
                if ok {
                    bed.dirty = true;
                }
                ok
            } else {
                false
            };
            if changed {
                app.sync_active_block_to_detail(ui);
                ui.canvas_follow_selected = true;
            }
            true
        }
        Action::Redo => {
            let changed = if let Some(bed) = ui.block_editor.as_mut() {
                let ok = bed.composer.redo();
                if ok {
                    bed.dirty = true;
                }
                ok
            } else {
                false
            };
            if changed {
                app.sync_active_block_to_detail(ui);
                ui.canvas_follow_selected = true;
            }
            true
        }
        Action::CursorLeft => {
            if let Some(bed) = ui.block_editor.as_mut() {
                bed.composer.move_left(false);
            }
            ui.canvas_follow_selected = true;
            true
        }
        Action::CursorRight => {
            if let Some(bed) = ui.block_editor.as_mut() {
                bed.composer.move_right(false);
            }
            ui.canvas_follow_selected = true;
            true
        }
        Action::CursorUp | Action::MoveUp => {
            let moved_within = ui
                .block_editor
                .as_mut()
                .is_some_and(|bed| bed.move_visual_up());
            if moved_within {
                ui.canvas_follow_selected = true;
            } else {
                let _ = app.navigate_block_editor(ui, -1);
            }
            true
        }
        Action::CursorDown | Action::MoveDown => {
            let moved_within = ui
                .block_editor
                .as_mut()
                .is_some_and(|bed| bed.move_visual_down());
            if moved_within {
                ui.canvas_follow_selected = true;
            } else {
                let _ = app.navigate_block_editor(ui, 1);
            }
            true
        }
        Action::CursorHome => {
            if let Some(bed) = ui.block_editor.as_mut() {
                bed.composer.move_home(false);
            }
            ui.canvas_follow_selected = true;
            true
        }
        Action::CursorEnd => {
            if let Some(bed) = ui.block_editor.as_mut() {
                bed.composer.move_end(false);
            }
            ui.canvas_follow_selected = true;
            true
        }
        Action::PageUp => {
            let _ = app.navigate_block_editor(ui, -1);
            true
        }
        Action::PageDown => {
            let _ = app.navigate_block_editor(ui, 1);
            true
        }
        Action::CloseOverlay | Action::CancelInlineEdit => {
            app.commit_and_sync_block_editor(ui, true);
            true
        }
        _ => false,
    }
}

pub(super) fn handle_inline_editor_action(action: &Action, app: &mut AppState, ui: &mut UiState) -> bool {
    match action {
        Action::Input(ch) => {
            let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                ed.follow_cursor = true;
                ed.composer.insert_char(*ch)
            } else {
                false
            };
            if changed {
                AppState::record_inline_mutation(ui);
            }
            true
        }
        Action::Newline | Action::SoftNewline | Action::Activate => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.newline();
                ed.follow_cursor = true;
            }
            AppState::record_inline_mutation(ui);
            true
        }
        Action::Tab => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.insert_str("  ");
                ed.follow_cursor = true;
            }
            AppState::record_inline_mutation(ui);
            true
        }
        Action::Backspace => {
            let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                let before = ed.composer.text().len();
                ed.composer.backspace();
                ed.follow_cursor = true;
                ed.composer.text().len() != before
            } else {
                false
            };
            if changed {
                AppState::record_inline_mutation(ui);
            }
            true
        }
        Action::Delete => {
            let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                let before = ed.composer.text().len();
                ed.composer.delete_forward();
                ed.follow_cursor = true;
                ed.composer.text().len() != before
            } else {
                false
            };
            if changed {
                AppState::record_inline_mutation(ui);
            }
            true
        }
        Action::Paste(text) => {
            let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                ed.follow_cursor = true;
                ed.composer.insert_str(text)
            } else {
                false
            };
            if changed {
                AppState::record_inline_mutation(ui);
            }
            true
        }
        Action::PasteClipboard => {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    if !text.is_empty() {
                        let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                            ed.follow_cursor = true;
                            ed.composer.insert_str(&text)
                        } else {
                            false
                        };
                        if changed {
                            AppState::record_inline_mutation(ui);
                        }
                    }
                }
            }
            true
        }
        Action::Undo => {
            let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                let ok = ed.composer.undo();
                ed.follow_cursor = true;
                ok
            } else {
                false
            };
            if changed {
                AppState::record_inline_mutation(ui);
            }
            true
        }
        Action::Redo => {
            let changed = if let Some(ed) = ui.inline_editor.as_mut() {
                let ok = ed.composer.redo();
                ed.follow_cursor = true;
                ok
            } else {
                false
            };
            if changed {
                AppState::record_inline_mutation(ui);
            }
            true
        }
        Action::CursorLeft => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.move_left(false);
                ed.follow_cursor = true;
            }
            true
        }
        Action::CursorRight => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.move_right(false);
                ed.follow_cursor = true;
            }
            true
        }
        Action::CursorUp | Action::MoveUp => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.move_up(false);
                ed.follow_cursor = true;
            }
            true
        }
        Action::CursorDown | Action::MoveDown => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.move_down(false);
                ed.follow_cursor = true;
            }
            true
        }
        Action::CursorHome => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.move_home(false);
                ed.follow_cursor = true;
            }
            true
        }
        Action::CursorEnd => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.composer.move_end(false);
                ed.follow_cursor = true;
            }
            true
        }
        Action::PageUp => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                let steps = ed.last_viewport_height.saturating_sub(2).max(5);
                for _ in 0..steps {
                    ed.composer.move_up(false);
                }
                ed.follow_cursor = true;
            }
            true
        }
        Action::PageDown => {
            if let Some(ed) = ui.inline_editor.as_mut() {
                let steps = ed.last_viewport_height.saturating_sub(2).max(5);
                for _ in 0..steps {
                    ed.composer.move_down(false);
                }
                ed.follow_cursor = true;
            }
            true
        }
        Action::CloseOverlay | Action::CancelInlineEdit => {
            app.handle_inline_editor_escape(ui);
            true
        }
        _ => false,
    }
}

