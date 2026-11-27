//! Navigation, focus, comments, sidebar, workspace, and settings action handlers.

use crate::{
    models::{spans_plain_text, DatabaseViewMode, NotionBlockKind},
    sync::SyncCommand,
    ui::{
        app::{
            is_block_enter_editable, AppState, BlockCursorInit, CanvasView, Focus, NarrowPane,
            UiMsg, UiState,
        },
        overlays::Overlay,
    },
};

use super::{
    can_show_comments_drawer, dispatch, find_media_index_for_block,
    overlays::{is_html_or_xml_ref, is_markdown_file_ref, is_previewable_markup_ref},
    selected_db_row_page, step_sidebar_selection, Action,
};

pub(super) fn try_dispatch(
    action: Action,
    app: &mut AppState,
    ui: &mut UiState,
) -> Result<bool, Action> {
    match action {
        Action::Noop => {}
        Action::CloseOverlay => {
            if ui.comments_drawer_open {
                ui.comments_drawer_open = false;
                ui.focus = Focus::Canvas;
            }
        }
        Action::NextFocus => {
            let comments_visible = ui.comments_drawer_open && can_show_comments_drawer(app, ui);
            ui.focus = match ui.focus {
                Focus::Sidebar => Focus::Canvas,
                Focus::Canvas => {
                    if comments_visible {
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
            let comments_visible = ui.comments_drawer_open && can_show_comments_drawer(app, ui);
            ui.focus = match ui.focus {
                Focus::Sidebar => {
                    if comments_visible {
                        Focus::Comments
                    } else {
                        Focus::Canvas
                    }
                }
                Focus::Canvas => {
                    if ui.show_sidebar {
                        Focus::Sidebar
                    } else if comments_visible {
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
            if f != Focus::Comments || (ui.comments_drawer_open && can_show_comments_drawer(app, ui))
            {
                ui.focus = f;
            }
        }
        Action::SidebarCollapseOrPrevFocus => {
            if ui.focus == Focus::Sidebar {
                let items = app.build_sidebar_items(ui);
                if let Some(item) = items.get(ui.sidebar_selected) {
                    if item.collapsible && !ui.collapsed_nodes.contains(&item.id) {
                        ui.collapsed_nodes.insert(item.id.clone());
                        app.snap_sidebar_selection(ui);
                        return Ok(false);
                    }
                }
            } else {
                ui.focus = Focus::Sidebar;
                app.snap_sidebar_selection(ui);
            }
        }
        Action::SidebarExpandOrNextFocus => {
            if ui.focus == Focus::Sidebar {
                let items = app.build_sidebar_items(ui);
                if let Some(item) = items.get(ui.sidebar_selected) {
                    if item.collapsible && ui.collapsed_nodes.contains(&item.id) {
                        ui.collapsed_nodes.remove(&item.id);
                        app.snap_sidebar_selection(ui);
                        return Ok(false);
                    }
                }
                ui.focus = Focus::Canvas;
            } else if ui.comments_drawer_open && can_show_comments_drawer(app, ui) {
                ui.focus = Focus::Comments;
            }
        }
        Action::MoveUp => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                ui.sidebar_selected = step_sidebar_selection(&items, ui.sidebar_selected, -1);
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
                        app.clamp_db_selection(ui);
                        ui.db_board_row = ui.db_board_row.saturating_sub(1);
                    }
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        app.clamp_db_selection(ui);
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
                let items = app.build_sidebar_items(ui);
                ui.sidebar_selected = step_sidebar_selection(&items, ui.sidebar_selected, 1);
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
                        if !cols.is_empty() {
                            ui.db_board_col = ui.db_board_col.min(cols.len() - 1);
                            let cards_len = cols[ui.db_board_col].cards.len();
                            if ui.db_board_row + 1 < cards_len {
                                ui.db_board_row += 1;
                            } else {
                                ui.db_board_row = cards_len.saturating_sub(1);
                            }
                        }
                    }
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        let len = app.filtered_sorted_db_rows(ui).len();
                        if ui.db_selected_row + 1 < len {
                            ui.db_selected_row += 1;
                        } else {
                            ui.db_selected_row = len.saturating_sub(1);
                        }
                    }
                },
            },
            Focus::Comments => {
                ui.comments_scroll = ui.comments_scroll.saturating_add(1);
            }
            Focus::Overlay => {}
        },
        Action::PageUp => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                ui.sidebar_selected = step_sidebar_selection(&items, ui.sidebar_selected, -10);
            }
            Focus::Comments => {
                ui.comments_scroll = ui.comments_scroll.saturating_sub(10);
            }
            Focus::Canvas | Focus::Overlay => match app.active_view {
                CanvasView::Page => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = ui.canvas_scroll.saturating_sub(12);
                    ui.selected_block = ui.selected_block.saturating_sub(6);
                }
                CanvasView::Database => match ui.db_view_mode {
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        app.clamp_db_selection(ui);
                        ui.db_selected_row = ui.db_selected_row.saturating_sub(10);
                    }
                    DatabaseViewMode::Board => {
                        app.clamp_db_selection(ui);
                        ui.db_board_row = ui.db_board_row.saturating_sub(5);
                    }
                },
            },
        },
        Action::PageDown => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                ui.sidebar_selected = step_sidebar_selection(&items, ui.sidebar_selected, 10);
            }
            Focus::Comments => {
                ui.comments_scroll = ui.comments_scroll.saturating_add(10);
            }
            Focus::Canvas | Focus::Overlay => match app.active_view {
                CanvasView::Page => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = ui.canvas_scroll.saturating_add(12);
                    let max_b = app.visible_blocks(ui).len().saturating_sub(1);
                    ui.selected_block = (ui.selected_block + 6).min(max_b);
                }
                CanvasView::Database => match ui.db_view_mode {
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        let max_r = app.filtered_sorted_db_rows(ui).len().saturating_sub(1);
                        ui.db_selected_row = ui.db_selected_row.saturating_add(10).min(max_r);
                    }
                    DatabaseViewMode::Board => {
                        let cols = app.build_board_columns(ui);
                        if !cols.is_empty() {
                            ui.db_board_col = ui.db_board_col.min(cols.len() - 1);
                            let max_r = cols[ui.db_board_col].cards.len().saturating_sub(1);
                            ui.db_board_row = ui.db_board_row.saturating_add(5).min(max_r);
                        }
                    }
                },
            },
        },
        Action::ScrollTop => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                ui.sidebar_selected = step_sidebar_selection(&items, 0, 0);
            }
            Focus::Comments => {
                ui.comments_scroll = 0;
            }
            Focus::Canvas | Focus::Overlay => match app.active_view {
                CanvasView::Page => {
                    ui.canvas_follow_selected = false;
                    ui.canvas_scroll = 0;
                    ui.selected_block = 0;
                }
                CanvasView::Database => match ui.db_view_mode {
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        ui.db_selected_row = 0;
                    }
                    DatabaseViewMode::Board => {
                        ui.db_board_row = 0;
                    }
                },
            },
        },
        Action::ScrollBottom => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                ui.sidebar_selected = items
                    .iter()
                    .rposition(|it| !it.is_section_header)
                    .unwrap_or(0);
            }
            Focus::Comments => {
                ui.comments_scroll = u16::MAX / 2;
            }
            Focus::Canvas | Focus::Overlay => match app.active_view {
                CanvasView::Page => {
                    ui.canvas_follow_selected = false;
                    let max_b = app.visible_blocks(ui).len().saturating_sub(1);
                    ui.selected_block = max_b;
                    ui.canvas_scroll = u16::MAX / 2;
                }
                CanvasView::Database => match ui.db_view_mode {
                    DatabaseViewMode::Table | DatabaseViewMode::List => {
                        ui.db_selected_row =
                            app.filtered_sorted_db_rows(ui).len().saturating_sub(1);
                    }
                    DatabaseViewMode::Board => {
                        let cols = app.build_board_columns(ui);
                        if !cols.is_empty() {
                            ui.db_board_col = ui.db_board_col.min(cols.len() - 1);
                            ui.db_board_row = cols[ui.db_board_col].cards.len().saturating_sub(1);
                        }
                    }
                },
            },
        },
        Action::Activate => match ui.focus {
            Focus::Sidebar => {
                let items = app.build_sidebar_items(ui);
                if let Some(item) = items.get(ui.sidebar_selected).cloned() {
                    if item.is_section_header {
                        app.snap_sidebar_selection(ui);
                        return Ok(false);
                    }
                    let opened = if item.is_database {
                        app.try_open_database(&item.id, ui, true)
                    } else {
                        app.try_open_page(&item.id, ui, true)
                    };
                    if opened {
                        ui.focus = Focus::Canvas;
                        ui.narrow_pane = NarrowPane::Canvas;
                    }
                }
            }
            Focus::Canvas => match app.active_view {
                CanvasView::Page => {
                    let visible = app.visible_blocks(ui);
                    if let Some(vblk) = visible.get(ui.selected_block) {
                        if is_block_enter_editable(&vblk.block.kind) {
                            app.open_block_editor(ui, ui.selected_block, BlockCursorInit::End);
                            return Ok(false);
                        }
                    }
                    return Ok(dispatch(Action::ToggleActiveBlock, app, ui));
                }
                CanvasView::Database => {
                    if let Some(page) = selected_db_row_page(app, ui) {
                        let _ = app.try_open_page(&page.id, ui, true);
                    }
                }
            },
            Focus::Comments => {
                return Ok(dispatch(Action::SubmitCommentReply, app, ui));
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
                        let children_unloaded =
                            vblk.block.has_children && vblk.block.children.is_empty();
                        if !ui.expanded_toggles.remove(&id) {
                            ui.expanded_toggles.insert(id.clone());
                            if children_unloaded {
                                app.hydrate_block_children(&id, ui);
                            }
                        }
                        app.clamp_canvas_selection(ui);
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
                        if is_html_or_xml_ref(url, None) {
                            let title = url
                                .rsplit('/')
                                .next()
                                .unwrap_or("HTML Preview")
                                .to_string();
                            let u = url.clone();
                            let bid = vblk.block.id.clone();
                            let pid = app.active_page_id.clone();
                            return Ok(dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            ));
                        }
                        return Ok(dispatch(Action::OpenImagePreview, app, ui));
                    }
                    NotionBlockKind::Video { url, .. } | NotionBlockKind::Audio { url, .. } => {
                        let bid = vblk.block.id.clone();
                        let u = url.clone();
                        let pid = app.active_page_id.clone();
                        return Ok(dispatch(
                            Action::OpenMediaUrl {
                                url: u,
                                block_id: (!bid.is_empty()).then_some(bid),
                                page_id: pid,
                            },
                            app,
                            ui,
                        ));
                    }
                    NotionBlockKind::Bookmark { url, .. }
                    | NotionBlockKind::Embed { url, .. }
                    | NotionBlockKind::LinkPreview { url } => {
                        let u = url.clone();
                        return Ok(dispatch(Action::OpenUrl(u), app, ui));
                    }
                    NotionBlockKind::File { url, name, .. } => {
                        let bid = vblk.block.id.clone();
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
                            return Ok(dispatch(
                                Action::FetchHtmlPreview {
                                    title,
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: pid,
                                },
                                app,
                                ui,
                            ));
                        }
                        let media_idx = app
                            .active_page_detail
                            .as_ref()
                            .and_then(|d| find_media_index_for_block(d, &bid, &u));
                        if let Some(midx) = media_idx {
                            return Ok(dispatch(Action::DownloadMedia(midx), app, ui));
                        } else if !u.trim().is_empty() {
                            return Ok(dispatch(
                                Action::OpenMediaUrl {
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: app.active_page_id.clone(),
                                },
                                app,
                                ui,
                            ));
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
                            return Ok(dispatch(Action::DownloadMedia(midx), app, ui));
                        } else if !u.trim().is_empty() {
                            return Ok(dispatch(
                                Action::OpenMediaUrl {
                                    url: u,
                                    block_id: (!bid.is_empty()).then_some(bid),
                                    page_id: app.active_page_id.clone(),
                                },
                                app,
                                ui,
                            ));
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
            return Ok(dispatch(Action::ToggleActiveBlock, app, ui));
        }
        Action::ActivateChildBlockAt(idx) => {
            ui.selected_block = idx;
            return Ok(dispatch(Action::ToggleActiveBlock, app, ui));
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
                            app.snap_sidebar_selection(ui);
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
                            app.snap_sidebar_selection(ui);
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
        Action::ToggleSidebarFavorite { id, is_database } => {
            let ws = app.workspace_id.clone();
            if is_database {
                if let Ok(fav) = app.storage.toggle_database_favorite(&ws, &id) {
                    if let Some(d) = app.databases.iter_mut().find(|d| d.id == id) {
                        d.is_favorite = fav;
                    }
                    app.snap_sidebar_selection(ui);
                    ui.success(if fav {
                        "Database added to Favorites ★"
                    } else {
                        "Database removed from Favorites"
                    });
                }
            } else if let Ok(fav) = app.storage.toggle_page_favorite(&ws, &id) {
                if let Some(p) = app.pages.iter_mut().find(|p| p.id == id) {
                    p.is_favorite = fav;
                }
                if let Some(ref mut d) = app.active_page_detail {
                    if d.page.id == id {
                        d.page.is_favorite = fav;
                    }
                }
                app.snap_sidebar_selection(ui);
                ui.success(if fav {
                    "Added to Favorites ★"
                } else {
                    "Removed from Favorites"
                });
            }
        }
        Action::ToggleCommentsDrawer => {
            if ui.comments_drawer_open {
                ui.comments_drawer_open = false;
                if ui.focus == Focus::Comments {
                    ui.focus = Focus::Canvas;
                }
            } else if app.active_view != CanvasView::Page {
                ui.warning("Comments drawer is only available on pages");
            } else if !can_show_comments_drawer(app, ui) {
                ui.warning("Terminal too narrow for Comments drawer (need ≥72 canvas cols)");
            } else {
                ui.comments_drawer_open = true;
                ui.focus = Focus::Comments;
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
                return Ok(false);
            }
            if app.active_view != CanvasView::Page {
                ui.warning("Open a page to add a comment");
                return Ok(false);
            }
            let Some(page_id) = app.active_page_id.clone() else {
                ui.warning("Open a page to add a comment");
                return Ok(false);
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
        Action::NavigateBack => {
            if app.nav_stack.is_empty() {
                return Ok(false);
            }
            if ui.block_editor.is_some() {
                app.commit_and_sync_block_editor(ui, true);
            }
            if ui.inline_editor.is_some() && !app.close_inline_editor_and_flush(ui) {
                ui.focus = Focus::Canvas;
                ui.warning("Resolve or discard inline edits (Esc x2) before navigating back");
                return Ok(false);
            }
            if let Some(prev) = app.nav_stack.pop() {
                match prev {
                    crate::ui::app::NavTarget::Page(pid) => app.open_page(&pid, ui, false),
                    crate::ui::app::NavTarget::Database(did) => app.open_database(&did, ui, false),
                }
            }
        }
        Action::NavigateBreadcrumb(idx) => {
            if idx < app.nav_stack.len() {
                if ui.block_editor.is_some() {
                    app.commit_and_sync_block_editor(ui, true);
                }
                if ui.inline_editor.is_some() && !app.close_inline_editor_and_flush(ui) {
                    ui.focus = Focus::Canvas;
                    ui.warning("Resolve or discard inline edits (Esc x2) before navigating");
                    return Ok(false);
                }
                let target = app.nav_stack[idx].clone();
                app.nav_stack.truncate(idx);
                match target {
                    crate::ui::app::NavTarget::Page(pid) => app.open_page(&pid, ui, false),
                    crate::ui::app::NavTarget::Database(did) => app.open_database(&did, ui, false),
                }
            }
        }
        Action::OpenPageById(pid) => {
            if app.try_open_page(&pid, ui, true) {
                ui.focus = Focus::Canvas;
            }
        }
        Action::OpenDatabaseById(did) => {
            if app.try_open_database(&did, ui, true) {
                ui.focus = Focus::Canvas;
            }
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
        Action::FullResync => {
            // Runs here rather than as a SyncCommand so a daemon of an older version keeps
            // understanding every command this TUI sends. Capture the workspace ID bound to
            // `storage` so a mid-resync workspace switch aborts instead of polluting SQLite.
            let client = app.client.clone();
            let storage = app.storage.clone();
            let ui_tx = app.ui_tx.clone();
            let expected_ws = app.workspace_id.clone();
            ui.tasks.begin("full resync");
            ui.info("Rescanning the whole workspace...");
            tokio::spawn(async move {
                match crate::sync::sync_workspace_tree_with_options_for_workspace(
                    &client,
                    &storage,
                    Some(&expected_ws),
                    true,
                    |_, _, _| {},
                )
                .await
                {
                    Ok((ws_id, pages, databases, outcome)) => {
                        let summaries = pages
                            .iter()
                            .map(crate::models::NotionPageSummary::from)
                            .collect();
                        let _ = ui_tx.send(UiMsg::WorkspaceTreeRefreshed {
                            workspace_id: ws_id,
                            pages: summaries,
                            databases,
                            task_label: "full resync",
                            toast: format!("Full resync read {} items", outcome.fetched),
                        });
                    }
                    Err(err) => {
                        let _ = ui_tx.send(UiMsg::TaskFailed {
                            label: "full resync".into(),
                            error: err.to_string(),
                        });
                    }
                }
            });
        }
        Action::SetTheme(preset) => {
            app.config.theme = preset;
            ui.theme = crate::ui::theme::Theme::detect(preset);
            app.clear_image_cache();
            let _ = app.config.save();
            ui.info(format!("Theme: {}", preset.label()));
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return Ok(dispatch(Action::LoadPreviewImage, app, ui));
            }
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
        other => return Err(other),
    }
    Ok(false)
}
