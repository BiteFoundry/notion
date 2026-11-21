//! Action definitions & central Notion TUI action dispatcher (`src/ui/action.rs`).

use std::path::PathBuf;

use bitefoundry_html::{sniff_content_kind, ContentKind};
use serde_json::json;

use crate::{
    config::{ImageProtocolPreference, ThemePreset},
    models::{spans_plain_text, DatabaseViewMode, NotionBlockKind, NotionPage},
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
    OpenLinkExtractor,
    OpenImagePreview,
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
    FinishInlineEdit,
    CancelInlineEdit,
    SaveInlineEdit,
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
        return handle_mouse_click(target, col, row, app, ui);
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
            | Action::ToggleInlineImages
            | Action::ToggleNotify
            | Action::CycleSyncInterval
            | Action::SwitchWorkspace(_)
            | Action::AttachFormFile(_)
            | Action::SubmitSlashAppend(_)
            | Action::SubmitPageEdit(_)
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

    // Route typing, cursor navigation, paste, and undo/redo to the active in-place inline editor
    // when Canvas is focused and no modal overlay is active.
    if ui.overlays.is_empty() && ui.inline_editor.is_some() && ui.focus == Focus::Canvas {
        if handle_inline_editor_action(&action, app, ui) {
            return false;
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
                    ui.canvas_follow_selected = true;
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
                    ui.canvas_scroll = ui.canvas_scroll.saturating_add(1);
                    ui.canvas_follow_selected = true;
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
            ui.canvas_follow_selected = false;
            ui.canvas_scroll = ui.canvas_scroll.saturating_sub(12);
            ui.selected_block = ui.selected_block.saturating_sub(6);
        }
        Action::PageDown => {
            ui.canvas_follow_selected = false;
            ui.canvas_scroll = ui.canvas_scroll.saturating_add(12);
            let max_b = app.visible_blocks(ui).len().saturating_sub(1);
            ui.selected_block = (ui.selected_block + 6).min(max_b);
        }
        Action::ScrollTop => {
            ui.canvas_follow_selected = false;
            ui.canvas_scroll = 0;
            ui.selected_block = 0;
            ui.db_selected_row = 0;
        }
        Action::ScrollBottom => {
            ui.canvas_follow_selected = false;
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
                    NotionBlockKind::Image { url, .. } => {
                        if is_html_svg_or_xml_ref(url, None) {
                            let title = url
                                .rsplit('/')
                                .next()
                                .unwrap_or("HTML Preview")
                                .to_string();
                            let u = url.clone();
                            let bid = vblk.block.id.clone();
                            let pid = app.active_page_id.clone();
                            return dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            );
                        }
                        return dispatch(Action::OpenImagePreview, app, ui);
                    }
                    NotionBlockKind::Video { url, .. } | NotionBlockKind::Audio { url, .. } => {
                        let bid = vblk.block.id.clone();
                        let u = url.clone();
                        let pid = app.active_page_id.clone();
                        return dispatch(
                            Action::OpenMediaUrl {
                                url: u,
                                block_id: (!bid.is_empty()).then_some(bid),
                                page_id: pid,
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
                        let bid = vblk.block.id.clone();
                        let u = url.clone();
                        if is_html_svg_or_xml_ref(&u, name.as_deref()) {
                            let title = name
                                .clone()
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| "HTML File".to_string());
                            let pid = app.active_page_id.clone();
                            return dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            );
                        }
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &bid, &u));
                        if let Some(midx) = media_idx {
                            return dispatch(Action::DownloadMedia(midx), app, ui);
                        } else if !u.trim().is_empty() {
                            return dispatch(
                                Action::OpenMediaUrl {
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: app.active_page_id.clone(),
                                },
                                app,
                                ui,
                            );
                        }
                    }
                    NotionBlockKind::Pdf { url, .. } => {
                        let bid = vblk.block.id.clone();
                        let u = url.clone();
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &bid, &u));
                        if let Some(midx) = media_idx {
                            return dispatch(Action::DownloadMedia(midx), app, ui);
                        } else if !u.trim().is_empty() {
                            return dispatch(
                                Action::OpenMediaUrl {
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: app.active_page_id.clone(),
                                },
                                app,
                                ui,
                            );
                        }
                    }
                    NotionBlockKind::Button { rich_text } => {
                        let raw_label = spans_plain_text(rich_text);
                        let label = if raw_label.trim().is_empty() {
                            "Button".to_string()
                        } else {
                            raw_label.trim().to_string()
                        };
                        let page_url = app
                            .active_page_detail
                            .as_ref()
                            .map(|d| d.page.url.trim().to_string())
                            .filter(|u| !u.is_empty());
                        if let Some(url) = page_url {
                            ui.info(format!(
                                "⚡ '{label}': Notion REST API does not expose button triggers — open in Notion Web?"
                            ));
                            ui.overlays.push(Overlay::confirm(
                                format!(
                                    "⚡ Button '{label}' runs Notion automations.\nNotion's public REST API does not expose button triggers.\nOpen this page in Notion Web?"
                                ),
                                Some(Action::OpenUrl(url)),
                            ));
                        } else {
                            ui.info(format!(
                                "⚡ '{label}': Notion public REST API does not expose button automation triggers"
                            ));
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
            OverlayKind::Attachments => {
                let mut tray = super::overlays::attachments::AttachmentsState::default();
                if let Some(std::borrow::Cow::Owned(draft)) = app.detail_for_block_action(ui) {
                    tray.draft_media = Some(draft.media);
                }
                ui.overlays.push(Overlay::Attachments(tray));
            }
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
            if let Some(detail) = app.detail_for_block_action(ui) {
                ui.overlays
                    .push(Overlay::links(LinkExtractorOverlay::from_notion_page(&detail)));
            } else {
                ui.info("Open a Notion page to extract links");
            }
        }
        Action::OpenImagePreview => {
            if let Some(detail) = app.detail_for_block_action(ui) {
                let visible = AppState::visible_detail_blocks(&detail, ui);
                if let Some(vblk) = visible.get(ui.selected_block) {
                    match &vblk.block.kind {
                        NotionBlockKind::Code {
                            rich_text,
                            language,
                            ..
                        } if is_html_code_language(language) => {
                            let html = spans_plain_text(rich_text);
                            let title = format!("{} ({language})", detail.page.display_title());
                            drop(detail);
                            return dispatch(Action::OpenHtmlPreview { title, html }, app, ui);
                        }
                        NotionBlockKind::File { url, name, .. }
                            if is_html_svg_or_xml_ref(url, name.as_deref()) =>
                        {
                            let title = name
                                .clone()
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| "HTML File".to_string());
                            let u = url.clone();
                            let bid = vblk.block.id.clone();
                            let pid = Some(detail.page.id.clone());
                            drop(detail);
                            return dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            );
                        }
                        NotionBlockKind::Image { url, caption, .. }
                            if is_html_svg_or_xml_ref(url, None) =>
                        {
                            let cap = spans_plain_text(caption);
                            let title = if cap.trim().is_empty() {
                                "HTML Preview".to_string()
                            } else {
                                cap
                            };
                            let u = url.clone();
                            let bid = vblk.block.id.clone();
                            let pid = Some(detail.page.id.clone());
                            drop(detail);
                            return dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            );
                        }
                        _ => {}
                    }
                }
                let preferred_media_idx = visible.get(ui.selected_block).and_then(|vblk| {
                    if let NotionBlockKind::Image { url, .. } = &vblk.block.kind {
                        find_media_index_for_block(&detail, &vblk.block.id, url)
                    } else {
                        None
                    }
                });
                let overlay = ImagePreviewOverlay::from_notion_page(&detail, preferred_media_idx);
                drop(detail);
                if let Some(ov) = overlay
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
        Action::OpenHtmlPreview { title, html } => {
            if matches!(
                ui.overlays.last(),
                Some(Overlay::ImagePreview(_) | Overlay::HtmlPreview(_))
            ) {
                ui.overlays.pop();
                ui.active_pixel_graphic = None;
                ui.pending_pixel_graphic = None;
            }
            ui.overlays.push(Overlay::html_preview(title, html));
        }
        Action::FetchHtmlPreview {
            title,
            url,
            block_id,
            page_id,
        } => {
            if url.trim().is_empty() {
                ui.warning("HTML file URL is empty");
                return false;
            }
            let client = app.client.clone();
            let http = client.http_client().clone();
            let ui_tx = app.ui_tx.clone();
            let pid = page_id.unwrap_or_default();
            ui.tasks.begin("html");
            tokio::spawn(async move {
                let fetched = match media::fetch_media_bytes_cached(&http, &url).await {
                    Ok(ok) => Ok(ok),
                    Err(first_err) => {
                        if let Some(bid) = block_id.filter(|s| !s.trim().is_empty()) {
                            if let Ok(blk) = client.get_block(&bid, &pid).await {
                                if let Some(fresh) = media::extract_block_media_url(&blk.kind) {
                                    if fresh != url {
                                        let _ = ui_tx.send(UiMsg::MediaUrlRefreshed {
                                            page_id: pid.clone(),
                                            block_id: bid.clone(),
                                            url: fresh.clone(),
                                        });
                                        media::fetch_media_bytes_cached(&http, &fresh).await
                                    } else {
                                        Err(first_err)
                                    }
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
                };
                match fetched {
                    Ok((bytes, mime)) => {
                        let raw = String::from_utf8_lossy(&bytes).into_owned();
                        let html = if mime == "image/svg+xml"
                            || media::looks_like_svg_document(&bytes)
                        {
                            format!("<div style=\"padding:16px\">{raw}</div>")
                        } else {
                            raw
                        };
                        let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded { title, html });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "html".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::ServeActiveHtml => {
            if let Some(Overlay::HtmlPreview(ov)) = ui.overlays.last() {
                let title = ov.title.clone();
                let raw_html = ov.session.raw_html().to_string();
                let server = app.html_server.clone();
                let ui_tx = app.ui_tx.clone();
                ui.tasks.begin("html-serve");
                tokio::spawn(async move {
                    match server.serve_html(&title, &raw_html).await {
                        Ok(url) => {
                            let _ = ui_tx.send(UiMsg::HtmlServed { url });
                        }
                        Err(err) => {
                            let _ = ui_tx.send(UiMsg::TaskFailed {
                                label: "html-serve".into(),
                                error: err.to_string(),
                            });
                        }
                    }
                });
            }
        }
        Action::ShowToast(msg) => {
            ui.info(msg);
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
        Action::OpenPageEditor => {
            if ui.inline_editor.is_some() {
                app.close_inline_editor_and_flush(ui);
            } else if !app.open_inline_editor(ui) {
                ui.warning("Open a page to edit");
            }
        }
        Action::FinishInlineEdit => {
            app.close_inline_editor_and_flush(ui);
        }
        Action::CancelInlineEdit => {
            app.handle_inline_editor_escape(ui);
        }
        Action::SaveInlineEdit => {
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
                            crate::sync::sync_page_detail_force(&client, &storage, &page_id).await
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
            if let Some(ed) = ui.inline_editor.take() {
                // Preserve unsaved inline buffer in active_page_detail before launching external $EDITOR
                app.apply_optimistic_local(&ed.page_id, ed.composer.text());
                ui.canvas_scroll = ed.scroll_y;
                ui.cursor_pos = None;
            }
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
        Action::OpenUrl(url) => match open_url_in_browser(&url) {
            Ok(()) => ui.info(format!("Opened {url}")),
            Err(err) => ui.error(format!("Failed to open browser: {err}")),
        },
        Action::OpenMediaUrl {
            url,
            block_id,
            page_id,
        } => {
            let bid = block_id.filter(|s| !s.trim().is_empty());
            if media::is_expiring_notion_s3_url(&url) {
                if let Some(bid) = bid {
                    let client = app.client.clone();
                    let ui_tx = app.ui_tx.clone();
                    let pid = page_id
                        .or_else(|| app.active_page_id.clone())
                        .unwrap_or_default();
                    ui.tasks.begin("open-media");
                    tokio::spawn(async move {
                        let mut target_url = url.clone();
                        if let Ok(blk) = client.get_block(&bid, &pid).await {
                            if let Some(fresh_url) = media::extract_block_media_url(&blk.kind) {
                                let _ = ui_tx.send(UiMsg::MediaUrlRefreshed {
                                    page_id: pid.clone(),
                                    block_id: bid.clone(),
                                    url: fresh_url.clone(),
                                });
                                target_url = fresh_url;
                            }
                        }
                        match open_url_in_browser(&target_url) {
                            Ok(()) => {
                                let _ = ui_tx.send(UiMsg::TaskDone {
                                    label: "open-media".into(),
                                    toast: format!("Opened {target_url}"),
                                });
                            }
                            Err(err) => {
                                let _ = ui_tx.send(UiMsg::TaskFailed {
                                    label: "open-media".into(),
                                    error: format!("Failed to open browser: {err}"),
                                });
                            }
                        }
                    });
                    return false;
                }
            }
            return dispatch(Action::OpenUrl(url), app, ui);
        }
        Action::CopyUrlToClipboard(url) => copy_to_clipboard(&url, "Copied URL to clipboard", ui),
        Action::CopySelectionOrMarkdown => {
            if let Some(detail) = app.detail_for_block_action(ui) {
                let visible = AppState::visible_detail_blocks(&detail, ui);
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
                        let block_id = if !target.block_id.trim().is_empty() {
                            target.block_id.clone()
                        } else {
                            app.active_page_detail
                                .as_ref()
                                .and_then(|d| d.media.get(target.media_index))
                                .map(|m| m.block_id.clone())
                                .unwrap_or_default()
                        };
                        let page_id = target.page_id.clone();
                        tokio::spawn(async move {
                            // First attempt cached/direct URL; if expired (Notion S3 URLs expire after 1h),
                            // refresh the block from Notion API to get a fresh signed URL and retry once.
                            let fetched = match media::fetch_media_bytes_cached(&http, &url).await {
                                Ok(ok) => Ok(ok),
                                Err(first_err) if !block_id.trim().is_empty() => {
                                    if let Ok(blk) = client.get_block(&block_id, &page_id).await {
                                        if let Some(fresh_url) =
                                            media::extract_block_media_url(&blk.kind)
                                        {
                                            if fresh_url != url {
                                                let _ = ui_tx.send(UiMsg::MediaUrlRefreshed {
                                                    page_id: page_id.clone(),
                                                    block_id: block_id.clone(),
                                                    url: fresh_url.clone(),
                                                });
                                                media::fetch_media_bytes_cached(&http, &fresh_url)
                                                    .await
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

                            let display_name = target.display_name.clone();
                            match fetched {
                                Ok((bytes, mime)) => {
                                    let is_svg = mime == "image/svg+xml"
                                        || media::looks_like_svg_document(&bytes);
                                    let is_xml = mime == "application/xml"
                                        || mime == "text/xml"
                                        || media::looks_like_xml_bytes(&bytes);
                                    if is_svg {
                                        let svg_src = String::from_utf8_lossy(&bytes);
                                        let html = format!("<div style=\"padding:16px\">{svg_src}</div>");
                                        let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                                            title: display_name,
                                            html,
                                        });
                                    } else if is_xml {
                                        let xml_src = String::from_utf8_lossy(&bytes).into_owned();
                                        let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                                            title: display_name,
                                            html: xml_src,
                                        });
                                    } else {
                                        match sniff_content_kind(
                                            Some(&mime),
                                            Some(&display_name),
                                            Some(&bytes),
                                        ) {
                                            ContentKind::Html => {
                                                let html =
                                                    String::from_utf8_lossy(&bytes).into_owned();
                                                let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                                                    title: display_name,
                                                    html,
                                                });
                                            }
                                            ContentKind::Svg => {
                                                let svg_src = String::from_utf8_lossy(&bytes);
                                                let html = format!(
                                                    "<div style=\"padding:16px\">{svg_src}</div>"
                                                );
                                                let _ = ui_tx.send(UiMsg::HtmlPreviewLoaded {
                                                    title: display_name,
                                                    html,
                                                });
                                            }
                                            _ => {
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
                                                        let _ = ui_tx.send(
                                                            UiMsg::ImagePreviewFailed(format!(
                                                                "{err} (press 'o' to open in browser)"
                                                            )),
                                                        );
                                                    }
                                                    Err(join_err) => {
                                                        let _ = ui_tx.send(
                                                            UiMsg::ImagePreviewFailed(format!(
                                                                "Image decode task failed: {join_err}"
                                                            )),
                                                        );
                                                    }
                                                }
                                            }
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
            if let Some(detail) = app.detail_for_block_action(ui) {
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
                | Some(HitTarget::InlineEditorBody)
                | Some(HitTarget::TodoToggle(_))
                | Some(HitTarget::ToggleBlock(_))
                | Some(HitTarget::ChildCard(_))
                | Some(HitTarget::MediaChip(_))
                | Some(HitTarget::NotionButton(_))
                | Some(HitTarget::CodeHtmlPreview(_))
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

fn handle_inline_editor_action(action: &Action, app: &mut AppState, ui: &mut UiState) -> bool {
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
        Action::Newline | Action::Activate => {
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

fn handle_mouse_click(
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
                        ui.overlays.pop();
                        ui.active_pixel_graphic = None;
                        ui.pending_pixel_graphic = None;
                        return false;
                    }
                    OverlayOutcome::Dispatch(next) => return dispatch(next, app, ui),
                    OverlayOutcome::CloseAndDispatch(next) => {
                        ui.overlays.pop();
                        ui.active_pixel_graphic = None;
                        ui.pending_pixel_graphic = None;
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
                return dispatch(Action::Input('m'), app, ui);
            }
            HitTarget::Button(ButtonId::HtmlServeBrowser) => {
                return dispatch(Action::ServeActiveHtml, app, ui);
            }
            _ => {
                ui.overlays.pop();
                ui.active_pixel_graphic = None;
                ui.pending_pixel_graphic = None;
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
        HitTarget::InlineEditorBody => {
            ui.focus = Focus::Canvas;
            if let Some(ed) = ui.inline_editor.as_mut() {
                ed.place_cursor_at_screen_pos(col, row);
            }
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
                    NotionBlockKind::Image { url, caption, .. } => {
                        let block_id = vblk.block.id.clone();
                        let url = url.clone();
                        if is_html_svg_or_xml_ref(&url, None) {
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
                        if is_html_svg_or_xml_ref(&u, name.as_deref()) {
                            let title = name
                                .clone()
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| "HTML File".to_string());
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
            ui.focus = Focus::Canvas;
            ui.selected_block = idx;
            return dispatch(Action::ToggleActiveBlock, app, ui);
        }
        HitTarget::CodeHtmlPreview(idx) => {
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
            ButtonId::NarrowSidebar => ui.narrow_pane = NarrowPane::Sidebar,
            ButtonId::NarrowCanvas => ui.narrow_pane = NarrowPane::Canvas,
            ButtonId::ConfirmOk
            | ButtonId::ConfirmCancel
            | ButtonId::HtmlToggleMode
            | ButtonId::HtmlServeBrowser => {}
        },
        HitTarget::OverlayBody | HitTarget::OverlayItem { .. } => {}
    }

    false
}

pub(crate) fn is_html_code_language(language: &str) -> bool {
    let lower = language.trim().to_ascii_lowercase();
    matches!(lower.as_str(), "html" | "xml" | "xhtml")
}

pub(crate) fn is_html_file_ref(url: &str, name: Option<&str>) -> bool {
    name.is_some_and(|n| sniff_content_kind(None, Some(n), None) == ContentKind::Html)
        || sniff_content_kind(None, Some(url), None) == ContentKind::Html
}

fn is_html_svg_or_xml_ref(url: &str, name: Option<&str>) -> bool {
    if is_html_file_ref(url, name) {
        return true;
    }
    let check_ref = |s: &str| {
        let lower = s
            .split(['?', '#'])
            .next()
            .unwrap_or(s)
            .to_ascii_lowercase();
        lower.ends_with(".svg")
            || lower.ends_with(".xml")
            || sniff_content_kind(None, Some(s), None) == ContentKind::Svg
    };
    name.is_some_and(check_ref) || check_ref(url)
}

fn open_url_in_browser(url: &str) -> std::io::Result<()> {
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
        app.pages = [&root_page, &child_page, &sibling_page].into_iter().map(Into::into).collect();

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
            comments_error: None,
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

    #[tokio::test]
    async fn test_html_returned_from_image_auto_routes_to_html_preview() {
        use axum::{http::header, routing::get, Router};
        use tokio::net::TcpListener;

        // Mock HTTP endpoint pretending to be an image URL but returning HTML!
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let html_body = "<!DOCTYPE html><html><head><title>Auth Gate</title></head><body><h1>Sign in required</h1><button onclick=\"document.querySelector('h1').textContent='Clicked'\">Continue</button></body></html>";
        let router = Router::new().route(
            "/signed-photo",
            get(move || async move {
                (
                    axum::http::StatusCode::OK,
                    [(header::CONTENT_TYPE, "image/png")],
                    html_body,
                )
            }),
        );
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_route_test");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let mut ui = UiState::from_config(&cfg);
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
        let page = NotionPage {
            id: "page-html-img".into(),
            title: "HTML Image Test".into(),
            ..Default::default()
        };
        let url = format!("http://{addr}/signed-photo");
        app.active_view = CanvasView::Page;
        app.active_page_id = Some(page.id.clone());
        app.active_page_detail = Some(NotionPageDetail {
            page,
            blocks: vec![NotionBlock {
                id: "blk-img-html".into(),
                page_id: "page-html-img".into(),
                kind: NotionBlockKind::Image {
                    url: url.clone(),
                    caption: vec![RichTextSpan::plain("Broken Photo")],
                    is_hosted: false,
                },
                ..Default::default()
            }],
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: vec![NotionMediaRef {
                block_id: "blk-img-html".into(),
                url,
                display_name: "Broken Photo".into(),
                mime_type: "image/png".into(),
                is_image: true,
            }],
        });

        dispatch(Action::OpenImagePreview, &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))));

        // Wait for LoadPreviewImage background task to sniff HTML and send HtmlPreviewLoaded
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for HtmlPreviewLoaded")
            .expect("ui_rx closed");
        match &msg {
            UiMsg::HtmlPreviewLoaded { title, html } => {
                assert_eq!(title, "Broken Photo");
                assert!(html.contains("Sign in required"));
            }
            _ => panic!("expected UiMsg::HtmlPreviewLoaded, got another UiMsg"),
        }

        app.handle_ui_msg(&mut ui, msg);
        assert_eq!(ui.overlays.len(), 1);
        match ui.overlays.last() {
            Some(Overlay::HtmlPreview(ov)) => {
                assert!(ov.session.js_active);
                assert!(ov.session.raw_html().contains("Sign in required"));
            }
            other => panic!("expected Overlay::HtmlPreview replacing ImagePreview, got {other:?}"),
        }

        server_handle.abort();
    }

    #[test]
    fn test_html_file_block_mime_code_preview_pill_notion_button_and_all_button_pills() {
        use crate::parser::{collect_media_refs, parse_notion_block};

        // 1. Parse Notion `button` blocks (rich_text, plain text, and fallback "Button")
        let btn_rt = parse_notion_block(
            &serde_json::json!({
                "id": "blk-btn-1",
                "type": "button",
                "button": { "rich_text": [{ "plain_text": "Deploy Release" }] }
            }),
            "page-1",
            None,
        );
        assert_eq!(btn_rt.plain_text(), "Deploy Release");
        assert!(matches!(btn_rt.kind, NotionBlockKind::Button { .. }));

        let btn_fallback = parse_notion_block(
            &serde_json::json!({
                "id": "blk-btn-2",
                "type": "button",
                "button": {}
            }),
            "page-1",
            None,
        );
        assert_eq!(btn_fallback.plain_text(), "Button");

        // 2. Parse `.html` File block & `.html`/`.svg` Image blocks -> verify media refs never mark them as raster image!
        let html_file = parse_notion_block(
            &serde_json::json!({
                "id": "blk-file-html",
                "type": "file",
                "file": {
                    "type": "external",
                    "name": "dashboard.html",
                    "external": { "url": "https://example.com/dashboard.html" }
                }
            }),
            "page-1",
            None,
        );
        let fake_img_html = parse_notion_block(
            &serde_json::json!({
                "id": "blk-img-htm",
                "type": "image",
                "image": {
                    "type": "external",
                    "external": { "url": "https://example.com/viewer.htm" }
                }
            }),
            "page-1",
            None,
        );
        let fake_img_svg = parse_notion_block(
            &serde_json::json!({
                "id": "blk-img-svg",
                "type": "image",
                "image": {
                    "type": "external",
                    "external": { "url": "https://example.com/diagram.svg" }
                }
            }),
            "page-1",
            None,
        );
        let html_code = NotionBlock {
            id: "blk-code-html".into(),
            page_id: "page-1".into(),
            kind: NotionBlockKind::Code {
                rich_text: vec![RichTextSpan::plain(
                    "<h1>Embedded HTML</h1><button onclick=\"alert('hi')\">Go</button>",
                )],
                language: "html".into(),
                caption: Vec::new(),
            },
            ..Default::default()
        };

        let blocks = vec![btn_rt, html_file, fake_img_html, fake_img_svg, html_code];
        let media_refs = collect_media_refs(&blocks);
        assert_eq!(media_refs.len(), 3);
        assert_eq!(media_refs[0].mime_type, "text/html");
        assert!(!media_refs[0].is_image);
        assert_eq!(media_refs[1].mime_type, "text/html");
        assert!(!media_refs[1].is_image);
        assert_eq!(media_refs[2].mime_type, "image/svg+xml");
        assert!(!media_refs[2].is_image);

        // 3. Render page canvas with these blocks and verify HitMap targets + ButtonIds
        let (mut app, mut ui) = make_test_app_and_ui();
        let page = NotionPage {
            id: "page-1".into(),
            title: "Interactive Page".into(),
            url: "https://www.notion.so/page-1".into(),
            ..Default::default()
        };
        app.active_view = CanvasView::Page;
        app.active_page_id = Some(page.id.clone());
        app.active_page_detail = Some(NotionPageDetail {
            page,
            blocks,
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: media_refs,
        });
        ui.focus = Focus::Canvas;

        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("⚡") && screen.contains("Deploy Release"));
        assert!(screen.contains("Preview HTML"));
        assert!(screen.contains("dashboard.html"));

        // Verify previously missing ButtonIds are now wired in HitMap!
        for btn in [
            ButtonId::ToggleSidebar,
            ButtonId::Search,
            ButtonId::CommandPalette,
            ButtonId::ToggleFavorite,
            ButtonId::QuickAppend,
            ButtonId::NewChildPage,
            ButtonId::EditPage,
            ButtonId::OpenWeb,
            ButtonId::LinkExtractor,
            ButtonId::ToggleHrefs,
            ButtonId::Attachments,
        ] {
            assert!(
                ui.hitmap.region_of(HitTarget::Button(btn)).is_some(),
                "expected ButtonId::{btn:?} to be registered in HitMap"
            );
        }

        // 4. Clicking Notion `button` block shows toast & Confirm dialog offering to open Notion page URL
        let btn_rect = ui
            .hitmap
            .region_of(HitTarget::NotionButton(0))
            .expect("NotionButton(0) should be in hitmap");
        dispatch(
            Action::MouseClick {
                target: HitTarget::NotionButton(0),
                col: btn_rect.x + 2,
                row: btn_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(
            ui.toasts
                .latest()
                .is_some_and(|t| t.message.contains("Deploy Release")),
            "clicking Notion button should display an informative toast"
        );
        assert!(matches!(ui.overlays.last(), Some(Overlay::Confirm(_))));
        ui.overlays.clear();

        // 5. Clicking `[▶ Preview HTML]` pill on the HTML code block opens Overlay::HtmlPreview!
        let code_pill = ui
            .hitmap
            .region_of(HitTarget::CodeHtmlPreview(4))
            .expect("CodeHtmlPreview(4) pill should be in hitmap");
        dispatch(
            Action::MouseClick {
                target: HitTarget::CodeHtmlPreview(4),
                col: code_pill.x + 1,
                row: code_pill.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));
        ui.overlays.clear();

        // 6. Pressing 'v' in Attachments overlay on `.html` item dispatches FetchHtmlPreview,
        //    and on `.svg` item does NOT open ImagePreview!
        let mut att = crate::ui::overlays::attachments::AttachmentsState::default();
        match att.handle(&Action::Input('v'), &app) {
            OverlayOutcome::CloseAndDispatch(Action::FetchHtmlPreview { title, url, .. }) => {
                assert_eq!(title, "dashboard.html");
                assert_eq!(url, "https://example.com/dashboard.html");
            }
            other => panic!("expected FetchHtmlPreview for HTML attachment, got {other:?}"),
        }
        att.selected = 2; // diagram.svg
        match att.handle(&Action::Input('v'), &app) {
            OverlayOutcome::Dispatch(Action::ShowToast(msg)) => {
                assert!(msg.contains("diagram.svg"));
            }
            other => panic!("expected ShowToast (not ImagePreview) for SVG attachment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_open_media_url_refreshes_signed_s3_url_and_svg_preview_wraps_html() {
        use axum::{
            extract::Path as AxumPath,
            http::header,
            routing::get,
            Json, Router,
        };
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let fresh_s3_url = format!(
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk-s3/fresh.png?X-Amz-Expires=3600&X-Amz-Signature=fresh_sig_999"
        );
        let fresh_for_route = fresh_s3_url.clone();
        let svg_body = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"24\" height=\"24\"><rect width=\"24\" height=\"24\" fill=\"teal\"/></svg>";

        let router = Router::new()
            .route(
                "/v1/blocks/{id}",
                get(move |AxumPath(id): AxumPath<String>| {
                    let fresh = fresh_for_route.clone();
                    async move {
                        Json(serde_json::json!({
                            "id": id,
                            "type": "image",
                            "has_children": false,
                            "image": {
                                "type": "file",
                                "file": { "url": fresh },
                                "caption": [{ "plain_text": "Signed Shot" }]
                            }
                        }))
                    }
                }),
            )
            .route(
                "/diagram.svg",
                get(move || async move {
                    (
                        axum::http::StatusCode::OK,
                        [(header::CONTENT_TYPE, "image/svg+xml")],
                        svg_body,
                    )
                }),
            );
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_s3_refresh_test");
        let client = NotionClient::new(&cfg, auth.clone())
            .unwrap()
            .with_base_url(format!("{base_url}/v1"));
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let mut ui = UiState::from_config(&cfg);
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);

        let stale_s3_url = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk-s3/old.png?X-Amz-Expires=3600&X-Amz-Signature=expired_sig_000".to_string();
        let svg_url = format!("{base_url}/diagram.svg");
        let page = NotionPage {
            id: "page-s3".into(),
            title: "S3 Refresh Test".into(),
            ..Default::default()
        };
        app.active_view = CanvasView::Page;
        app.active_page_id = Some(page.id.clone());
        app.active_page_detail = Some(NotionPageDetail {
            page: page.clone(),
            blocks: vec![NotionBlock {
                id: "blk-s3".into(),
                page_id: page.id.clone(),
                kind: NotionBlockKind::Image {
                    url: stale_s3_url.clone(),
                    caption: vec![RichTextSpan::plain("Signed Shot")],
                    is_hosted: true,
                },
                ..Default::default()
            }],
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: vec![
                NotionMediaRef {
                    block_id: "blk-s3".into(),
                    url: stale_s3_url.clone(),
                    display_name: "Signed Shot".into(),
                    mime_type: "image/png".into(),
                    is_image: true,
                },
                NotionMediaRef {
                    block_id: "blk-svg".into(),
                    url: svg_url.clone(),
                    display_name: "diagram.svg".into(),
                    mime_type: "image/png".into(),
                    is_image: true,
                },
            ],
        });

        // Open ImagePreviewOverlay and press 'o' -> dispatches OpenMediaUrl and refreshes signed S3 URL!
        let ov = ImagePreviewOverlay::from_notion_page(app.active_page_detail.as_ref().unwrap(), Some(0)).unwrap();
        assert_eq!(ov.current().unwrap().block_id, "blk-s3");
        ui.overlays.push(Overlay::ImagePreview(ov));

        dispatch(Action::Input('o'), &mut app, &mut ui);

        let mut got_refreshed = false;
        for _ in 0..3 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timed out waiting for MediaUrlRefreshed")
                .expect("ui_rx closed");
            if let UiMsg::MediaUrlRefreshed {
                ref page_id,
                ref block_id,
                ref url,
            } = msg
            {
                assert_eq!(page_id, "page-s3");
                assert_eq!(block_id, "blk-s3");
                assert_eq!(url, &fresh_s3_url);
                got_refreshed = true;
            }
            app.handle_ui_msg(&mut ui, msg);
            if got_refreshed {
                break;
            }
        }
        assert!(got_refreshed, "expected UiMsg::MediaUrlRefreshed");
        assert_eq!(
            app.active_page_detail.as_ref().unwrap().media[0].url,
            fresh_s3_url
        );
        match ui.overlays.last() {
            Some(Overlay::ImagePreview(ov)) => {
                assert_eq!(ov.items[0].url, fresh_s3_url);
            }
            other => panic!("expected ImagePreviewOverlay with refreshed URL, got {other:?}"),
        }

        // Now verify LoadPreviewImage on genuine SVG wraps it cleanly in <div style="padding:16px">...</div>
        if let Some(Overlay::ImagePreview(ref mut ov)) = ui.overlays.last_mut() {
            ov.index = 1; // diagram.svg
        }
        dispatch(Action::LoadPreviewImage, &mut app, &mut ui);
        let mut got_svg_html = false;
        for _ in 0..4 {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
                .await
                .expect("timed out waiting for SVG HtmlPreviewLoaded")
                .expect("ui_rx closed");
            if let UiMsg::HtmlPreviewLoaded { ref title, ref html } = msg {
                assert_eq!(title, "diagram.svg");
                assert!(html.starts_with("<div style=\"padding:16px\">"));
                assert!(html.contains("<svg "));
                got_svg_html = true;
                app.handle_ui_msg(&mut ui, msg);
                break;
            }
            app.handle_ui_msg(&mut ui, msg);
        }
        assert!(got_svg_html, "expected SVG to open cleanly in HtmlPreviewOverlay");
        assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));

        server_handle.abort();
    }

    #[test]
    fn test_canvas_move_down_advances_scroll_and_keeps_selected_block_visible() {
        let (mut app, mut ui) = make_test_app_and_ui();
        ui.focus = Focus::Canvas;
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();

        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert_eq!(ui.selected_block, 0);
        assert_eq!(ui.canvas_scroll, 0);

        // MoveDown increments selected_block AND advances canvas_scroll
        dispatch(Action::MoveDown, &mut app, &mut ui);
        assert_eq!(ui.selected_block, 1);
        assert_eq!(ui.canvas_scroll, 1);
        assert!(ui.canvas_follow_selected);

        // Navigating 35 blocks down auto-scrolls canvas_scroll so selected block stays in viewport
        for _ in 0..35 {
            dispatch(Action::MoveDown, &mut app, &mut ui);
        }
        assert_eq!(ui.selected_block, 36);
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert!(ui.canvas_scroll >= 15, "expected canvas_scroll to follow selected block, got {}", ui.canvas_scroll);
        assert!(!ui.canvas_follow_selected);

        // MouseScroll freely scrolls without snapping back to selected_block
        let before_wheel = ui.canvas_scroll;
        dispatch(
            Action::MouseScroll {
                delta: -6,
                column: 60,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        assert_eq!(ui.canvas_scroll, before_wheel.saturating_sub(6));
    }

    #[tokio::test]
    async fn test_inline_editor_opens_in_canvas_without_popup_optimistic_update_and_esc_done_scroll_preserved() {
        use std::sync::{Arc, Mutex};
        use axum::{
            extract::Path as AxumPath,
            routing::{delete, get},
            Json, Router,
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use tokio::net::TcpListener;

        let deleted_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let patched_blocks: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let appended_payloads: Arc<Mutex<Vec<(Option<String>, Vec<serde_json::Value>)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let current_blocks: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(vec![
            serde_json::json!({
                "id": "blk-old-1",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "Old content" }] }
            }),
            serde_json::json!({
                "id": "blk-child-page",
                "type": "child_page",
                "has_children": false,
                "child_page": { "title": "Keep Child Page" }
            }),
        ]));

        let del_log = deleted_ids.clone();
        let patch_log = patched_blocks.clone();
        let blocks_del = current_blocks.clone();
        let blocks_get = current_blocks.clone();
        let blocks_append = current_blocks.clone();
        let blocks_update = current_blocks.clone();
        let app_log = appended_payloads.clone();

        let router = Router::new()
            .route(
                "/v1/pages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "object": "page",
                        "id": id,
                        "url": "https://www.notion.so/page-edit-1",
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false,
                        "properties": {
                            "title": { "type": "title", "title": [{ "plain_text": "Editable Page" }] }
                        }
                    }))
                }),
            )
            .route(
                "/v1/blocks/{id}/children",
                get(move || {
                    let b = blocks_get.clone();
                    async move {
                        Json(serde_json::json!({
                            "object": "list",
                            "results": b.lock().unwrap().clone(),
                            "has_more": false
                        }))
                    }
                })
                .patch(move |Json(body): Json<serde_json::Value>| {
                    let b = blocks_append.clone();
                    let log = app_log.clone();
                    async move {
                        let after = body
                            .get("after")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let children = body
                            .get("children")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        log.lock().unwrap().push((after, children.clone()));
                        let mut guard = b.lock().unwrap();
                        let mut created = Vec::new();
                        for (i, mut c) in children.into_iter().enumerate() {
                            c["id"] = serde_json::json!(format!("blk-new-{}-{}", guard.len(), i));
                            c["has_children"] = serde_json::json!(false);
                            guard.push(c.clone());
                            created.push(c);
                        }
                        Json(serde_json::json!({ "object": "list", "results": created }))
                    }
                }),
            )
            .route(
                "/v1/blocks/{id}",
                delete(move |AxumPath(id): AxumPath<String>| {
                    let d = del_log.clone();
                    let b = blocks_del.clone();
                    async move {
                        d.lock().unwrap().push(id.clone());
                        b.lock().unwrap().retain(|blk| blk["id"] != id);
                        Json(serde_json::json!({ "id": id, "archived": true }))
                    }
                })
                .patch(
                    move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                        let log = patch_log.clone();
                        let b = blocks_update.clone();
                        async move {
                            log.lock().unwrap().push((id.clone(), body.clone()));
                            let mut guard = b.lock().unwrap();
                            if let Some(existing) = guard.iter_mut().find(|blk| blk["id"] == id) {
                                if let Some(btype) = existing["type"].as_str().map(str::to_string) {
                                    if let Some(updated_inner) = body.get(&btype) {
                                        existing[&btype] = updated_inner.clone();
                                    }
                                }
                                return Json(existing.clone());
                            }
                            Json(serde_json::json!({
                                "id": id,
                                "type": "paragraph",
                                "has_children": false,
                                "paragraph": body.get("paragraph").cloned().unwrap_or_else(|| serde_json::json!({ "rich_text": [] }))
                            }))
                        }
                    },
                ),
            )
            .route(
                "/v1/comments",
                get(|| async {
                    Json(serde_json::json!({ "object": "list", "results": [], "has_more": false }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_edit_test");
        let client = NotionClient::new(&cfg, auth.clone())
            .unwrap()
            .with_base_url(format!("http://{addr}/v1"));
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let mut ui = UiState::from_config(&cfg);
        ui.focus = Focus::Canvas;
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
        let page = NotionPage {
            id: "11111111-2222-3333-4444-555555555555".into(),
            workspace_id: app.workspace_id.clone(),
            title: "Editable Page".into(),
            ..Default::default()
        };
        let other_page = NotionPage {
            id: "22222222-2222-3333-4444-555555555555".into(),
            workspace_id: app.workspace_id.clone(),
            title: "Second Page".into(),
            ..Default::default()
        };
        app.storage.upsert_page(&page).unwrap();
        app.storage.upsert_page(&other_page).unwrap();
        app.pages = [&page, &other_page].into_iter().map(Into::into).collect();
        app.active_view = CanvasView::Page;
        app.active_page_id = Some(page.id.clone());
        app.active_page_detail = Some(NotionPageDetail {
            page: page.clone(),
            blocks: vec![
                NotionBlock {
                    id: "blk-old-1".into(),
                    page_id: page.id.clone(),
                    block_type: "paragraph".into(),
                    kind: NotionBlockKind::Paragraph {
                        rich_text: vec![RichTextSpan::plain("Old content")],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-child-page".into(),
                    page_id: page.id.clone(),
                    block_type: "child_page".into(),
                    kind: NotionBlockKind::ChildPage {
                        title: "Keep Child Page".into(),
                    },
                    ..Default::default()
                },
            ],
            comments: Vec::new(),
            comments_error: None,
            markdown: "Old content".into(),
            media: Vec::new(),
        });

        // 1. Pressing 'e' opens INLINE editor directly inside Canvas (ZERO popup overlays!)
        let act_e = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_e, Action::OpenPageEditor);
        dispatch(act_e, &mut app, &mut ui);
        assert!(!ui.editor_requested, "'e' must NEVER launch external editor");
        assert!(ui.inline_editor.is_some(), "'e' must open inline Canvas editor");
        assert!(ui.overlays.is_empty(), "inline editor must NOT open a popup overlay");

        // Pressing Esc when clean exits inline editor cleanly
        let act_esc = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_esc, Action::CancelInlineEdit);
        dispatch(act_esc, &mut app, &mut ui);
        assert!(ui.inline_editor.is_none());

        // 2. Clicking [✎ Edit] pill opens INLINE editor in Canvas (ZERO popup overlays!)
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let edit_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::EditPage))
            .expect("[✎ Edit] button should map to ButtonId::EditPage");
        dispatch(
            Action::MouseClick {
                target: HitTarget::Button(ButtonId::EditPage),
                col: edit_btn.x + 1,
                row: edit_btn.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(!ui.editor_requested, "[✎ Edit] must NEVER launch external editor");
        assert!(ui.inline_editor.is_some());
        assert!(ui.overlays.is_empty());

        // Render inline editor and verify header bar pills, cursor position, and hitmap targets
        terminal
            .draw(|f| {
                ui::draw(f, &mut app, &mut ui);
            })
            .unwrap();
        let edit_screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(edit_screen.contains("EDITING"));
        assert!(edit_screen.contains("[✓ Done]"));
        assert!(edit_screen.contains("[Ctrl+S Save]"));
        assert!(edit_screen.contains("[Ext $EDITOR]"));
        assert!(edit_screen.contains("Ln 1, Col 1"));
        assert!(ui.cursor_pos.is_some(), "inline editor must expose live terminal cursor");
        assert!(ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::FinishInlineEdit))
            .is_some());
        assert!(ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::SaveInlineEdit))
            .is_some());
        assert!(ui.hitmap.region_of(HitTarget::InlineEditorBody).is_some());

        // 3. Typing keys (including 'q', 'j', 'k', 'e', Korean UTF-8, Enter, Tab, Undo) routes to inline editor
        //    AND immediately updates optimistic local page detail in 0ms while preserving ChildPage!
        let editor_body = ui
            .hitmap
            .region_of(HitTarget::InlineEditorBody)
            .expect("inline editor body hit target");
        // Click at col 4 on line 1 ("Old |content") to verify mouse click-to-place cursor
        dispatch(
            Action::MouseClick {
                target: HitTarget::InlineEditorBody,
                col: editor_body.x + 4,
                row: editor_body.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.inline_editor.as_ref().unwrap().cursor_line_col(), (1, 5));

        dispatch(Action::CursorEnd, &mut app, &mut ui);
        let act_enter = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_enter, Action::Newline);
        dispatch(act_enter, &mut app, &mut ui);

        for ch in "qje 새 문단X".chars() {
            let act_ch = crate::ui::event::key_to_action(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                ui.focus(),
                &ui.keymap,
                ui.accepts_text_input(),
                false,
                ui.db_view_mode,
            )
            .unwrap();
            assert_eq!(act_ch, Action::Input(ch));
            dispatch(act_ch, &mut app, &mut ui);
        }

        // Ctrl+Z undoes 'X'
        let act_undo = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_undo, Action::Undo);
        dispatch(act_undo, &mut app, &mut ui);

        // Rendering the draft leaves the active block model intact until save or preview.
        let ed_gen = ui.inline_editor.as_ref().unwrap().edit_generation;
        assert!(ed_gen > 0);
        let active_detail = app.active_page_detail.as_ref().unwrap();
        assert_eq!(active_detail.markdown, "Old content");
        assert!(!active_detail.page.plain_text.contains("qje 새 문단"));
        let opt_detail = app.detail_for_block_action(&ui).unwrap();
        assert_eq!(opt_detail.markdown, "Old content\nqje 새 문단");
        assert!(opt_detail.page.plain_text.contains("qje 새 문단"));
        assert!(
            opt_detail
                .blocks
                .iter()
                .any(|b| matches!(b.kind, NotionBlockKind::ChildPage { ref title } if title == "Keep Child Page")),
            "optimistic update must preserve ChildPage blocks"
        );
        drop(opt_detail);

        // 4. Ctrl+S flushes immediately while STAYING in inline edit mode!
        let act_save = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_save, Action::SaveInlineEdit);
        dispatch(act_save, &mut app, &mut ui);
        assert!(
            ui.inline_editor.is_some(),
            "Ctrl+S must stay in inline edit mode while flushing sync"
        );
        assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);

        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for PageEdited")
            .expect("ui_rx closed");
        let expected_gen = ui.inline_editor.as_ref().unwrap().edit_generation;
        assert!(
            matches!(msg, UiMsg::PageEdited { generation, .. } if generation == expected_gen),
            "expected UiMsg::PageEdited with matching generation"
        );
        app.handle_ui_msg(&mut ui, msg);
        assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
        assert_eq!(
            ui.inline_editor.as_ref().unwrap().sync_status().0,
            "✓ Saved"
        );
        // Unchanged first paragraph was NOT deleted! Only the new 2nd paragraph was appended with `after: blk-old-1`!
        assert!(deleted_ids.lock().unwrap().is_empty());
        assert_eq!(appended_payloads.lock().unwrap().len(), 1);
        assert_eq!(
            appended_payloads.lock().unwrap()[0].0.as_deref(),
            Some("blk-old-1")
        );

        // 5. Clicking [✓ Done] returns to rendered Canvas preserving scroll position
        ui.inline_editor.as_mut().unwrap().scroll_y = 7;
        let done_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::FinishInlineEdit))
            .expect("[✓ Done] pill should be in hitmap");
        dispatch(
            Action::MouseClick {
                target: HitTarget::Button(ButtonId::FinishInlineEdit),
                col: done_btn.x + 1,
                row: done_btn.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.inline_editor.is_none());
        assert_eq!(ui.canvas_scroll, 7);

        // 6. Ctrl+E still requests external $EDITOR
        let act_ext = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            ui.focus(),
            &ui.keymap,
            ui.accepts_text_input(),
            false,
            ui.db_view_mode,
        )
        .unwrap();
        assert_eq!(act_ext, Action::RequestExternalEditor);
        dispatch(act_ext, &mut app, &mut ui);
        assert!(ui.editor_requested);
        ui.editor_requested = false;

        // 7. Navigating to another page while inline editing flushes and closes cleanly
        app.open_inline_editor(&mut ui);
        assert!(ui.inline_editor.is_some());
        dispatch(Action::Input('!'), &mut app, &mut ui);
        app.open_page(&other_page.id, &mut ui, true);
        assert!(ui.inline_editor.is_none());
        assert_eq!(app.active_page_id.as_deref(), Some(other_page.id.as_str()));

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_manual_save_only_sync_confirm_discard_and_undo_save_revert() {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        use axum::{
            extract::Path as AxumPath,
            routing::get,
            Json, Router,
        };
        use tokio::net::TcpListener;

        let patch_texts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let patch_log = patch_texts.clone();
        let confirmed_text = patch_texts.clone();

        let router = Router::new()
            .route(
                "/v1/pages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "object": "page", "id": id,
                        "parent": { "type": "workspace", "workspace": true },
                        "properties": { "title": { "type": "title", "title": [{ "plain_text": "Manual Save Page" }] } }
                    }))
                }),
            )
            .route(
                "/v1/blocks/{id}",
                axum::routing::patch(
                    move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                        let log = patch_log.clone();
                        async move {
                            let content = body
                                .pointer("/paragraph/rich_text")
                                .and_then(|value| value.as_array())
                                .into_iter()
                                .flatten()
                                .filter_map(|span| span.pointer("/text/content").and_then(|value| value.as_str()))
                                .collect::<String>();
                            log.lock().unwrap().push(content.clone());
                            Json(serde_json::json!({
                                "id": id,
                                "type": "paragraph",
                                "has_children": false,
                                "paragraph": { "rich_text": [{ "plain_text": content }] }
                            }))
                        }
                    },
                ),
            )
            .route(
                "/v1/blocks/{id}/children",
                get(move || {
                    let confirmed_text = confirmed_text.clone();
                    async move {
                        let content = confirmed_text.lock().unwrap().last().cloned()
                            .unwrap_or_else(|| "Hello".to_owned());
                        Json(serde_json::json!({
                            "object": "list", "has_more": false, "results": [{
                                "id": "blk-p-1", "type": "paragraph", "has_children": false,
                                "paragraph": { "rich_text": [{ "plain_text": content }] }
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1/comments",
                get(|| async {
                    Json(serde_json::json!({ "object": "list", "results": [], "has_more": false }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_manual_save_test");
        let client = NotionClient::new(&cfg, auth.clone())
            .unwrap()
            .with_base_url(format!("http://{addr}/v1"));
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let mut ui = UiState::from_config(&cfg);
        ui.focus = Focus::Canvas;
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
        let page = NotionPage {
            id: "33333333-2222-3333-4444-555555555555".into(),
            workspace_id: app.workspace_id.clone(),
            title: "Manual Save Page".into(),
            ..Default::default()
        };
        app.storage.upsert_page(&page).unwrap();
        app.pages = vec![(&page).into()];
        app.active_view = CanvasView::Page;
        app.active_page_id = Some(page.id.clone());
        app.active_page_detail = Some(NotionPageDetail {
            page: page.clone(),
            blocks: vec![NotionBlock {
                id: "blk-p-1".into(),
                page_id: page.id.clone(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Hello")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            }],
            comments: Vec::new(),
            comments_error: None,
            markdown: "Hello".into(),
            media: Vec::new(),
        });

        // 1. Typing makes 0 API calls (manual save-only) and shows "● Unsaved"
        assert!(app.open_inline_editor(&mut ui));
        assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "✓ Saved");
        dispatch(Action::CursorEnd, &mut app, &mut ui);
        dispatch(Action::Paste(" World".into()), &mut app, &mut ui);
        assert!(ui.inline_editor.as_ref().unwrap().is_dirty());
        assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
        assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "● Unsaved");
        assert!(patch_texts.lock().unwrap().is_empty(), "typing must NEVER auto-sync");

        // 2. Undo before saving reverts is_dirty() -> false and Ctrl+S makes 0 API calls
        dispatch(Action::Undo, &mut app, &mut ui);
        assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello");
        assert!(!ui.inline_editor.as_ref().unwrap().is_dirty());
        dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
        assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
        assert!(patch_texts.lock().unwrap().is_empty(), "saving unchanged buffer must make 0 API calls");
        assert_eq!(
            ui.toasts.latest().map(|t| t.message.as_str()),
            Some("No changes to save")
        );

        // 3. First Esc with unsaved changes arms confirm_discard WITHOUT syncing; second Esc discards & restores active_page_detail
        dispatch(Action::Paste(" Temp".into()), &mut app, &mut ui);
        assert_eq!(app.active_page_detail.as_ref().unwrap().markdown, "Hello");
        dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
        assert!(ui.inline_editor.is_some(), "1st Esc with unsaved edits must keep editor open");
        assert!(ui.inline_editor.as_ref().unwrap().confirm_discard);
        assert!(!ui.inline_editor.as_ref().unwrap().sync_in_flight);
        assert!(patch_texts.lock().unwrap().is_empty(), "1st Esc must NEVER fire Notion API sync");

        // Typing resets confirm_discard
        dispatch(Action::Input('!'), &mut app, &mut ui);
        assert!(!ui.inline_editor.as_ref().unwrap().confirm_discard);

        // First Esc again -> arm confirm_discard, Second Esc -> discard and restore last_synced_detail!
        dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
        assert!(ui.inline_editor.as_ref().unwrap().confirm_discard);
        dispatch(Action::CancelInlineEdit, &mut app, &mut ui);
        assert!(ui.inline_editor.is_none(), "2nd Esc must exit inline edit mode");
        assert!(patch_texts.lock().unwrap().is_empty(), "2nd Esc discard must make 0 API calls");
        assert_eq!(
            app.active_page_detail.as_ref().unwrap().markdown,
            "Hello",
            "discard must restore active_page_detail back to last_synced_detail"
        );

        // 4. Undo + Save revert: Edit "Hello" -> "Hello World" -> Ctrl+S -> Ctrl+Z -> Ctrl+S
        assert!(app.open_inline_editor(&mut ui));
        dispatch(Action::CursorEnd, &mut app, &mut ui);
        dispatch(Action::Paste(" World".into()), &mut app, &mut ui);
        assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello World");

        // Save "Hello World"
        dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
        assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);
        assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "● Saving...");
        let msg1 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for save 1")
            .expect("ui_rx closed");
        app.handle_ui_msg(&mut ui, msg1);
        assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "✓ Saved");
        assert_eq!(ui.inline_editor.as_ref().unwrap().last_synced_markdown, "Hello World");
        assert_eq!(app.active_page_detail.as_ref().unwrap().markdown, "Hello World");

        // Ctrl+Z (Undo back to "Hello") -> must be dirty!
        dispatch(Action::Undo, &mut app, &mut ui);
        assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello");
        assert!(
            ui.inline_editor.as_ref().unwrap().is_dirty(),
            "undoing after save must mark buffer dirty vs last_synced_markdown"
        );
        assert_eq!(ui.inline_editor.as_ref().unwrap().sync_status().0, "● Unsaved");

        // Ctrl+S (Save reverted "Hello") -> MUST PATCH Notion back to "Hello"!
        dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
        assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);
        let msg2 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for revert save")
            .expect("ui_rx closed");
        app.handle_ui_msg(&mut ui, msg2);

        let ed = ui.inline_editor.as_ref().unwrap();
        assert!(!ed.sync_in_flight);
        assert!(!ed.is_dirty());
        assert_eq!(ed.sync_status().0, "✓ Saved");
        assert_eq!(ed.last_synced_markdown, "Hello");
        assert_eq!(app.active_page_detail.as_ref().unwrap().markdown, "Hello");
        assert_eq!(
            *patch_texts.lock().unwrap(),
            vec!["Hello World".to_string(), "Hello".to_string()],
            "Notion API must receive the revert PATCH back to 'Hello'"
        );

        // 5. Typing while an explicit save is in flight must NOT overwrite newer keystrokes or auto-sync them
        dispatch(Action::Paste(" A".into()), &mut app, &mut ui);
        dispatch(Action::SaveInlineEdit, &mut app, &mut ui);
        assert!(ui.inline_editor.as_ref().unwrap().sync_in_flight);
        dispatch(Action::Input('B'), &mut app, &mut ui);
        assert_eq!(ui.inline_editor.as_ref().unwrap().composer.text(), "Hello AB");

        let msg3 = tokio::time::timeout(Duration::from_secs(5), ui_rx.recv())
            .await
            .expect("timed out waiting for in-flight save")
            .expect("ui_rx closed");
        app.handle_ui_msg(&mut ui, msg3);
        let ed = ui.inline_editor.as_ref().unwrap();
        assert_eq!(ed.composer.text(), "Hello AB");
        assert_eq!(ed.last_synced_markdown, "Hello A");
        assert!(!ed.sync_in_flight, "typing after Ctrl+S must NOT auto-sync");
        assert!(ed.is_dirty());
        assert_eq!(ed.sync_status().0, "● Unsaved");

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_diff_sync_single_paragraph_edit_calls_one_patch_and_zero_deletes() {
        use std::sync::{Arc, Mutex};
        use axum::{
            extract::Path as AxumPath,
            routing::{delete, get},
            Json, Router,
        };
        use tokio::net::TcpListener;

        let deleted_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let patched_ids: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let appended_calls: Arc<Mutex<Vec<(Option<String>, usize)>>> =
            Arc::new(Mutex::new(Vec::new()));

        let initial_blocks = vec![
            serde_json::json!({
                "id": "blk-1",
                "type": "heading_1",
                "has_children": false,
                "heading_1": { "rich_text": [{ "plain_text": "Title Section" }] }
            }),
            serde_json::json!({
                "id": "blk-2",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "First untouched paragraph" }] }
            }),
            serde_json::json!({
                "id": "blk-3",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "Middle line to edit" }] }
            }),
            serde_json::json!({
                "id": "blk-4",
                "type": "to_do",
                "has_children": false,
                "to_do": { "checked": false, "rich_text": [{ "plain_text": "Untouched todo" }] }
            }),
            serde_json::json!({
                "id": "blk-5",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "Last untouched paragraph" }] }
            }),
            serde_json::json!({
                "id": "blk-child",
                "type": "child_page",
                "has_children": false,
                "child_page": { "title": "Preserved Child" }
            }),
        ];

        let state_blocks = Arc::new(Mutex::new(initial_blocks));
        let get_blocks = state_blocks.clone();
        let del_blocks = state_blocks.clone();
        let patch_blocks = state_blocks.clone();
        let append_blocks = state_blocks.clone();
        let del_log = deleted_ids.clone();
        let patch_log = patched_ids.clone();
        let app_log = appended_calls.clone();

        let router = Router::new()
            .route(
                "/v1/pages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "object": "page",
                        "id": id,
                        "url": "https://www.notion.so/page-diff",
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false,
                        "properties": {
                            "title": { "type": "title", "title": [{ "plain_text": "Diff Page" }] }
                        }
                    }))
                }),
            )
            .route(
                "/v1/blocks/{id}/children",
                get(move || {
                    let b = get_blocks.clone();
                    async move {
                        Json(serde_json::json!({
                            "object": "list",
                            "results": b.lock().unwrap().clone(),
                            "has_more": false
                        }))
                    }
                })
                .patch(move |Json(body): Json<serde_json::Value>| {
                    let b = append_blocks.clone();
                    let log = app_log.clone();
                    async move {
                        let after = body
                            .get("after")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let children = body
                            .get("children")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        log.lock().unwrap().push((after.clone(), children.len()));
                        let mut guard = b.lock().unwrap();
                        let mut created = Vec::new();
                        let insert_pos = after
                            .as_deref()
                            .and_then(|aid| guard.iter().position(|x| x["id"] == aid))
                            .map(|p| p + 1)
                            .unwrap_or_else(|| {
                                if body.pointer("/position/type").and_then(|v| v.as_str()) == Some("start") {
                                    0
                                } else {
                                    guard.len()
                                }
                            });
                        for (i, mut c) in children.into_iter().enumerate() {
                            c["id"] = serde_json::json!(format!("blk-ins-{i}"));
                            c["has_children"] = serde_json::json!(false);
                            guard.insert(insert_pos + i, c.clone());
                            created.push(c);
                        }
                        Json(serde_json::json!({ "object": "list", "results": created }))
                    }
                }),
            )
            .route(
                "/v1/blocks/{id}",
                delete(move |AxumPath(id): AxumPath<String>| {
                    let d = del_log.clone();
                    let b = del_blocks.clone();
                    async move {
                        d.lock().unwrap().push(id.clone());
                        b.lock().unwrap().retain(|blk| blk["id"] != id);
                        Json(serde_json::json!({ "id": id, "archived": true }))
                    }
                })
                .patch(
                    move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                        let p = patch_log.clone();
                        let b = patch_blocks.clone();
                        async move {
                            p.lock().unwrap().push((id.clone(), body.clone()));
                            let mut guard = b.lock().unwrap();
                            if let Some(existing) = guard.iter_mut().find(|blk| blk["id"] == id) {
                                let btype = existing["type"].as_str().unwrap().to_string();
                                if let Some(inner) = body.get(&btype) {
                                    existing[&btype] = inner.clone();
                                }
                                return Json(existing.clone());
                            }
                            Json(serde_json::json!({ "id": id, "type": "paragraph", "paragraph": { "rich_text": [] } }))
                        }
                    },
                ),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_diff_test");
        let client = NotionClient::new(&cfg, auth)
            .unwrap()
            .with_base_url(format!("http://{addr}/v1"));
        let storage = Storage::open_in_memory().unwrap();
        let page_id = "44444444-2222-3333-4444-555555555555";

        // 1. Change ONLY paragraph 3 (`blk-3`) on a 5-block + child_page page
        let edited_md = "# Title Section\n\nFirst untouched paragraph\n\nMiddle line UPDATED!\n\n- [ ] Untouched todo\n\nLast untouched paragraph";
        let detail = crate::cli::op_replace_page_markdown(&client, &storage, page_id, edited_md)
            .await
            .unwrap();

        assert_eq!(
            deleted_ids.lock().unwrap().len(),
            0,
            "editing 1 paragraph in place must make 0 DELETE calls"
        );
        assert_eq!(
            appended_calls.lock().unwrap().len(),
            0,
            "editing 1 paragraph in place must make 0 append calls"
        );
        {
            let patches = patched_ids.lock().unwrap();
            assert_eq!(
                patches.len(),
                1,
                "editing 1 paragraph on a 5-block page must make exactly 1 PATCH call"
            );
            assert_eq!(patches[0].0, "blk-3");
            assert_eq!(
                patches[0].1.pointer("/paragraph/rich_text/0/text/content"),
                Some(&serde_json::json!("Middle line UPDATED!"))
            );
        }
        assert!(detail.markdown.contains("Middle line UPDATED!"));
        assert!(detail
            .blocks
            .iter()
            .any(|b| matches!(b.kind, NotionBlockKind::ChildPage { ref title } if title == "Preserved Child")));

        // 2. Pure middle insertion after `blk-3` -> 0 DELETEs, 1 PATCH /children with `"after": "blk-3"`
        patched_ids.lock().unwrap().clear();
        let inserted_md = "# Title Section\n\nFirst untouched paragraph\n\nMiddle line UPDATED!\n\nBrand new inserted line\n\n- [ ] Untouched todo\n\nLast untouched paragraph";
        let detail2 =
            crate::cli::op_replace_page_markdown(&client, &storage, page_id, inserted_md)
                .await
                .unwrap();
        assert_eq!(deleted_ids.lock().unwrap().len(), 0);
        assert_eq!(patched_ids.lock().unwrap().len(), 0);
        {
            let appends = appended_calls.lock().unwrap();
            assert_eq!(appends.len(), 1);
            assert_eq!(appends[0].0.as_deref(), Some("blk-3"));
            assert_eq!(appends[0].1, 1);
        }
        assert!(detail2.markdown.contains("Brand new inserted line"));

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_diff_sync_preserves_hosted_image_and_recovers_from_stale_known_id() {
        use std::sync::{Arc, Mutex};
        use axum::{
            extract::Path as AxumPath,
            http::StatusCode,
            response::IntoResponse,
            routing::{delete, get},
            Json, Router,
        };
        use tokio::net::TcpListener;

        let deleted_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let patched_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let appended_payloads: Arc<Mutex<Vec<(Option<String>, Vec<serde_json::Value>)>>> =
            Arc::new(Mutex::new(Vec::new()));

        let hosted_url = "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/blk/demo.png?X-Amz=sig";
        let state_blocks = Arc::new(Mutex::new(vec![
            serde_json::json!({
                "id": "blk-top",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "Top" }] }
            }),
            serde_json::json!({
                "id": "blk-hosted-img",
                "type": "image",
                "has_children": false,
                "image": {
                    "type": "file",
                    "file": { "url": hosted_url },
                    "caption": []
                }
            }),
            serde_json::json!({
                "id": "blk-bottom",
                "type": "paragraph",
                "has_children": false,
                "paragraph": { "rich_text": [{ "plain_text": "Bottom" }] }
            }),
        ]));

        let get_blocks = state_blocks.clone();
        let del_blocks = state_blocks.clone();
        let patch_blocks = state_blocks.clone();
        let append_blocks = state_blocks.clone();
        let del_log = deleted_ids.clone();
        let patch_log = patched_ids.clone();
        let app_log = appended_payloads.clone();

        let router = Router::new()
            .route(
                "/v1/pages/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "object": "page",
                        "id": id,
                        "url": "https://www.notion.so/page-img",
                        "parent": { "type": "workspace", "workspace": true },
                        "archived": false,
                        "properties": {
                            "title": { "type": "title", "title": [{ "plain_text": "Hosted Image Page" }] }
                        }
                    }))
                }),
            )
            .route(
                "/v1/blocks/{id}/children",
                get(move || {
                    let b = get_blocks.clone();
                    async move {
                        Json(serde_json::json!({
                            "object": "list",
                            "results": b.lock().unwrap().clone(),
                            "has_more": false
                        }))
                    }
                })
                .patch(move |Json(body): Json<serde_json::Value>| {
                    let b = append_blocks.clone();
                    let log = app_log.clone();
                    async move {
                        let after = body
                            .get("after")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let children = body
                            .get("children")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        log.lock().unwrap().push((after.clone(), children.clone()));
                        let mut guard = b.lock().unwrap();
                        let mut created = Vec::new();
                        let insert_pos = after
                            .as_deref()
                            .and_then(|aid| guard.iter().position(|x| x["id"] == aid))
                            .map(|p| p + 1)
                            .unwrap_or_else(|| {
                                if body.pointer("/position/type").and_then(|v| v.as_str()) == Some("start") {
                                    0
                                } else {
                                    guard.len()
                                }
                            });
                        for (i, mut c) in children.into_iter().enumerate() {
                            c["id"] = serde_json::json!(format!("blk-ins-{}-{}", guard.len(), i));
                            c["has_children"] = serde_json::json!(false);
                            guard.insert(insert_pos + i, c.clone());
                            created.push(c);
                        }
                        Json(serde_json::json!({ "object": "list", "results": created }))
                    }
                }),
            )
            .route(
                "/v1/blocks/{id}",
                delete(move |AxumPath(id): AxumPath<String>| {
                    let d = del_log.clone();
                    let b = del_blocks.clone();
                    async move {
                        d.lock().unwrap().push(id.clone());
                        b.lock().unwrap().retain(|blk| blk["id"] != id);
                        Json(serde_json::json!({ "id": id, "archived": true }))
                    }
                })
                .patch(
                    move |AxumPath(id): AxumPath<String>, Json(body): Json<serde_json::Value>| {
                        let p = patch_log.clone();
                        let b = patch_blocks.clone();
                        async move {
                            p.lock().unwrap().push(id.clone());
                            let mut guard = b.lock().unwrap();
                            if let Some(existing) = guard.iter_mut().find(|blk| blk["id"] == id) {
                                let btype = existing["type"].as_str().unwrap().to_string();
                                if let Some(inner) = body.get(&btype) {
                                    existing[&btype] = inner.clone();
                                }
                                return (StatusCode::OK, Json(existing.clone())).into_response();
                            }
                            (
                                StatusCode::NOT_FOUND,
                                Json(serde_json::json!({
                                    "code": "object_not_found",
                                    "message": "Could not find block with ID"
                                })),
                            )
                                .into_response()
                        }
                    },
                ),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_img_test");
        let client = NotionClient::new(&cfg, auth)
            .unwrap()
            .with_base_url(format!("http://{addr}/v1"));
        let storage = Storage::open_in_memory().unwrap();
        let page_id = "55555555-2222-3333-4444-555555555555";

        // 1. Edit "Top" -> "Top Updated": 0 deletes on hosted image, 1 PATCH on blk-top!
        let edited_md = format!("Top Updated\n\n![]({hosted_url})\n\nBottom");
        let detail = crate::cli::op_replace_page_markdown(&client, &storage, page_id, &edited_md)
            .await
            .unwrap();
        assert!(
            !deleted_ids.lock().unwrap().iter().any(|id| id == "blk-hosted-img"),
            "hosted image must NEVER be deleted"
        );
        assert!(deleted_ids.lock().unwrap().is_empty());
        assert_eq!(*patched_ids.lock().unwrap(), vec!["blk-top".to_string()]);
        assert!(appended_payloads.lock().unwrap().is_empty());
        assert!(detail.blocks.iter().any(|b| b.id == "blk-hosted-img"));
        assert_eq!(detail.media.len(), 1);

        // 2. Insert a new paragraph ABOVE the hosted image: 0 deletes on hosted image, exact order!
        patched_ids.lock().unwrap().clear();
        let inserted_above_md =
            format!("Intro\n\nTop Updated\n\n![]({hosted_url})\n\nBottom");
        let detail2 =
            crate::cli::op_replace_page_markdown(&client, &storage, page_id, &inserted_above_md)
                .await
                .unwrap();
        assert!(
            !deleted_ids.lock().unwrap().iter().any(|id| id == "blk-hosted-img"),
            "inserting above hosted image must make 0 DELETE calls on the hosted image"
        );
        assert!(
            appended_payloads
                .lock()
                .unwrap()
                .iter()
                .flat_map(|(_, c)| c.iter())
                .all(|c| c["type"] != "image"),
            "hosted image must NEVER be re-appended as an external image payload"
        );
        let types: Vec<&str> = detail2.blocks.iter().map(|b| b.block_type.as_str()).collect();
        assert_eq!(types, vec!["paragraph", "paragraph", "image", "paragraph"]);
        assert_eq!(detail2.blocks[0].plain_text(), "Intro");
        assert_eq!(detail2.blocks[1].plain_text(), "Top Updated");
        assert_eq!(detail2.blocks[2].id, "blk-hosted-img");
        assert_eq!(detail2.blocks[3].plain_text(), "Bottom");

        // 3. Optimistic local preview preserves hosted image & media when editing text around it
        let mut opt_detail = detail2.clone();
        crate::parser::apply_optimistic_markdown_to_detail(
            &mut opt_detail,
            &format!("Intro Edited\n\nTop Updated\n\n![]({hosted_url})\n\nBottom"),
        );
        assert!(opt_detail.blocks.iter().any(|b| b.id == "blk-hosted-img"));
        assert_eq!(opt_detail.media.len(), 1);

        // 4. Stale known_existing ID -> 404 triggers clean fallback refetch + retry!
        let stale_known = vec![
            NotionBlock {
                id: "blk-stale-gone".into(),
                page_id: page_id.into(),
                block_type: "paragraph".into(),
                kind: NotionBlockKind::Paragraph {
                    rich_text: vec![RichTextSpan::plain("Stale")],
                    color: NotionColor::Default,
                },
                ..Default::default()
            },
            detail2.blocks[2].clone(),
            detail2.blocks[3].clone(),
        ];
        let retry_md = format!("Intro Final\n\nTop Updated\n\n![]({hosted_url})\n\nBottom");
        let detail3 = crate::cli::op_diff_sync_page_markdown(
            &client,
            &storage,
            page_id,
            &retry_md,
            Some(stale_known),
        )
        .await
        .unwrap();
        assert_eq!(detail3.blocks[0].plain_text(), "Intro Final");
        assert!(detail3.blocks.iter().any(|b| b.id == "blk-hosted-img"));

        // 5. Empty image URL must NEVER emit an `image` payload
        assert!(
            crate::parser::markdown_to_notion_blocks("![]()\n![caption]()")
                .iter()
                .all(|b| b["type"] != "image")
        );
        let empty_img_blk = NotionBlock {
            id: "blk-empty-img".into(),
            page_id: page_id.into(),
            block_type: "image".into(),
            kind: NotionBlockKind::Image {
                url: String::new(),
                caption: Vec::new(),
                is_hosted: true,
            },
            ..Default::default()
        };
        assert!(crate::parser::is_preserved_anchor_block(&empty_img_blk));
        assert!(!crate::parser::blocks_to_markdown(&[empty_img_blk]).contains("![]("));

        server_handle.abort();
    }

    #[tokio::test]
    async fn test_sidebar_classification_block_id_nesting_db_exclusion_and_natural_sort() {
        use axum::{
            extract::Path as AxumPath,
            routing::{get, post},
            Json, Router,
        };
        use tokio::net::TcpListener;

        let router = Router::new()
            .route(
                "/v1/search",
                post(|| async {
                    Json(serde_json::json!({
                        "object": "list",
                        "results": [
                            {
                                "object": "page",
                                "id": "page-parent-a",
                                "url": "https://notion.so/page-parent-a",
                                "parent": { "type": "workspace", "workspace": true },
                                "archived": false,
                                "last_edited_time": "2026-11-20T05:00:00Z",
                                "properties": {
                                    "title": { "type": "title", "title": [{ "plain_text": "Idea Pet" }] }
                                }
                            },
                            {
                                "object": "page",
                                "id": "page-sub-via-api",
                                "url": "https://notion.so/page-sub-via-api",
                                "parent": { "type": "block_id", "block_id": "blk-nested-col" },
                                "archived": false,
                                "last_edited_time": "2026-11-20T06:00:00Z",
                                "properties": {
                                    "title": { "type": "title", "title": [{ "plain_text": "03. API Nested Subpage" }] }
                                }
                            }
                        ],
                        "has_more": false
                    }))
                }),
            )
            .route(
                "/v1/blocks/{id}",
                get(|AxumPath(id): AxumPath<String>| async move {
                    let parent = match id.as_str() {
                        "blk-nested-col" => serde_json::json!({ "type": "block_id", "block_id": "blk-heading-2" }),
                        "blk-heading-2" => serde_json::json!({ "type": "page_id", "page_id": "page-parent-a" }),
                        _ => serde_json::json!({ "type": "workspace", "workspace": true }),
                    };
                    Json(serde_json::json!({
                        "object": "block",
                        "id": id,
                        "type": "heading_2",
                        "parent": parent
                    }))
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_sidebar_test");
        let client = NotionClient::new(&cfg, auth.clone())
            .unwrap()
            .with_base_url(format!("http://{addr}/v1"));
        let storage = Storage::open_in_memory().unwrap();

        // 1. Online sync resolves multi-hop block_id -> block_id -> page_id into SQLite block_owners!
        let (ws_id, _, _) = crate::sync::sync_workspace_tree(&client, &storage)
            .await
            .unwrap();
        let (owners, _) = storage.load_page_parent_hints(&ws_id).unwrap();
        assert_eq!(
            owners.get("blk-nested-col").map(String::as_str),
            Some("page-parent-a")
        );
        assert_eq!(
            owners.get("blk-heading-2").map(String::as_str),
            Some("page-parent-a")
        );

        // 2. Add local blocks + numbered sub-pages (out of order by last_edited_time),
        //    plus a database_id row (with NO matching database in app.databases!) and an Untitled root.
        storage
            .replace_page_blocks(
                &ws_id,
                "page-parent-a",
                &[NotionBlock {
                    id: "blk-local-toggle".into(),
                    page_id: "page-parent-a".into(),
                    block_type: "toggle".into(),
                    kind: NotionBlockKind::Toggle {
                        rich_text: vec![RichTextSpan::plain("Section")],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                }],
            )
            .unwrap();

        let mk_page = |id: &str, title: &str, ptype: &str, pid: Option<&str>, edited: &str| NotionPage {
            id: id.into(),
            workspace_id: ws_id.clone(),
            parent_type: ptype.into(),
            parent_id: pid.map(str::to_string),
            title: title.into(),
            last_edited_time: Some(edited.into()),
            ..Default::default()
        };

        storage
            .upsert_pages(&[
                mk_page("page-untitled", "Untitled", "workspace", None, "2026-11-20T09:00:00Z"),
                mk_page("page-alpha", "Alpha Root", "workspace", None, "2026-11-20T01:00:00Z"),
                mk_page("page-db-row", "Orphan DB Row", "database_id", Some("db-unloaded"), "2026-11-20T09:30:00Z"),
                mk_page("sub-05", "05. Fifth", "page_id", Some("page-parent-a"), "2026-11-20T08:00:00Z"),
                mk_page("sub-02", "02. Second", "block_id", Some("blk-local-toggle"), "2026-11-20T07:00:00Z"),
                mk_page("sub-01", "01. First", "block_id", Some("blk-local-toggle"), "2026-11-20T02:00:00Z"),
                mk_page("sub-04", "04. Fourth", "page_id", Some("page-parent-a"), "2026-11-20T06:00:00Z"),
            ])
            .unwrap();

        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let ui = UiState::from_config(&cfg);
        let mut app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
        app.workspace_id = ws_id.clone();
        app.load_cached_initial();
        assert!(app.databases.is_empty(), "databases intentionally empty for exclusion test");

        let items = app.build_sidebar_items(&ui);
        let ws_items: Vec<&crate::ui::app::SidebarItem> =
            items.iter().filter(|i| !i.is_section_header).collect();

        // database_id row MUST NOT appear in top-level WORKSPACE roots!
        assert!(
            !ws_items.iter().any(|i| i.id == "page-db-row"),
            "database_id rows must NEVER be dumped into WORKSPACE roots"
        );

        // Top-level roots: Alpha Root, Idea Pet, Untitled (Untitled pushed to bottom!)
        let root_ids: Vec<&str> = ws_items
            .iter()
            .filter(|i| i.depth == 0)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(root_ids, vec!["page-alpha", "page-parent-a", "page-untitled"]);

        // All 5 sub-pages (2 from local toggle block_id, 1 from API block_owners, 2 from page_id)
        // must nest at depth 1 under Idea Pet in exact natural order: 01 -> 02 -> 03 -> 04 -> 05!
        let parent_item = ws_items.iter().find(|i| i.id == "page-parent-a").unwrap();
        assert!(parent_item.collapsible);
        let child_ids: Vec<&str> = ws_items
            .iter()
            .filter(|i| i.depth == 1)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(
            child_ids,
            vec!["sub-01", "sub-02", "page-sub-via-api", "sub-04", "sub-05"]
        );

        // When child_page block positions exist in `blocks`, exact Notion page order wins!
        app.child_page_positions.insert("sub-05".into(), 0);
        app.child_page_positions.insert("sub-01".into(), 1);
        let reordered = app.build_sidebar_items(&ui);
        let reordered_kids: Vec<&str> = reordered
            .iter()
            .filter(|i| !i.is_section_header && i.depth == 1)
            .map(|i| i.id.as_str())
            .collect();
        assert_eq!(reordered_kids[0], "sub-05");
        assert_eq!(reordered_kids[1], "sub-01");

        server_handle.abort();
    }
}
