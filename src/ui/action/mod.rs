//! Action definitions & central Notion TUI action dispatcher (`src/ui/action/`).

mod editor;
mod mouse;
mod navigation;
mod overlays;

#[cfg(test)]
mod tests;

use std::path::PathBuf;

use crate::{
    config::ThemePreset,
    models::{DatabaseViewMode, NotionPage},
    sync::SyncCommand,
    ui::{
        app::{AppState, CanvasView, Focus, UiState},
        hitmap::HitTarget,
        overlays::{Overlay, OverlayKind, OverlayOutcome},
    },
};

pub(crate) use self::overlays::{is_html_code_language, is_html_file_ref, is_markdown_file_ref};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Noop,
    Quit,
    QuitConfirm,
    NextFocus,
    PrevFocus,
    SetFocus(Focus),
    SidebarCollapseOrPrevFocus,
    SidebarExpandOrNextFocus,
    MoveUp,
    MoveDown,
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,
    Activate,
    Tab,
    Input(char),
    Newline,
    SoftNewline,
    Backspace,
    Delete,
    Undo,
    Redo,
    Paste(String),
    PasteClipboard,
    OverlaySubmit,
    CloseOverlay,
    OpenOverlay(OverlayKind),
    OpenSearch,
    OpenFilePicker,
    OpenContextMenu { col: u16, row: u16 },
    RightClick { col: u16, row: u16 },
    OpenLinkExtractor,
    OpenImagePreview,
    OpenImagePreviewAt(usize),
    OpenHtmlPreview {
        title: String,
        html: String,
    },
    FetchHtmlPreview {
        title: String,
        url: String,
        block_id: Option<String>,
        page_id: Option<String>,
    },
    ServeActiveHtml,
    ShowToast(String),
    OpenQuickAppend,
    OpenPageEditor,
    OpenBlockEditor,
    ToggleEditorMode,
    FinishInlineEdit,
    CancelInlineEdit,
    SaveInlineEdit,
    OpenNewPageModal,
    OpenDbFilter,
    ConfirmArchivePage,
    ConfirmArchivePageById { id: String, title: String },
    ArchiveActivePage,
    ArchivePageById(String),
    RequestExternalEditor,
    ToggleSidebar,
    TogglePageWidth,
    ToggleDensity,
    ToggleFavorite,
    ToggleSidebarFavorite { id: String, is_database: bool },
    ToggleCommentsDrawer,
    ToggleShowLinkUrls,
    ToggleActiveBlock,
    ToggleBlockAt(usize),
    ActivateChildBlockAt(usize),
    SubmitCommentReply,
    SubmitSlashAppend(String),
    SubmitPageEdit(String),
    SubmitCreatePage {
        title: String,
        icon: Option<String>,
        body: String,
        in_database: bool,
    },
    ApplyDbFilter(String),
    SetDbViewMode(DatabaseViewMode),
    CycleDbSort,
    BoardPrevColumn,
    BoardNextColumn,
    BoardMoveCardNextColumn,
    NavigateBack,
    NavigateBreadcrumb(usize),
    OpenPageById(String),
    OpenDatabaseById(String),
    SwitchWorkspace(String),
    RefreshAll,
    /// Rescans the whole workspace, ignoring the last sync point, then reloads the tree.
    FullResync,
    OpenActiveInBrowser,
    OpenUrl(String),
    OpenMediaUrl {
        url: String,
        block_id: Option<String>,
        page_id: Option<String>,
    },
    CopyUrlToClipboard(String),
    CopySelectionOrMarkdown,
    AttachFormFile(PathBuf),
    LoadPreviewImage,
    CycleImageProtocol,
    DownloadMedia(usize),
    SetTheme(ThemePreset),
    ToggleInlineImages,
    ToggleNotify,
    CycleSyncInterval,
    MouseClick { target: HitTarget, col: u16, row: u16 },
    MouseScroll { delta: i16, column: u16, row: u16 },
}


