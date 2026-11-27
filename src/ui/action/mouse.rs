//! Mouse scroll routing and hit-tested click handling for the Notion TUI.

use crate::{
    models::{spans_plain_text, DatabaseViewMode, NotionBlockKind},
    ui::{
        app::{
            is_block_inline_editable, AppState, BlockCursorInit, CanvasView, Focus, NarrowPane,
            UiState,
        },
        hitmap::{ButtonId, HitTarget},
        overlays::{image_preview::ImagePreviewOverlay, Overlay, OverlayKind, OverlayOutcome},
    },
};

use super::{
    dispatch, find_media_index_for_block,
    overlays::{is_html_or_xml_ref, is_markdown_file_ref, is_previewable_markup_ref},
    step_sidebar_selection, Action,
};

pub(super) fn handle_mouse_scroll(
    delta: i16,
    column: u16,
    row: u16,
    app: &mut AppState,
    ui: &mut UiState,
) {
    if delta == 0 {
        return;
    }
    let pos = ratatui::layout::Position::new(column, row);
    let hit = ui.hitmap.hit(row, column);
    let target_pane = match hit {
        Some(HitTarget::SidebarRow(_))
        | Some(HitTarget::SidebarToggle(_))
        | Some(HitTarget::Pane(Focus::Sidebar)) => Focus::Sidebar,
        Some(HitTarget::Pane(Focus::Comments)) if ui.comments_drawer_open => {
            Focus::Comments
        }
        Some(HitTarget::CanvasBlock(_))
        | Some(HitTarget::InlineEditorBody)
        | Some(HitTarget::TodoToggle(_))
        | Some(HitTarget::ToggleBlock(_))
        | Some(HitTarget::ChildCard(_))
        | Some(HitTarget::MediaChip(_))
        | Some(HitTarget::NotionButton(_))
        | Some(HitTarget::CodeHtmlPreview(_))
        | Some(HitTarget::DbViewTab(_))
        | Some(HitTarget::DbRow(_))
        | Some(HitTarget::DbBoardColumn(_))
        | Some(HitTarget::DbBoardCard { .. })
        | Some(HitTarget::Pane(Focus::Canvas)) => Focus::Canvas,
        _ => {
            if ui.comments_drawer_open
                && ui
                    .hitmap
                    .region_of(HitTarget::Pane(Focus::Comments))
                    .is_some_and(|r| r.contains(pos))
            {
                Focus::Comments
            } else if ui
                .hitmap
                .region_of(HitTarget::Pane(Focus::Sidebar))
                .is_some_and(|r| r.contains(pos))
            {
                Focus::Sidebar
            } else if ui
                .hitmap
                .region_of(HitTarget::Pane(Focus::Canvas))
                .is_some_and(|r| r.contains(pos))
            {
                Focus::Canvas
            } else {
                match ui.focus {
                    Focus::Comments if ui.comments_drawer_open => Focus::Comments,
                    Focus::Sidebar => Focus::Sidebar,
                    _ => Focus::Canvas,
                }
            }
        }
    };

    match target_pane {
        Focus::Sidebar => {
            let items = app.build_sidebar_items(ui);
            ui.sidebar_selected = step_sidebar_selection(&items, ui.sidebar_selected, delta);
        }
        Focus::Comments => {
            let mag = delta.unsigned_abs();
            if delta < 0 {
                ui.comments_scroll = ui.comments_scroll.saturating_sub(mag);
            } else {
                ui.comments_scroll = ui.comments_scroll.saturating_add(mag);
            }
        }
        Focus::Canvas | Focus::Overlay => match app.active_view {
            CanvasView::Page => {
                if let Some(ed) = ui.inline_editor.as_mut() {
                    ed.follow_cursor = false;
                    let mag = delta.unsigned_abs();
                    if delta < 0 {
                        ed.scroll_y = ed.scroll_y.saturating_sub(mag);
                    } else {
                        ed.scroll_y = ed.scroll_y.saturating_add(mag);
                    }
                } else {
                    ui.canvas_follow_selected = false;
                    let mag = delta.unsigned_abs();
                    if delta < 0 {
                        ui.canvas_scroll = ui.canvas_scroll.saturating_sub(mag);
                    } else {
                        ui.canvas_scroll = ui.canvas_scroll.saturating_add(mag);
                    }
                }
            }
            CanvasView::Database => match ui.db_view_mode {
                DatabaseViewMode::Table | DatabaseViewMode::List => {
                    let len = app.filtered_sorted_db_rows(ui).len();
                    let max_idx = len.saturating_sub(1);
                    let steps = delta.unsigned_abs() as usize;
                    if delta < 0 {
                        ui.db_selected_row = ui.db_selected_row.saturating_sub(steps);
                    } else {
                        ui.db_selected_row =
                            ui.db_selected_row.saturating_add(steps).min(max_idx);
                    }
                }
                DatabaseViewMode::Board => {
                    let cols = app.build_board_columns(ui);
                    let hovered_col = match hit {
                        Some(HitTarget::DbBoardCard { col, .. })
                        | Some(HitTarget::DbBoardColumn(col)) => Some(col),
                        _ => None,
                    };
                    if let Some(col_idx) = hovered_col {
                        if col_idx < cols.len() && ui.db_board_col != col_idx {
                            ui.db_board_col = col_idx;
                            ui.db_board_row = 0;
                        }
                    }
                    if !cols.is_empty() {
                        ui.db_board_col = ui.db_board_col.min(cols.len() - 1);
                    }
                    if let Some(col) = cols.get(ui.db_board_col) {
                        let max_row = col.cards.len().saturating_sub(1);
                        let cur = ui.db_board_row.min(max_row);
                        if delta < 0 {
                            ui.db_board_row = cur.saturating_sub(1);
                        } else {
                            ui.db_board_row = cur.saturating_add(1).min(max_row);
                        }
                    }
                }
            },
        },
    }
}

