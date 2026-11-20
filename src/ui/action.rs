//! Action definitions & central Notion TUI action dispatcher (`src/ui/action.rs`).

use std::path::PathBuf;

use serde_json::json;

use crate::{
    config::{ImageProtocolPreference, ThemePreset},
    models::{DatabaseViewMode, NotionBlockKind, NotionPage},
    notion::media,
    parser::parse_slash_command_blocks,
    sync::SyncCommand,
    ui::{
        app::{AppState, CanvasView, Focus, NarrowPane, UiMsg, UiState},
        hitmap::{ButtonId, HitTarget},
        overlays::{
            context_menu::{ContextMenu, ContextTarget},
            image_preview::ImagePreviewOverlay,
            links::LinkExtractorOverlay,
            notion_form::{NotionFormMode, NotionFormOverlay},
            Overlay, OverlayKind, OverlayOutcome,
        },
    },
};

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
    Backspace,
    Delete,
    Paste(String),
    PasteClipboard,
    OverlaySubmit,
    CloseOverlay,
    OpenOverlay(OverlayKind),
    OpenSearch,
    OpenFilePicker,
    OpenContextMenu { col: u16, row: u16 },
    OpenLinkExtractor,
    OpenImagePreview,
    OpenQuickAppend,
    OpenNewPageModal,
    OpenDbFilter,
    ConfirmArchivePage,
    ArchiveActivePage,
    RequestExternalEditor,
    ToggleSidebar,
    TogglePageWidth,
    ToggleDensity,
    ToggleFavorite,
    ToggleCommentsDrawer,
    ToggleShowLinkUrls,
    ToggleActiveBlock,
    ToggleBlockAt(usize),
    ActivateChildBlockAt(usize),
    SubmitCommentReply,
    SubmitSlashAppend(String),
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
    OpenActiveInBrowser,
    OpenUrl(String),
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
        return handle_mouse_click(target, col, row, app, ui);
    }

    // Route through top overlay first when present — except internal effect actions
    // (e.g., `LoadPreviewImage`, `CycleImageProtocol`, `DownloadMedia`, `OpenUrl`) which must reach the core dispatcher.
    let bypass_overlay = matches!(
        action,
        Action::LoadPreviewImage
            | Action::CycleImageProtocol
            | Action::DownloadMedia(_)
            | Action::OpenUrl(_)
            | Action::CopyUrlToClipboard(_)
            | Action::SetTheme(_)
            | Action::ToggleInlineImages
            | Action::ToggleNotify
            | Action::CycleSyncInterval
            | Action::SwitchWorkspace(_)
            | Action::AttachFormFile(_)
            | Action::SubmitSlashAppend(_)
            | Action::SubmitCreatePage { .. }
            | Action::ArchiveActivePage
    );

    if !bypass_overlay {
        if let Some(mut top) = ui.overlays.pop() {
            let outcome = top.handle(&action, app, ui);
            match outcome {
                OverlayOutcome::Handled => {
                    ui.overlays.push(top);
                    return false;
                }
                OverlayOutcome::Close => {
                    ui.active_pixel_graphic = None;
                    return false;
                }
                OverlayOutcome::Dispatch(next) => {
                    ui.overlays.push(top);
                    return dispatch(next, app, ui);
                }
                OverlayOutcome::CloseAndDispatch(next) => {
                    ui.active_pixel_graphic = None;
                    return dispatch(next, app, ui);
                }
                OverlayOutcome::NotHandled => {
                    ui.overlays.push(top);
                }
            }
        }
    }

    match action {
        Action::Noop => {}
        Action::CloseOverlay => {
            if ui.comments_drawer_open {
                ui.comments_drawer_open = false;
                ui.focus = Focus::Canvas;
            }
        }
        Action::NextFocus => {
            ui.focus = match ui.focus {
                Focus::Sidebar => Focus::Canvas,
                Focus::Canvas => {
                    if ui.comments_drawer_open {
                        Focus::Comments
                    } else if ui.show_sidebar {
                        Focus::Sidebar
                    } else {
                        Focus::Canvas
                    }
                }
                Focus::Comments => {
                    if ui.show_sidebar {
                        Focus::Sidebar
                    } else {
                        Focus::Canvas
                    }
                }
                Focus::Overlay => Focus::Canvas,
            };
        }
        Action::PrevFocus => {
            ui.focus = match ui.focus {
                Focus::Sidebar => {
                    if ui.comments_drawer_open {
                        Focus::Comments
                    } else {
                        Focus::Canvas
                    }
                }
                Focus::Canvas => {
                    if ui.show_sidebar {
                        Focus::Sidebar
                    } else if ui.comments_drawer_open {
                        Focus::Comments
                    } else {
                        Focus::Canvas
                    }
                }
                Focus::Comments => Focus::Canvas,
                Focus::Overlay => Focus::Canvas,
            };
        }
        Action::SetFocus(f) => {
            ui.focus = f;
        }
        Action::SidebarCollapseOrPrevFocus => {
            if ui.focus == Focus::Sidebar {
                let items = app.build_sidebar_items(ui);
                if let Some(item) = items.get(ui.sidebar_selected) {
                    if item.collapsible && !ui.collapsed_nodes.contains(&item.id) {
                        ui.collapsed_nodes.insert(item.id.clone());
                        return false;
                    }
                }
            } else {
                ui.focus = Focus::Sidebar;
            }
        }
        Action::SidebarExpandOrNextFocus => {
            if ui.focus == Focus::Sidebar {
                let items = app.build_sidebar_items(ui);
                if let Some(item) = items.get(ui.sidebar_selected) {
                    if item.collapsible && ui.collapsed_nodes.contains(&item.id) {
                        ui.collapsed_nodes.remove(&item.id);
                        return false;
                    }
                }
                ui.focus = Focus::Canvas;
            } else if ui.comments_drawer_open {
                ui.focus = Focus::Comments;
            }
        }
        Action::MoveUp => match ui.focus {
            Focus::Sidebar => {
                let len = app.build_sidebar_items(ui).len();
                if len > 0 {
                    ui.sidebar_selected = ui.sidebar_selected.saturating_sub(1);
                }
            }
            Focus::Canvas => match app.active_view {
                CanvasView::Page => {
                    if ui.selected_block > 0 {
                        ui.selected_block -= 1;
                    }
                    ui.canvas_scroll = ui.canvas_scroll.saturating_sub(1);
                }
                CanvasView::Database => match ui.db_view_mode {
                    DatabaseViewMode::Board => {
                        ui.db_board_row = ui.db_board_row.saturating_sub(1);
                    }
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        ui.db_selected_row = ui.db_selected_row.saturating_sub(1);
                    }
                },
            },
            Focus::Comments => {
                ui.comments_scroll = ui.comments_scroll.saturating_sub(1);
            }
            Focus::Overlay => {}
        },
        Action::MoveDown => match ui.focus {
            Focus::Sidebar => {
                let len = app.build_sidebar_items(ui).len();
                if ui.sidebar_selected + 1 < len {
                    ui.sidebar_selected += 1;
                }
            }
            Focus::Canvas => match app.active_view {
                CanvasView::Page => {
                    let count = app.visible_blocks(ui).len();
                    if ui.selected_block + 1 < count {
                        ui.selected_block += 1;
                    }
                }
                CanvasView::Database => match ui.db_view_mode {
                    DatabaseViewMode::Board => {
                        let cols = app.build_board_columns(ui);
                        if let Some(col) = cols.get(ui.db_board_col) {
                            if ui.db_board_row + 1 < col.cards.len() {
                                ui.db_board_row += 1;
                            }
                        }
                    }
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        let len = app.filtered_sorted_db_rows(ui).len();
                        if ui.db_selected_row + 1 < len {
                            ui.db_selected_row += 1;
                        }
                    }
                },
            },
            Focus::Comments => {
                ui.comments_scroll = ui.comments_scroll.saturating_add(1);
            }
            Focus::Overlay => {}
        },
        Action::PageUp => {
            ui.canvas_scroll = ui.canvas_scroll.saturating_sub(12);
            ui.selected_block = ui.selected_block.saturating_sub(6);
        }
        Action::PageDown => {
            ui.canvas_scroll = ui.canvas_scroll.saturating_add(12);
            let max_b = app.visible_blocks(ui).len().saturating_sub(1);
            ui.selected_block = (ui.selected_block + 6).min(max_b);
        }
        Action::ScrollTop => {
            ui.canvas_scroll = 0;
            ui.selected_block = 0;
            ui.db_selected_row = 0;
        }
        Action::ScrollBottom => {
            let max_b = app.visible_blocks(ui).len().saturating_sub(1);
            ui.selected_block = max_b;
            ui.canvas_scroll = u16::MAX / 2;
        }
        Action::Activate => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                if let Some(item) = items.get(ui.sidebar_selected).cloned() {
                    if item.is_database {
                        app.open_database(&item.id, ui, true);
                    } else {
                        app.open_page(&item.id, ui, true);
                    }
                    ui.focus = Focus::Canvas;
                    ui.narrow_pane = NarrowPane::Canvas;
                }
            }
            Focus::Canvas => match app.active_view {
                CanvasView::Page => {
                    return dispatch(Action::ToggleActiveBlock, app, ui);
                }
                CanvasView::Database => {
                    if let Some(page) = selected_db_row_page(app, ui) {
                        app.open_page(&page.id, ui, true);
                    }
                }
            },
            Focus::Comments => {
                return dispatch(Action::SubmitCommentReply, app, ui);
            }
            Focus::Overlay => {}
        },
        Action::ToggleActiveBlock => {
            let idx = ui.selected_block;
            let visible = app.visible_blocks(ui);
            if let Some(vblk) = visible.get(idx) {
                match &vblk.block.kind {
                    NotionBlockKind::Toggle { .. }
                    | NotionBlockKind::Heading1 { is_toggleable: true, .. }
                    | NotionBlockKind::Heading2 { is_toggleable: true, .. }
                    | NotionBlockKind::Heading3 { is_toggleable: true, .. } => {
                        let id = vblk.block.id.clone();
                        if !ui.expanded_toggles.remove(&id) {
                            ui.expanded_toggles.insert(id);
                        }
                    }
                    NotionBlockKind::ToDo { checked, .. } => {
                        let id = vblk.block.id.clone();
                        let next = !*checked;
                        app.toggle_todo_block(&id, next, ui);
                    }
                    NotionBlockKind::ChildPage { .. } => {
                        let id = vblk.block.id.clone();
                        app.open_page(&id, ui, true);
                    }
                    NotionBlockKind::ChildDatabase { .. } => {
                        let id = vblk.block.id.clone();
                        app.open_database(&id, ui, true);
                    }
                    NotionBlockKind::Image { .. } => {
                        return dispatch(Action::OpenImagePreview, app, ui);
                    }
                    NotionBlockKind::Video { url, .. }
                    | NotionBlockKind::Audio { url, .. }
                    | NotionBlockKind::Bookmark { url, .. }
                    | NotionBlockKind::Embed { url, .. }
                    | NotionBlockKind::LinkPreview { url } => {
                        let u = url.clone();
                        return dispatch(Action::OpenUrl(u), app, ui);
                    }
                    NotionBlockKind::File { url, .. } | NotionBlockKind::Pdf { url, .. } => {
                        let bid = vblk.block.id.clone();
                        let u = url.clone();
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &bid, &u));
                        if let Some(midx) = media_idx {
                            return dispatch(Action::DownloadMedia(midx), app, ui);
                        } else if !u.trim().is_empty() {
                            return dispatch(Action::OpenUrl(u), app, ui);
                        }
                    }
                    _ => {}
                }
            }
        }
        Action::ToggleBlockAt(idx) => {
            ui.selected_block = idx;
            return dispatch(Action::ToggleActiveBlock, app, ui);
        }
        Action::ActivateChildBlockAt(idx) => {
            ui.selected_block = idx;
            return dispatch(Action::ToggleActiveBlock, app, ui);
        }
        Action::ToggleSidebar => {
            ui.show_sidebar = !ui.show_sidebar;
            app.config.show_sidebar = ui.show_sidebar;
            let _ = app.config.save();
            if !ui.show_sidebar && ui.focus == Focus::Sidebar {
                ui.focus = Focus::Canvas;
            }
        }
        Action::TogglePageWidth => {
            ui.page_width = ui.page_width.toggle();
            app.config.page_width = ui.page_width;
            let _ = app.config.save();
            ui.info(format!("Page width: {}", ui.page_width.label()));
        }
        Action::ToggleDensity => {
            ui.density = ui.density.toggle();
            app.config.density = ui.density;
            let _ = app.config.save();
            ui.info(format!("Density: {}", ui.density.label()));
        }
        Action::ToggleFavorite => {
            let ws = app.workspace_id.clone();
            match app.active_view {
                CanvasView::Page => {
                    if let Some(ref pid) = app.active_page_id.clone() {
                        if let Ok(fav) = app.storage.toggle_page_favorite(&ws, pid) {
                            if let Some(p) = app.pages.iter_mut().find(|p| p.id == *pid) {
                                p.is_favorite = fav;
                            }
                            if let Some(ref mut d) = app.active_page_detail {
                                d.page.is_favorite = fav;
                            }
                            ui.success(if fav {
                                "Added to Favorites ★"
                            } else {
                                "Removed from Favorites"
                            });
                        }
                    }
                }
                CanvasView::Database => {
                    if let Some(ref did) = app.active_database_id.clone() {
                        if let Ok(fav) = app.storage.toggle_database_favorite(&ws, did) {
                            if let Some(d) = app.databases.iter_mut().find(|d| d.id == *did) {
                                d.is_favorite = fav;
                            }
                            ui.success(if fav {
                                "Database added to Favorites ★"
                            } else {
                                "Database removed from Favorites"
                            });
                        }
                    }
                }
            }
        }
        Action::ToggleCommentsDrawer => {
            ui.comments_drawer_open = !ui.comments_drawer_open;
            if ui.comments_drawer_open {
                ui.focus = Focus::Comments;
            } else if ui.focus == Focus::Comments {
                ui.focus = Focus::Canvas;
            }
        }
        Action::Input(ch) if ui.focus == Focus::Comments => {
            ui.comment_reply_input.push(ch);
        }
        Action::Backspace if ui.focus == Focus::Comments => {
            ui.comment_reply_input.pop();
        }
        Action::SubmitCommentReply => {
            let text = ui.comment_reply_input.trim().to_string();
            if text.is_empty() {
                return false;
            }
            let Some(page_id) = app.active_page_id.clone() else {
                ui.warning("Open a page to add a comment");
                return false;
            };
            ui.comment_reply_input.clear();
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            ui.tasks.begin("comment");
            tokio::spawn(async move {
                let res = crate::cli::op_add_comment(&client, &storage, &page_id, None, &text).await;
                match res {
                    Ok(comment) => {
                        let _ = ui_tx.send(UiMsg::CommentAdded { page_id, comment });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "comment".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::ToggleShowLinkUrls => {
            ui.show_inline_hrefs = !ui.show_inline_hrefs;
            app.config.show_inline_hrefs = ui.show_inline_hrefs;
            let _ = app.config.save();
            ui.info(if ui.show_inline_hrefs {
                "Inline link URLs: ON"
            } else {
                "Inline link URLs: OFF"
            });
        }
        Action::OpenOverlay(kind) => match kind {
            OverlayKind::CommandPalette => ui.overlays.push(Overlay::command_palette()),
            OverlayKind::WorkspaceSwitcher => ui.overlays.push(Overlay::workspace_switcher()),
            OverlayKind::Settings => ui.overlays.push(Overlay::settings()),
            OverlayKind::Help => ui.overlays.push(Overlay::help()),
            OverlayKind::Search => ui.overlays.push(Overlay::search()),
            OverlayKind::Attachments => ui.overlays.push(Overlay::attachments()),
            OverlayKind::FilePicker => ui.overlays.push(Overlay::file_picker()),
            OverlayKind::PathInput => ui.overlays.push(Overlay::path_input()),
            _ => {}
        },
        Action::OpenSearch => {
            ui.overlays.push(Overlay::search());
        }
        Action::OpenFilePicker => {
            ui.overlays.push(Overlay::file_picker());
        }
        Action::OpenContextMenu { col, row } => {
            let target = match ui.focus {
                Focus::Sidebar => ContextTarget::Sidebar,
                Focus::Canvas if app.active_view == CanvasView::Database => ContextTarget::Database,
                Focus::Canvas => ContextTarget::PageCanvas,
                _ => ContextTarget::Global,
            };
            let menu = ContextMenu::build(target, (col, row), app, ui);
            ui.overlays.push(Overlay::context_menu(menu));
        }
        Action::OpenLinkExtractor => {
            if let Some(detail) = app.active_page_detail.as_ref() {
                ui.overlays
                    .push(Overlay::links(LinkExtractorOverlay::from_notion_page(detail)));
            } else {
                ui.info("Open a Notion page to extract links");
            }
        }
        Action::OpenImagePreview => {
            if let Some(detail) = app.active_page_detail.as_ref() {
                let visible = app.visible_blocks(ui);
                let preferred_media_idx = visible.get(ui.selected_block).and_then(|vblk| {
                    if let NotionBlockKind::Image { url, .. } = &vblk.block.kind {
                        find_media_index_for_block(detail, &vblk.block.id, url)
                    } else {
                        None
                    }
                });
                if let Some(ov) = ImagePreviewOverlay::from_notion_page(detail, preferred_media_idx)
                {
                    ui.overlays.push(Overlay::ImagePreview(ov));
                    return dispatch(Action::LoadPreviewImage, app, ui);
                } else {
                    ui.info("No images on the current page");
                }
            } else {
                ui.info("Open a Notion page with images first");
            }
        }
        Action::OpenQuickAppend => {
            if app.active_page_id.is_none() {
                ui.warning("Open a page before appending blocks");
                return false;
            }
            ui.overlays
                .push(Overlay::notion_form(NotionFormOverlay::new(
                    NotionFormMode::QuickAppend,
                )));
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
            ui.db_board_row = 0;
        }
        Action::SubmitSlashAppend(text) => {
            let Some(page_id) = app.active_page_id.clone() else {
                return false;
            };
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            ui.tasks.begin("append");
            tokio::spawn(async move {
                let res = crate::cli::op_append_blocks(&client, &storage, &page_id, &text).await;
                match res {
                    Ok(_) => {
                        if let Ok(detail) =
                            crate::sync::sync_page_detail(&client, &storage, &page_id).await
                        {
                            let _ = ui_tx.send(UiMsg::PageRefreshed(Box::new(detail)));
                        } else {
                            let _ = ui_tx.send(UiMsg::TaskDone {
                                label: "append".into(),
                                toast: "Appended blocks".into(),
                            });
                        }
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "append".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
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
                return false;
            };
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            ui.tasks.begin("create");
            tokio::spawn(async move {
                let res = crate::cli::op_create_page(
                    &client,
                    &storage,
                    &parent,
                    &title,
                    icon.as_deref(),
                    (!body.trim().is_empty()).then_some(body.as_str()),
                    in_database,
                    None,
                )
                .await;
                match res {
                    Ok(created) => {
                        let _ = ui_tx.send(UiMsg::PageCreated(created));
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
        Action::ConfirmArchivePage => {
            if let Some(detail) = app.active_page_detail.as_ref() {
                ui.overlays.push(Overlay::confirm(
                    format!("Archive page '{}' in Notion?", detail.page.display_title()),
                    Some(Action::ArchiveActivePage),
                ));
            }
        }
        Action::ArchiveActivePage => {
            let Some(page_id) = app.active_page_id.clone() else {
                return false;
            };
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
            if app.active_page_id.is_some() {
                ui.editor_requested = true;
            } else {
                ui.warning("Open a page to edit in $EDITOR");
            }
        }
        Action::SetDbViewMode(mode) => {
            ui.db_view_mode = mode;
            ui.info(format!("Database view: {}", mode.label()));
        }
        Action::CycleDbSort => {
            ui.db_sort = ui.db_sort.next();
            ui.info(format!("Sort: {}", ui.db_sort.label()));
        }
        Action::BoardPrevColumn => {
            ui.db_board_col = ui.db_board_col.saturating_sub(1);
            ui.db_board_row = 0;
        }
        Action::BoardNextColumn => {
            let count = app.build_board_columns(ui).len();
            if ui.db_board_col + 1 < count {
                ui.db_board_col += 1;
                ui.db_board_row = 0;
            }
        }
        Action::BoardMoveCardNextColumn => {
            let cols = app.build_board_columns(ui);
            if cols.len() < 2 {
                ui.info("Need at least 2 status columns to move card");
                return false;
            }
            let Some(cur_col) = cols.get(ui.db_board_col) else {
                return false;
            };
            let Some(card) = cur_col.cards.get(ui.db_board_row).cloned() else {
                return false;
            };
            let next_col_idx = (ui.db_board_col + 1) % cols.len();
            let target_col = &cols[next_col_idx];
            let prop_name = target_col.property_name.clone();
            let prop_type = target_col.property_type.clone();
            let target_status = target_col.label.clone();

            let prop_val = if prop_type == "status" {
                json!({ prop_name: { "status": { "name": target_status } } })
            } else {
                json!({ prop_name: { "select": { "name": target_status } } })
            };

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
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "move card".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
            ui.db_board_col = next_col_idx;
            ui.db_board_row = 0;
        }
        Action::NavigateBack => {
            if let Some(prev) = app.nav_stack.pop() {
                match prev {
                    crate::ui::app::NavTarget::Page(pid) => app.open_page(&pid, ui, false),
                    crate::ui::app::NavTarget::Database(did) => app.open_database(&did, ui, false),
                }
            }
        }
        Action::NavigateBreadcrumb(idx) => {
            if idx < app.nav_stack.len() {
                let target = app.nav_stack[idx].clone();
                app.nav_stack.truncate(idx);
                match target {
                    crate::ui::app::NavTarget::Page(pid) => app.open_page(&pid, ui, false),
                    crate::ui::app::NavTarget::Database(did) => app.open_database(&did, ui, false),
                }
            }
        }
        Action::OpenPageById(pid) => {
            app.open_page(&pid, ui, true);
            ui.focus = Focus::Canvas;
        }
        Action::OpenDatabaseById(did) => {
            app.open_database(&did, ui, true);
            ui.focus = Focus::Canvas;
        }
        Action::SwitchWorkspace(name) => {
            let auth = app.auth.clone();
            let ui_tx = app.ui_tx.clone();
            let target_name = name.clone();
            ui.tasks.begin("workspace");
            tokio::spawn(async move {
                match auth.switch_workspace(&target_name).await {
                    Ok(_) => {
                        let ws_id = auth.active_workspace_id().await;
                        let _ = ui_tx.send(UiMsg::WorkspaceSwitched {
                            name: target_name,
                            workspace_id: ws_id,
                        });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "workspace".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::RefreshAll => {
            app.sync_ctrl.request(SyncCommand::RefreshAll);
            ui.info("Syncing Notion workspace...");
        }
        Action::OpenActiveInBrowser => {
            let url = match app.active_view {
                CanvasView::Page => app
                    .active_page_detail
                    .as_ref()
                    .map(|d| d.page.url.clone()),
                CanvasView::Database => app
                    .active_database()
                    .map(|d| d.url.clone()),
            };
            if let Some(u) = url.filter(|s| !s.trim().is_empty()) {
                return dispatch(Action::OpenUrl(u), app, ui);
            } else {
                ui.warning("No Notion URL available for the current item");
            }
        }
        Action::OpenUrl(url) => match open::that(&url) {
            Ok(()) => ui.info(format!("Opened {url}")),
            Err(err) => ui.error(format!("Failed to open browser: {err}")),
        },
        Action::CopyUrlToClipboard(url) => copy_to_clipboard(&url, "Copied URL to clipboard", ui),
        Action::CopySelectionOrMarkdown => {
            if let Some(detail) = app.active_page_detail.as_ref() {
                let visible = app.visible_blocks(ui);
                if let Some(vblk) = visible.get(ui.selected_block) {
                    if let NotionBlockKind::Code { ref rich_text, .. } = vblk.block.kind {
                        let code = crate::models::spans_plain_text(rich_text);
                        copy_to_clipboard(&code, "Copied code block to clipboard", ui);
                        return false;
                    }
                }
                let md = format!("# {}\n\n{}", detail.page.display_title(), detail.markdown);
                copy_to_clipboard(&md, "Copied page Markdown to clipboard", ui);
            }
        }
        Action::PasteClipboard => {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                if let Ok(text) = cb.get_text() {
                    if !text.trim().is_empty() {
                        if ui.focus == Focus::Comments {
                            ui.comment_reply_input.push_str(text.trim());
                        } else if app.active_page_id.is_some() {
                            return dispatch(Action::SubmitSlashAppend(text), app, ui);
                        }
                    }
                } else if let Ok(img) = cb.get_image() {
                    let ts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0);
                    match crate::ui::composer::save_clipboard_rgba_to_temp_png(
                        &std::env::temp_dir(),
                        ts,
                        img.width as u32,
                        img.height as u32,
                        img.bytes.into_owned(),
                    ) {
                        Ok((path, _)) => {
                            ui.success(format!("Saved clipboard image -> {}", path.display()));
                        }
                        Err(err) => ui.error(err),
                    }
                }
            }
        }
        Action::AttachFormFile(path) => {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let blocks = parse_slash_command_blocks(&content);
                if !blocks.is_empty() && app.active_page_id.is_some() {
                    return dispatch(Action::SubmitSlashAppend(content), app, ui);
                }
            }
            ui.info(format!("Selected file: {}", path.display()));
        }
        Action::LoadPreviewImage => {
            if let Some(Overlay::ImagePreview(ov)) = ui.overlays.last() {
                if let Some(target) = ov.current().cloned() {
                    let (cols, rows) = ov.target_dimensions(ui.term_width, ui.term_height);
                    let caps = ui.graphics_caps;
                    let cache_key = format!(
                        "hd:{}:{}:{}:{}:{:?}",
                        target.page_id, target.media_index, cols, rows, caps.protocol
                    );
                    if !app.image_cache.contains_key(&cache_key) {
                        let client = app.client.clone();
                        let http = client.http_client().clone();
                        let ui_tx = app.ui_tx.clone();
                        let url = target.url.clone();
                        let block_id = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| d.media.get(target.media_index))
                            .map(|m| m.block_id.clone())
                            .unwrap_or_default();
                        let page_id = target.page_id.clone();
                        tokio::spawn(async move {
                            // First attempt cached/direct URL; if expired (Notion S3 URLs expire after 1h),
                            // refresh the block from Notion API to get a fresh signed URL and retry once.
                            let fetched = match media::fetch_media_bytes_cached(&http, &url).await {
                                Ok(ok) => Ok(ok),
                                Err(first_err) if !block_id.trim().is_empty() => {
                                    if let Ok(blk) = client.get_block(&block_id, &page_id).await {
                                        if let NotionBlockKind::Image { url: fresh_url, .. } = blk.kind {
                                            if !fresh_url.trim().is_empty() && fresh_url != url {
                                                media::fetch_media_bytes_cached(&http, &fresh_url).await
                                            } else {
                                                Err(first_err)
                                            }
                                        } else {
                                            Err(first_err)
                                        }
                                    } else {
                                        Err(first_err)
                                    }
                                }
                                Err(err) => Err(err),
                            };

                            match fetched {
                                Ok((bytes, _)) => {
                                    let rendered = tokio::task::spawn_blocking(move || {
                                        crate::img_preview::decode_and_render_for_protocol(
                                            &bytes,
                                            cols,
                                            rows,
                                            (25, 25, 25),
                                            caps,
                                        )
                                    })
                                    .await;
                                    match rendered {
                                        Ok(Ok(img)) => {
                                            let _ = ui_tx.send(UiMsg::ImageRendered {
                                                cache_key,
                                                rendered: Box::new(img),
                                            });
                                        }
                                        Ok(Err(err)) => {
                                            let _ = ui_tx.send(UiMsg::ImagePreviewFailed(format!(
                                                "{err} (press 'o' to open in browser)"
                                            )));
                                        }
                                        Err(join_err) => {
                                            let _ = ui_tx.send(UiMsg::ImagePreviewFailed(format!(
                                                "Image decode task failed: {join_err}"
                                            )));
                                        }
                                    }
                                }
                                Err(err) => {
                                    let _ = ui_tx.send(UiMsg::ImagePreviewFailed(format!(
                                        "{err} (press 'o' to open in browser)"
                                    )));
                                }
                            }
                        });
                    }
                }
            }
        }
        Action::CycleImageProtocol => {
            let next = ui.graphics_caps.protocol.next_manual();
            ui.graphics_caps.protocol = next;
            app.config.image_protocol = match next {
                crate::img_preview::TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
                crate::img_preview::TerminalImageProtocol::ITerm2 => {
                    ImageProtocolPreference::Iterm2
                }
                crate::img_preview::TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
                crate::img_preview::TerminalImageProtocol::Halfblock => {
                    ImageProtocolPreference::Halfblock
                }
            };
            let _ = app.config.save();
            ui.active_pixel_graphic = None;
            ui.info(format!("Image protocol: {}", next.label()));
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return dispatch(Action::LoadPreviewImage, app, ui);
            }
        }
        Action::DownloadMedia(media_idx) => {
            if let Some(detail) = app.active_page_detail.as_ref() {
                if let Some(m) = detail.media.get(media_idx).cloned() {
                    let cfg = app.config.clone();
                    let client = app.client.clone();
                    let ui_tx = app.ui_tx.clone();
                    ui.tasks.begin("download");
                    tokio::spawn(async move {
                        match media::download_media_target(&cfg, &client, &m.url, None).await {
                            Ok(dl) => {
                                let _ = ui_tx.send(UiMsg::TaskDone {
                                    label: "download".into(),
                                    toast: format!("Saved {}", dl.path),
                                });
                            }
                            Err(err) => {
                                let _ = ui_tx.send(UiMsg::TaskFailed {
                                    label: "download".into(),
                                    error: err.to_string(),
                                });
                            }
                        }
                    });
                }
            }
        }
        Action::SetTheme(preset) => {
            app.config.theme = preset;
            ui.theme = crate::ui::theme::Theme::detect(preset);
            let _ = app.config.save();
            ui.info(format!("Theme: {}", preset.label()));
        }
        Action::ToggleInlineImages => {
            app.config.inline_images = !app.config.inline_images;
            ui.inline_images = app.config.inline_images;
            let _ = app.config.save();
        }
        Action::ToggleNotify => {
            app.config.notify = !app.config.notify;
            let _ = app.config.save();
        }
        Action::CycleSyncInterval => {
            app.config.sync_poll_ms = match app.config.sync_poll_ms {
                0..=30_000 => 60_000,
                30_001..=90_000 => 120_000,
                _ => 30_000,
            };
            let _ = app.config.save();
        }
        Action::MouseScroll { delta, column, row } => {
            if delta == 0 {
                return false;
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
                | Some(HitTarget::TodoToggle(_))
                | Some(HitTarget::ToggleBlock(_))
                | Some(HitTarget::ChildCard(_))
                | Some(HitTarget::MediaChip(_))
                | Some(HitTarget::DbViewTab(_))
                | Some(HitTarget::DbRow(_))
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
                        let mag = delta.unsigned_abs();
                        if delta < 0 {
                            ui.canvas_scroll = ui.canvas_scroll.saturating_sub(mag);
                        } else {
                            ui.canvas_scroll = ui.canvas_scroll.saturating_add(mag);
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
                            if let Some(HitTarget::DbBoardCard { col, .. }) = hit {
                                if col < cols.len() {
                                    ui.db_board_col = col;
                                }
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
        _ => {}
    }

    false
}

fn handle_mouse_click(
    target: HitTarget,
    _col: u16,
    _row: u16,
    app: &mut AppState,
    ui: &mut UiState,
) -> bool {
    if !ui.overlays.is_empty() {
        match target {
            HitTarget::OverlayBody => return false,
            HitTarget::OverlayItem { index } => {
                if let Some(top) = ui.overlays.last_mut() {
                    top.select(index);
                }
                return dispatch(Action::Activate, app, ui);
            }
            HitTarget::Button(ButtonId::ConfirmOk) => return dispatch(Action::Activate, app, ui),
            HitTarget::Button(ButtonId::ConfirmCancel) => {
                return dispatch(Action::CloseOverlay, app, ui);
            }
            _ => {
                ui.overlays.pop();
                ui.active_pixel_graphic = None;
                return false;
            }
        }
    }

    match target {
        HitTarget::Pane(focus) => {
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
        }
        HitTarget::TodoToggle(idx) | HitTarget::ToggleBlock(idx) => {
            ui.focus = Focus::Canvas;
            return dispatch(Action::ToggleBlockAt(idx), app, ui);
        }
        HitTarget::ChildCard(idx) => {
            ui.focus = Focus::Canvas;
            return dispatch(Action::ActivateChildBlockAt(idx), app, ui);
        }
        HitTarget::MediaChip(idx) => {
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            let visible = app.visible_blocks(ui);
            if let Some(vblk) = visible.get(idx) {
                match &vblk.block.kind {
                    NotionBlockKind::Image { url, .. } => {
                        let block_id = vblk.block.id.clone();
                        let url = url.clone();
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
                    NotionBlockKind::Video { url, .. }
                    | NotionBlockKind::Audio { url, .. }
                    | NotionBlockKind::Bookmark { url, .. }
                    | NotionBlockKind::Embed { url, .. }
                    | NotionBlockKind::LinkPreview { url } => {
                        let u = url.clone();
                        return dispatch(Action::OpenUrl(u), app, ui);
                    }
                    NotionBlockKind::File { url, .. } | NotionBlockKind::Pdf { url, .. } => {
                        let block_id = vblk.block.id.clone();
                        let u = url.clone();
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &block_id, &u));
                        if let Some(midx) = media_idx {
                            return dispatch(Action::DownloadMedia(midx), app, ui);
                        } else if !u.trim().is_empty() {
                            return dispatch(Action::OpenUrl(u), app, ui);
                        }
                        return false;
                    }
                    _ => {}
                }
            }
            return dispatch(Action::OpenImagePreview, app, ui);
        }
        HitTarget::DbViewTab(mode) => {
            return dispatch(Action::SetDbViewMode(mode), app, ui);
        }
        HitTarget::DbRow(idx) => {
            ui.focus = Focus::Canvas;
            ui.db_selected_row = idx;
            return dispatch(Action::Activate, app, ui);
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
            ButtonId::EditExternal => return dispatch(Action::RequestExternalEditor, app, ui),
            ButtonId::OpenWeb => return dispatch(Action::OpenActiveInBrowser, app, ui),
            ButtonId::LinkExtractor => return dispatch(Action::OpenLinkExtractor, app, ui),
            ButtonId::ToggleHrefs => return dispatch(Action::ToggleShowLinkUrls, app, ui),
            ButtonId::ImagePreview => return dispatch(Action::OpenImagePreview, app, ui),
            ButtonId::Attachments => {
                return dispatch(Action::OpenOverlay(OverlayKind::Attachments), app, ui);
            }
            ButtonId::DbFilter => return dispatch(Action::OpenDbFilter, app, ui),
            ButtonId::DbSort => return dispatch(Action::CycleDbSort, app, ui),
            ButtonId::NarrowSidebar => ui.narrow_pane = NarrowPane::Sidebar,
            ButtonId::NarrowCanvas => ui.narrow_pane = NarrowPane::Canvas,
            ButtonId::ConfirmOk | ButtonId::ConfirmCancel => {}
        },
        HitTarget::OverlayBody | HitTarget::OverlayItem { .. } => {}
    }

    false
}

fn selected_db_row_page(app: &AppState, ui: &UiState) -> Option<NotionPage> {
    match ui.db_view_mode {
        DatabaseViewMode::Board => {
            let cols = app.build_board_columns(ui);
            cols.get(ui.db_board_col)
                .and_then(|c| c.cards.get(ui.db_board_row))
                .cloned()
        }
        DatabaseViewMode::Table | DatabaseViewMode::List => {
            let rows = app.filtered_sorted_db_rows(ui);
            rows.get(ui.db_selected_row).cloned()
        }
    }
}

fn find_media_index_for_block(
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

fn step_sidebar_selection(
    items: &[crate::ui::app::SidebarItem],
    current: usize,
    delta: i16,
) -> usize {
    if items.is_empty() || delta == 0 {
        return current.min(items.len().saturating_sub(1));
    }
    let max_idx = items.len().saturating_sub(1);
    let mut idx = current.min(max_idx);
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
    idx
}

fn copy_to_clipboard(text: &str, ok_msg: &str, ui: &mut UiState) {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => ui.success(ok_msg),
        Err(err) => ui.error(format!("Clipboard error: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{backend::TestBackend, layout::Rect, Terminal};

    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        img_preview::TerminalImageProtocol,
        models::{
            NotionBlock, NotionComment, NotionColor, NotionDatabase, NotionMediaRef,
            NotionPageDetail, NotionPropertySchema, NotionPropertyValue, RichTextSpan, SelectOption,
        },
        notion::NotionClient,
        storage::Storage,
        sync::{FocusTarget, SyncController},
        ui,
    };

    fn make_test_app_and_ui() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_unit_test_token");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let mut ui_state = UiState::from_config(&cfg);
        ui_state.show_sidebar = true;
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);

        let root_page = NotionPage {
            id: "page-root".into(),
            title: "Root Page".into(),
            parent_type: "workspace".into(),
            ..Default::default()
        };
        let child_page = NotionPage {
            id: "page-child".into(),
            title: "Child Page".into(),
            parent_type: "page_id".into(),
            parent_id: Some("page-root".into()),
            ..Default::default()
        };
        let sibling_page = NotionPage {
            id: "page-sibling".into(),
            title: "Sibling Page".into(),
            parent_type: "workspace".into(),
            ..Default::default()
        };
        app.pages = vec![root_page.clone(), child_page, sibling_page];

        let mut schema = BTreeMap::new();
        schema.insert(
            "Status".into(),
            NotionPropertySchema {
                id: "st".into(),
                name: "Status".into(),
                prop_type: "status".into(),
                options: vec![
                    SelectOption {
                        id: Some("1".into()),
                        name: "Todo".into(),
                        color: NotionColor::Gray,
                    },
                    SelectOption {
                        id: Some("2".into()),
                        name: "Done".into(),
                        color: NotionColor::Green,
                    },
                ],
            },
        );
        app.databases = vec![NotionDatabase {
            id: "db-1".into(),
            title: "Tasks".into(),
            schema,
            ..Default::default()
        }];
        app.active_database_rows = (0..6)
            .map(|i| {
                let mut props = BTreeMap::new();
                let status = if i < 3 { "Todo" } else { "Done" };
                props.insert(
                    "Status".into(),
                    NotionPropertyValue::Status(Some(SelectOption {
                        id: None,
                        name: status.into(),
                        color: NotionColor::Blue,
                    })),
                );
                NotionPage {
                    id: format!("row-{i}"),
                    title: format!("Task {i}"),
                    parent_type: "database_id".into(),
                    parent_id: Some("db-1".into()),
                    properties: props,
                    ..Default::default()
                }
            })
            .collect();

        let mut blocks: Vec<NotionBlock> = (0..50)
            .map(|i| NotionBlock {
                id: format!("blk-p-{i}"),
                page_id: "page-root".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain(format!("Paragraph line number {i}"))],
                    color: NotionColor::Default,
                },
                ..Default::default()
            })
            .collect();
        blocks.insert(
            0,
            NotionBlock {
                id: "blk-img-1".into(),
                page_id: "page-root".into(),
                kind: NotionBlockKind::Image {
                    url: "https://example.com/1.png".into(),
                    caption: vec![RichTextSpan::plain("First image")],
                    is_hosted: false,
                },
                ..Default::default()
            },
        );
        blocks.insert(
            1,
            NotionBlock {
                id: "blk-img-2".into(),
                page_id: "page-root".into(),
                kind: NotionBlockKind::Image {
                    url: "https://example.com/2.png".into(),
                    caption: vec![RichTextSpan::plain("Second image")],
                    is_hosted: false,
                },
                ..Default::default()
            },
        );
        blocks.insert(
            2,
            NotionBlock {
                id: "blk-bookmark".into(),
                page_id: "page-root".into(),
                kind: NotionBlockKind::Bookmark {
                    url: "https://notion.so".into(),
                    caption: vec![RichTextSpan::plain("Notion Docs")],
                },
                ..Default::default()
            },
        );

        let comments: Vec<NotionComment> = (0..15)
            .map(|i| NotionComment {
                id: format!("c-{i}"),
                page_id: "page-root".into(),
                author: format!("User {i}"),
                body: format!("Comment body line {i}"),
                created_time: "2026-11-20T01:00:00Z".into(),
                ..Default::default()
            })
            .collect();

        let media = vec![
            NotionMediaRef {
                block_id: "blk-img-1".into(),
                url: "https://example.com/1.png".into(),
                display_name: "First image".into(),
                mime_type: "image/png".into(),
                is_image: true,
            },
            NotionMediaRef {
                block_id: "blk-img-2".into(),
                url: "https://example.com/2.png".into(),
                display_name: "Second image".into(),
                mime_type: "image/png".into(),
                is_image: true,
            },
        ];

        app.active_view = CanvasView::Page;
        app.active_page_id = Some("page-root".into());
        app.active_page_detail = Some(NotionPageDetail {
            page: root_page,
            blocks,
            comments,
            markdown: String::new(),
            media,
        });
        (app, ui_state)
    }

    #[test]
    fn test_mouse_scroll_routes_by_hovered_pane_sidebar_canvas_database_and_comments() {
        let (mut app, mut ui) = make_test_app_and_ui();
        ui.focus = Focus::Canvas;
        ui.comments_drawer_open = true;

        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();

        // 1. Scroll over Sidebar (col = 5)
        let before_sidebar = ui.sidebar_selected;
        dispatch(
            Action::MouseScroll {
                delta: 1,
                column: 5,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.sidebar_selected > before_sidebar);
        assert_eq!(ui.canvas_scroll, 0);
        assert_eq!(ui.comments_scroll, 0);

        // 2. Scroll over Page Canvas (col = 60)
        dispatch(
            Action::MouseScroll {
                delta: 4,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.canvas_scroll, 4);
        assert_eq!(ui.comments_scroll, 0);

        // 3. Scroll over Comments Drawer (col = 130)
        dispatch(
            Action::MouseScroll {
                delta: 3,
                column: 130,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.comments_scroll, 3);
        assert_eq!(ui.canvas_scroll, 4);

        // 4. Switch to Database View (Table, List, Board)
        app.active_view = CanvasView::Database;
        app.active_database_id = Some("db-1".into());
        ui.comments_drawer_open = false;

        for mode in [DatabaseViewMode::Table, DatabaseViewMode::List] {
            ui.db_view_mode = mode;
            ui.db_selected_row = 0;
            terminal
                .draw(|f| {
                    ui::draw(f, &mut app, &mut ui);
                })
                .unwrap();
            dispatch(
                Action::MouseScroll {
                    delta: 2,
                    column: 60,
                    row: 10,
                },
                &mut app,
                &mut ui,
            );
            assert_eq!(ui.db_selected_row, 2, "failed for {mode:?}");
            dispatch(
                Action::MouseScroll {
                    delta: -1,
                    column: 60,
                    row: 10,
                },
                &mut app,
                &mut ui,
            );
            assert_eq!(ui.db_selected_row, 1, "failed scroll up for {mode:?}");
        }

        // Board mode: hovering card in column 1 switches db_board_col and scrolls db_board_row
        ui.db_view_mode = DatabaseViewMode::Board;
        ui.db_board_col = 0;
        ui.db_board_row = 0;
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let card_rect = ui
            .hitmap
            .region_of(HitTarget::DbBoardCard { col: 1, row: 0 })
            .expect("board card col=1 row=0 should be in hitmap");
        dispatch(
            Action::MouseScroll {
                delta: 1,
                column: card_rect.x + 1,
                row: card_rect.y + 1,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.db_board_col, 1);
        assert_eq!(ui.db_board_row, 1);
    }

    #[test]
    fn test_canvas_scroll_clamps_after_scroll_bottom_so_wheel_up_responds_immediately() {
        let (mut app, mut ui) = make_test_app_and_ui();
        ui.focus = Focus::Canvas;
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();

        dispatch(Action::ScrollBottom, &mut app, &mut ui);
        assert_eq!(ui.canvas_scroll, u16::MAX / 2);

        // Drawing clamps ui.canvas_scroll back to exact max_scroll
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let clamped_max = ui.canvas_scroll;
        assert!(clamped_max > 0 && clamped_max < 200);

        // Immediately wheeling up moves canvas_scroll back by 3 without lag
        dispatch(
            Action::MouseScroll {
                delta: -3,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.canvas_scroll, clamped_max.saturating_sub(3));
    }

    #[test]
    fn test_clicking_outside_overlay_on_canvas_margin_closes_overlay_and_clears_pixel_graphic() {
        let (mut app, mut ui) = make_test_app_and_ui();
        ui.overlays.push(Overlay::help());
        ui.active_pixel_graphic = Some((
            "hd:page-root:0".into(),
            Rect::new(10, 5, 40, 20),
            TerminalImageProtocol::Sixel,
        ));

        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();

        // Top-left margin inside sidebar/canvas outside the centered Help overlay
        let target = ui
            .hitmap
            .hit(2, 2)
            .expect("outside click must hit a Pane backdrop target");
        assert!(!matches!(
            target,
            HitTarget::OverlayBody | HitTarget::OverlayItem { .. }
        ));

        dispatch(
            Action::MouseClick {
                target,
                col: 2,
                row: 2,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty());
        assert!(ui.active_pixel_graphic.is_none());
    }

    #[tokio::test]
    async fn test_clicking_nth_image_chip_opens_matching_media_index_and_sidebar_arrow_toggles_tree()
    {
        let (mut app, mut ui) = make_test_app_and_ui();
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();

        // 1. Verify Bookmark block registered as ChildCard(2)
        assert!(ui.hitmap.region_of(HitTarget::ChildCard(2)).is_some());

        // 2. Clicking 2nd MediaChip (block index 1) opens ImagePreviewOverlay at index == 1
        let chip2 = ui
            .hitmap
            .region_of(HitTarget::MediaChip(1))
            .expect("2nd image MediaChip should be mapped");
        let hit = ui.hitmap.hit(chip2.y, chip2.x).unwrap();
        assert_eq!(hit, HitTarget::MediaChip(1));
        dispatch(
            Action::MouseClick {
                target: hit,
                col: chip2.x,
                row: chip2.y,
            },
            &mut app,
            &mut ui,
        );
        match ui.overlays.last() {
            Some(Overlay::ImagePreview(ov)) => {
                assert_eq!(ov.index, 1);
                assert_eq!(ov.current().unwrap().url, "https://example.com/2.png");
            }
            other => panic!("expected ImagePreview overlay, got {other:?}"),
        }
        ui.overlays.clear();

        // 3. Clicking SidebarToggle arrow toggles collapsed_nodes while keeping Focus::Sidebar
        let items = app.build_sidebar_items(&ui);
        let root_idx = items
            .iter()
            .position(|it| it.id == "page-root")
            .expect("root page in sidebar");
        let toggle_rect = ui
            .hitmap
            .region_of(HitTarget::SidebarToggle(root_idx))
            .expect("collapsible root page arrow should be in hitmap");
        let arrow_hit = ui.hitmap.hit(toggle_rect.y, toggle_rect.x + 1).unwrap();
        assert_eq!(arrow_hit, HitTarget::SidebarToggle(root_idx));

        ui.focus = Focus::Canvas;
        dispatch(
            Action::MouseClick {
                target: arrow_hit,
                col: toggle_rect.x + 1,
                row: toggle_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus, Focus::Sidebar);
        assert!(ui.collapsed_nodes.contains("page-root"));

        // Clicking again expands it and stays in Focus::Sidebar
        dispatch(
            Action::MouseClick {
                target: arrow_hit,
                col: toggle_rect.x + 1,
                row: toggle_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.focus, Focus::Sidebar);
        assert!(!ui.collapsed_nodes.contains("page-root"));

        // 4. Page Header metadata badges (💬 N and 🖼 N media) are clickable
        let comment_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::ToggleComments))
            .expect("comment metadata badge should be clickable");
        let media_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::ImagePreview))
            .expect("media metadata badge should be clickable");
        assert!(media_btn.x > comment_btn.x);
    }
}