/// Dispatches an `Action` against `(AppState, UiState)`. Returns `true` if TUI should exit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if matches!(action, Action::Quit) {
        app.commit_and_sync_block_editor(ui, true);
        app.close_inline_editor_and_flush(ui);
        app.html_server.shutdown();
        app.sync_ctrl.request(SyncCommand::Shutdown);
        return true;
    }

    if matches!(action, Action::QuitConfirm) {
        if !matches!(ui.overlays.last(), Some(Overlay::Confirm(_))) {
            ui.overlays.push(Overlay::confirm(
                "Quit @bitefoundry/notion TUI?",
                Some(Action::Quit),
            ));
        }
        return false;
    }

    // Mouse clicks on overlay items or outside overlay
    if let Action::MouseClick { target, col, row } = action.clone() {
        return mouse::handle_mouse_click(target, col, row, app, ui);
    }

    // Route through top overlay first when present — except internal effect actions
    // (e.g., `LoadPreviewImage`, `CycleImageProtocol`, `DownloadMedia`, `OpenUrl`) which must reach the core dispatcher.
    let bypass_overlay = matches!(
        action,
        Action::LoadPreviewImage
            | Action::CycleImageProtocol
            | Action::DownloadMedia(_)
            | Action::OpenHtmlPreview { .. }
            | Action::FetchHtmlPreview { .. }
            | Action::ServeActiveHtml
            | Action::ShowToast(_)
            | Action::OpenUrl(_)
            | Action::OpenMediaUrl { .. }
            | Action::CopyUrlToClipboard(_)
            | Action::SetTheme(_)
            | Action::TogglePageWidth
            | Action::ToggleShowLinkUrls
            | Action::ToggleInlineImages
            | Action::ToggleNotify
            | Action::CycleSyncInterval
            | Action::SwitchWorkspace(_)
            | Action::AttachFormFile(_)
            | Action::SubmitSlashAppend(_)
            | Action::SubmitPageEdit(_)
            | Action::SubmitCreatePage { .. }
            | Action::ArchiveActivePage
            | Action::ArchivePageById(_)
            | Action::ConfirmArchivePageById { .. }
            | Action::ToggleSidebarFavorite { .. }
            | Action::OpenImagePreviewAt(_)
    );

    if !bypass_overlay {
        if let Some(mut top) = ui.overlays.pop() {
            let was_preview = matches!(top, Overlay::ImagePreview(_) | Overlay::HtmlPreview(_));
            let outcome = top.handle(&action, app, ui);
            match outcome {
                OverlayOutcome::Handled => {
                    ui.overlays.push(top);
                    return false;
                }
                OverlayOutcome::Close => {
                    ui.active_pixel_graphic = None;
                    if was_preview {
                        ui.bump_preview_request_seq();
                    }
                    return false;
                }
                OverlayOutcome::Dispatch(next) => {
                    ui.overlays.push(top);
                    return dispatch(next, app, ui);
                }
                OverlayOutcome::CloseAndDispatch(next) => {
                    ui.active_pixel_graphic = None;
                    if was_preview {
                        ui.bump_preview_request_seq();
                    }
                    return dispatch(next, app, ui);
                }
                OverlayOutcome::NotHandled => {
                    ui.overlays.push(top);
                }
            }
        }
    }

    // Route typing, cursor navigation, paste, and undo/redo to the active in-place inline editor
    // (either Notion Native Block-Live editor or full-page Markdown editor) when Canvas is focused
    // and no modal overlay is active.
    if ui.overlays.is_empty() && ui.focus == Focus::Canvas {
        if ui.block_editor.is_some() && editor::handle_block_editor_action(&action, app, ui) {
            return false;
        }
        if ui.inline_editor.is_some() && editor::handle_inline_editor_action(&action, app, ui) {
            return false;
        }
    }

    let action = match navigation::try_dispatch(action, app, ui) {
        Ok(quit) => return quit,
        Err(action) => action,
    };
    let action = match overlays::try_dispatch(action, app, ui) {
        Ok(quit) => return quit,
        Err(action) => action,
    };
    let action = match editor::try_dispatch(action, app, ui) {
        Ok(quit) => return quit,
        Err(action) => action,
    };
    if let Action::MouseScroll { delta, column, row } = action {
        mouse::handle_mouse_scroll(delta, column, row, app, ui);
    }

    false
}