pub(super) fn handle_mouse_click(
    target: HitTarget,
    col: u16,
    row: u16,
    app: &mut AppState,
    ui: &mut UiState,
) -> bool {
    if !ui.overlays.is_empty() {
        match target {
            HitTarget::OverlayBody => {
                let outcome = match ui.overlays.last_mut() {
                    Some(Overlay::HtmlPreview(ov)) => ov.handle_click(col, row),
                    _ => return false,
                };
                match outcome {
                    OverlayOutcome::Handled | OverlayOutcome::NotHandled => return false,
                    OverlayOutcome::Close => {
                        let popped = ui.overlays.pop();
                        ui.active_pixel_graphic = None;
                        ui.pending_pixel_graphics.clear();
                        if matches!(
                            popped,
                            Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
                        ) {
                            ui.bump_preview_request_seq();
                        }
                        return false;
                    }
                    OverlayOutcome::Dispatch(next) => return dispatch(next, app, ui),
                    OverlayOutcome::CloseAndDispatch(next) => {
                        let popped = ui.overlays.pop();
                        ui.active_pixel_graphic = None;
                        ui.pending_pixel_graphics.clear();
                        if matches!(
                            popped,
                            Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
                        ) {
                            ui.bump_preview_request_seq();
                        }
                        return dispatch(next, app, ui);
                    }
                }
            }
            HitTarget::OverlayItem { index } => {
                if let Some(top) = ui.overlays.last_mut() {
                    top.select(index);
                }
                return dispatch(Action::Activate, app, ui);
            }
            HitTarget::Button(ButtonId::ConfirmOk) => {
                return dispatch(Action::Activate, app, ui);
            }
            HitTarget::Button(ButtonId::ConfirmCancel) => {
                return dispatch(Action::CloseOverlay, app, ui);
            }
            HitTarget::Button(ButtonId::EditExternal) => {
                return dispatch(Action::RequestExternalEditor, app, ui);
            }
            HitTarget::Button(ButtonId::HtmlToggleMode) => {
                if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last_mut() {
                    ov.toggle_raw_mode();
                }
                return false;
            }
            HitTarget::Button(ButtonId::HtmlServeBrowser) => {
                return dispatch(Action::ServeActiveHtml, app, ui);
            }
            _ => {
                let popped = ui.overlays.pop();
                ui.active_pixel_graphic = None;
                ui.pending_pixel_graphics.clear();
                if matches!(
                    popped,
                    Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
                ) {
                    ui.bump_preview_request_seq();
                }
                return false;
            }
        }
    }

    match target {
        HitTarget::Pane(focus) => {
            if focus != Focus::Canvas && ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            if focus != Focus::Overlay && (focus != Focus::Comments || ui.comments_drawer_open) {
                ui.focus = focus;
            }
        }
        HitTarget::SidebarRow(idx) => {
            ui.focus = Focus::Sidebar;
            ui.sidebar_selected = idx;
            return dispatch(Action::Activate, app, ui);
        }
        HitTarget::SidebarToggle(idx) => {
            ui.focus = Focus::Sidebar;
            let items = app.build_sidebar_items(ui);
            if let Some(item) = items.get(idx) {
                ui.sidebar_selected = idx;
                if item.collapsible && !ui.collapsed_nodes.remove(&item.id) {
                    ui.collapsed_nodes.insert(item.id.clone());
                }
            }
        }
        HitTarget::BreadcrumbSegment(idx) => {
            return dispatch(Action::NavigateBreadcrumb(idx), app, ui);
        }
        HitTarget::CanvasBlock(idx) => {
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            let visible = app.visible_blocks(ui);
            let already_editing_this = visible.get(idx).is_some_and(|vb| {
                ui.block_editor
                    .as_ref()
                    .is_some_and(|b| b.block_id == vb.block.id)
            });
            if !already_editing_this {
                let pos = ratatui::layout::Position::new(col, row);
                if let Some(url) = ui
                    .canvas_link_hits
                    .iter()
                    .find(|h| h.rect.contains(pos))
                    .map(|h| h.url.clone())
                {
                    if ui.block_editor.is_some() {
                        app.commit_and_sync_block_editor(ui, true);
                    }
                    return dispatch(Action::OpenUrl(url), app, ui);
                }
            }
            if visible
                .get(idx)
                .is_some_and(|vb| is_block_inline_editable(&vb.block.kind))
            {
                app.open_block_editor(ui, idx, BlockCursorInit::ScreenPos { col, row });
            } else if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
        }
        HitTarget::InlineEditorBody => {
            ui.focus = Focus::Canvas;
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.place_cursor_at_screen_pos(col, row);
            }
        }
        HitTarget::TodoToggle(idx) | HitTarget::ToggleBlock(idx) => {
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            let clicked_text_region = ui
                .canvas_block_layouts
                .iter()
                .find(|l| l.visible_idx == idx)
                .is_some_and(|lay| row > lay.first_screen_row || col >= lay.first_text_x);
            if clicked_text_region {
                let visible = app.visible_blocks(ui);
                let already_editing_this = visible.get(idx).is_some_and(|vb| {
                    ui.block_editor
                        .as_ref()
                        .is_some_and(|b| b.block_id == vb.block.id)
                });
                if !already_editing_this {
                    let pos = ratatui::layout::Position::new(col, row);
                    if let Some(url) = ui
                        .canvas_link_hits
                        .iter()
                        .find(|h| h.rect.contains(pos))
                        .map(|h| h.url.clone())
                    {
                        if ui.block_editor.is_some() {
                            app.commit_and_sync_block_editor(ui, true);
                        }
                        return dispatch(Action::OpenUrl(url), app, ui);
                    }
                }
                if visible
                    .get(idx)
                    .is_some_and(|vb| is_block_inline_editable(&vb.block.kind))
                {
                    app.open_block_editor(ui, idx, BlockCursorInit::ScreenPos { col, row });
                    return false;
                }
            }
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            return dispatch(Action::ToggleBlockAt(idx), app, ui);
        }
        HitTarget::ChildCard(idx) => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            ui.focus = Focus::Canvas;
            return dispatch(Action::ActivateChildBlockAt(idx), app, ui);
        }
        HitTarget::MediaChip(idx) => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            let visible = app.visible_blocks(ui);
            if let Some(vblk) = visible.get(idx) {
                match &vblk.block.kind {
                    NotionBlockKind::Image { url, caption, .. } => {
                        let block_id = vblk.block.id.clone();
                        let url = url.clone();
                        if is_html_or_xml_ref(&url, None) {
                            let cap = spans_plain_text(caption);
                            let title = if cap.trim().is_empty() {
                                "HTML Preview".to_string()
                            } else {
                                cap
                            };
                            let pid = app.active_page_id.clone();
                            return dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url,
                                    block_id: (!block_id.is_empty()).then_some(block_id),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            );
                        }
                        if let Some(detail) = app.active_page_detail.as_ref() {
                            let media_idx = find_media_index_for_block(detail, &block_id, &url);
                            if let Some(ov) =
                                ImagePreviewOverlay::from_notion_page(detail, media_idx)
                            {
                                ui.overlays.push(Overlay::ImagePreview(ov));
                                return dispatch(Action::LoadPreviewImage, app, ui);
                            }
                        }
                        ui.info("No images on the current page");
                        return false;
                    }
                    NotionBlockKind::Video { url, .. } | NotionBlockKind::Audio { url, .. } => {
                        let block_id = vblk.block.id.clone();
                        let u = url.clone();
                        return dispatch(
                            Action::OpenMediaUrl {
                                url: u,
                                block_id: (!block_id.is_empty()).then_some(block_id),
                                page_id: app.active_page_id.clone(),
                            },
                            app,
                            ui,
                        );
                    }
                    NotionBlockKind::Bookmark { url, .. }
                    | NotionBlockKind::Embed { url, .. }
                    | NotionBlockKind::LinkPreview { url } => {
                        let u = url.clone();
                        return dispatch(Action::OpenUrl(u), app, ui);
                    }
                    NotionBlockKind::File { url, name, .. } => {
                        let block_id = vblk.block.id.clone();
                        let u = url.clone();
                        if is_previewable_markup_ref(&u, name.as_deref()) {
                            let fallback = if is_markdown_file_ref(&u, name.as_deref()) {
                                "Markdown File"
                            } else {
                                "HTML File"
                            };
                            let title = name
                                .clone()
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| fallback.to_string());
                            let pid = app.active_page_id.clone();
                            return dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!block_id.is_empty()).then_some(block_id),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            );
                        }
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &block_id, &u));
                        if let Some(midx) = media_idx {
                            return dispatch(Action::DownloadMedia(midx), app, ui);
                        } else if !u.trim().is_empty() {
                            return dispatch(
                                Action::OpenMediaUrl {
                                    url: u,
                                    block_id: (!block_id.is_empty()).then_some(block_id),
                                    page_id: app.active_page_id.clone(),
                                },
                                app,
                                ui,
                            );
                        }
                        return false;
                    }
                    NotionBlockKind::Pdf { url, .. } => {
                        let block_id = vblk.block.id.clone();
                        let u = url.clone();
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &block_id, &u));
                        if let Some(midx) = media_idx {
                            return dispatch(Action::DownloadMedia(midx), app, ui);
                        } else if !u.trim().is_empty() {
                            return dispatch(
                                Action::OpenMediaUrl {
                                    url: u,
                                    block_id: (!block_id.is_empty()).then_some(block_id),
                                    page_id: app.active_page_id.clone(),
                                },
                                app,
                                ui,
                            );
                        }
                        return false;
                    }
                    _ => {}
                }
            }
            return dispatch(Action::OpenImagePreview, app, ui);
        }
        HitTarget::NotionButton(idx) => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            return dispatch(Action::ToggleActiveBlock, app, ui);
        }
        HitTarget::CodeHtmlPreview(idx) => {
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            let visible = app.visible_blocks(ui);
            if let Some(vblk) = visible.get(idx) {
                if let NotionBlockKind::Code {
                    rich_text,
                    language,
                    ..
                } = &vblk.block.kind
                {
                    let html = spans_plain_text(rich_text);
                    let page_title = app
                        .active_page_detail
                        .as_ref()
                        .map(|d| d.page.display_title().to_string())
                        .unwrap_or_else(|| "Code".to_string());
                    let title = format!("{page_title} ({language})");
                    return dispatch(Action::OpenHtmlPreview { title, html }, app, ui);
                }
            }
        }
        HitTarget::DbViewTab(mode) => {
            return dispatch(Action::SetDbViewMode(mode), app, ui);
        }
        HitTarget::DbRow(idx) => {
            ui.focus = Focus::Canvas;
            ui.db_selected_row = idx;
            return dispatch(Action::Activate, app, ui);
        }
        HitTarget::DbBoardColumn(col) => {
            ui.focus = Focus::Canvas;
            ui.db_board_col = col;
            ui.db_board_row = 0;
            app.clamp_db_selection(ui);
        }
        HitTarget::DbBoardCard { col, row } => {
            ui.focus = Focus::Canvas;
            ui.db_board_col = col;
            ui.db_board_row = row;
            return dispatch(Action::Activate, app, ui);
        }
        HitTarget::Button(btn) => match btn {
            ButtonId::WorkspaceSwitcher => {
                return dispatch(Action::OpenOverlay(OverlayKind::WorkspaceSwitcher), app, ui);
            }
            ButtonId::ToggleWidth => return dispatch(Action::TogglePageWidth, app, ui),
            ButtonId::SyncNow => return dispatch(Action::RefreshAll, app, ui),
            ButtonId::Help => return dispatch(Action::OpenOverlay(OverlayKind::Help), app, ui),
            ButtonId::Settings => {
                return dispatch(Action::OpenOverlay(OverlayKind::Settings), app, ui);
            }
            ButtonId::Search => return dispatch(Action::OpenSearch, app, ui),
            ButtonId::CommandPalette => {
                return dispatch(Action::OpenOverlay(OverlayKind::CommandPalette), app, ui);
            }
            ButtonId::ToggleSidebar => return dispatch(Action::ToggleSidebar, app, ui),
            ButtonId::ToggleFavorite => return dispatch(Action::ToggleFavorite, app, ui),
            ButtonId::ToggleComments => return dispatch(Action::ToggleCommentsDrawer, app, ui),
            ButtonId::QuickAppend => return dispatch(Action::OpenQuickAppend, app, ui),
            ButtonId::NewChildPage | ButtonId::DbNewItem => {
                return dispatch(Action::OpenNewPageModal, app, ui);
            }
            ButtonId::EditPage => return dispatch(Action::OpenPageEditor, app, ui),
            ButtonId::EditExternal => return dispatch(Action::RequestExternalEditor, app, ui),
            ButtonId::ToggleEditorMode => return dispatch(Action::ToggleEditorMode, app, ui),
            ButtonId::FinishInlineEdit => return dispatch(Action::FinishInlineEdit, app, ui),
            ButtonId::SaveInlineEdit => return dispatch(Action::SaveInlineEdit, app, ui),
            ButtonId::OpenWeb => return dispatch(Action::OpenActiveInBrowser, app, ui),
            ButtonId::LinkExtractor => return dispatch(Action::OpenLinkExtractor, app, ui),
            ButtonId::ToggleHrefs => return dispatch(Action::ToggleShowLinkUrls, app, ui),
            ButtonId::ImagePreview => return dispatch(Action::OpenImagePreview, app, ui),
            ButtonId::Attachments => {
                return dispatch(Action::OpenOverlay(OverlayKind::Attachments), app, ui);
            }
            ButtonId::DbFilter => return dispatch(Action::OpenDbFilter, app, ui),
            ButtonId::DbSort => return dispatch(Action::CycleDbSort, app, ui),
            ButtonId::NarrowSidebar => {
                ui.narrow_pane = NarrowPane::Sidebar;
                ui.focus = Focus::Sidebar;
                app.snap_sidebar_selection(ui);
            }
            ButtonId::NarrowCanvas => {
                ui.narrow_pane = NarrowPane::Canvas;
                ui.focus = Focus::Canvas;
            }
            ButtonId::ConfirmOk
            | ButtonId::ConfirmCancel
            | ButtonId::HtmlToggleMode
            | ButtonId::HtmlServeBrowser => {}
        },
        HitTarget::OverlayBody | HitTarget::OverlayItem { .. } => {}
    }

    false
}