pub(super) fn open_url_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let _ = url;
        Ok(())
    }
    #[cfg(not(test))]
    {
        open::that(url)
    }
}

pub(super) fn effective_canvas_width(ui: &UiState) -> u16 {
    if let Some(rect) = ui.hitmap.region_of(HitTarget::Pane(Focus::Canvas)) {
        if ui.comments_drawer_open {
            if let Some(comments_rect) = ui.hitmap.region_of(HitTarget::Pane(Focus::Comments)) {
                return rect
                    .width
                    .saturating_add(comments_rect.width)
                    .saturating_add(1);
            }
        }
        return rect.width;
    }
    if ui.term_width < 80 {
        ui.term_width
    } else if ui.show_sidebar {
        let nav_w = if ui.term_width >= 120 { 28 } else { 24 };
        ui.term_width.saturating_sub(nav_w + 1)
    } else {
        ui.term_width
    }
}

pub(super) fn can_show_comments_drawer(app: &AppState, ui: &UiState) -> bool {
    app.active_view == CanvasView::Page && effective_canvas_width(ui) >= 72
}

pub(super) fn selected_db_row_page(app: &AppState, ui: &UiState) -> Option<NotionPage> {
    match ui.db_view_mode {
        DatabaseViewMode::Board => {
            let cols = app.build_board_columns(ui);
            if cols.is_empty() {
                return None;
            }
            let col_idx = ui.db_board_col.min(cols.len() - 1);
            let col = &cols[col_idx];
            if col.cards.is_empty() {
                return None;
            }
            let row_idx = ui.db_board_row.min(col.cards.len() - 1);
            col.cards.get(row_idx).cloned()
        }
        DatabaseViewMode::Table | DatabaseViewMode::List => {
            let rows = app.filtered_sorted_db_rows(ui);
            if rows.is_empty() {
                return None;
            }
            let row_idx = ui.db_selected_row.min(rows.len() - 1);
            rows.get(row_idx).cloned()
        }
    }
}

pub(super) fn find_media_index_for_block(
    detail: &crate::models::NotionPageDetail,
    block_id: &str,
    url: &str,
) -> Option<usize> {
    detail
        .media
        .iter()
        .position(|m| !block_id.is_empty() && m.block_id == block_id)
        .or_else(|| {
            detail
                .media
                .iter()
                .position(|m| !url.is_empty() && m.url == url)
        })
}

pub(super) fn step_sidebar_selection(
    items: &[crate::ui::app::SidebarItem],
    current: usize,
    delta: i16,
) -> usize {
    if items.is_empty() {
        return 0;
    }
    let max_idx = items.len().saturating_sub(1);
    let mut idx = current.min(max_idx);
    if delta == 0 {
        if !items[idx].is_section_header {
            return idx;
        }
        return ((idx + 1)..items.len())
            .find(|&i| !items[i].is_section_header)
            .or_else(|| (0..idx).rev().find(|&i| !items[i].is_section_header))
            .unwrap_or(0);
    }
    let steps = delta.unsigned_abs() as usize;
    if delta > 0 {
        for _ in 0..steps {
            if let Some(next) = ((idx + 1)..items.len()).find(|&i| !items[i].is_section_header) {
                idx = next;
            } else {
                break;
            }
        }
    } else {
        for _ in 0..steps {
            if let Some(prev) = (0..idx).rev().find(|&i| !items[i].is_section_header) {
                idx = prev;
            } else {
                break;
            }
        }
    }
    if items[idx].is_section_header {
        if let Some(first) = items.iter().position(|it| !it.is_section_header) {
            idx = first;
        }
    }
    idx
}

pub(super) fn copy_to_clipboard(text: &str, ok_msg: &str, ui: &mut UiState) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => ui.success(ok_msg),
        Err(err) => ui.error(format!("Clipboard error: {err}")),
    }
}

