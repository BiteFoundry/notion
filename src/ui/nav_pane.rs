//! Iconic Notion Left Workspace Sidebar (`src/ui/nav_pane.rs`):
//! - ★ Favorites
//! - Workspace hierarchical collapsible tree (`▸`/`▾`, emoji/type icons, indent guides)
//! - Databases quick section
//! - Recent / Trash

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
    Frame,
};

use crate::ui::{
    app::{AppState, CanvasView, Focus, UiState},
    hitmap::{HitMap, HitTarget},
};

/// Deepest indentation level drawn in the sidebar. Deeper pages keep rendering at this indent
/// (the tree itself is listed much deeper) so their labels never get pushed out of a narrow pane.
pub const SIDEBAR_MAX_INDENT_LEVEL: usize = 6;

/// Visual indent level for a sidebar tree depth; also positions the `SidebarToggle` arrow hit.
pub fn sidebar_indent_level(depth: usize) -> usize {
    depth.min(SIDEBAR_MAX_INDENT_LEVEL)
}

pub fn selection_offset(selected: usize, visible_rows: usize, total: usize) -> usize {
    if visible_rows == 0 || total <= visible_rows {
        return 0;
    }
    let half = visible_rows / 2;
    let max_off = total.saturating_sub(visible_rows);
    selected.saturating_sub(half).min(max_off)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let focused = ui.focus == Focus::Sidebar;

    hitmap.add(area, HitTarget::Pane(Focus::Sidebar));

    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)),
        area,
    );

    if area.width < 6 || area.height < 2 {
        return;
    }

    let items = app.sidebar_items_cached(ui);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (Syncing workspace...)", theme.muted_style()))
                .style(Style::default().bg(theme.bg_sidebar)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        return;
    }

    let visible_rows = area.height as usize;
    let raw_selected = ui.sidebar_selected.min(items.len().saturating_sub(1));
    let selected = if items[raw_selected].is_section_header {
        ((raw_selected + 1)..items.len())
            .find(|&i| !items[i].is_section_header)
            .or_else(|| (0..raw_selected).rev().find(|&i| !items[i].is_section_header))
            .unwrap_or(raw_selected)
    } else {
        raw_selected
    };
    let offset = selection_offset(selected, visible_rows, items.len());
    let show_scrollbar = items.len() > visible_rows && area.width >= 8;
    let content_w = if show_scrollbar {
        area.width.saturating_sub(1)
    } else {
        area.width
    };

    for (row_idx, item) in items.iter().skip(offset).take(visible_rows).enumerate() {
        let idx = offset + row_idx;
        let y = area.y + row_idx as u16;
        let row_rect = Rect::new(area.x, y, content_w, 1);

        if item.is_section_header {
            let header = if item.icon.is_empty() {
                format!("  {}", item.label)
            } else {
                format!("  {} {}", item.icon, item.label)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&header, content_w as usize),
                    theme.muted_style().add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(theme.bg_sidebar)),
                row_rect,
            );
            continue;
        }

        let is_active_open = match app.active_view {
            CanvasView::Page => !item.is_database && app.active_page_id.as_deref() == Some(item.id.as_str()),
            CanvasView::Database => item.is_database && app.active_database_id.as_deref() == Some(item.id.as_str()),
        };
        let is_sel = focused && idx == selected;

        let indent_level = sidebar_indent_level(item.depth);
        let indent = "  ".repeat(indent_level);
        let arrow = if item.collapsible {
            if item.collapsed {
                "▸ "
            } else {
                "▾ "
            }
        } else if item.depth > 0 {
            "└ "
        } else {
            "  "
        };

        let fav_suffix = if item.is_favorite { " ★" } else { "" };
        let prefix = format!(" {indent}{arrow}{} ", item.icon);
        let label_budget = (content_w as usize)
            .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()) + fav_suffix.len() + 1)
            .max(4);
        let text = format!(
            "{prefix}{}{fav_suffix}",
            crate::ui::truncate_width(&item.label, label_budget)
        );

        let row_style = if is_sel {
            theme
                .selection()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD)
        } else if is_active_open {
            Style::default()
                .bg(theme.bg_elevated)
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD)
        } else if item.archived {
            Style::default()
                .bg(theme.bg_sidebar)
                .fg(theme.fg_muted)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().bg(theme.bg_sidebar).fg(theme.fg_secondary)
        };

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, row_style))).style(row_style),
            row_rect,
        );
        hitmap.add_line(area.x, y, content_w, HitTarget::SidebarRow(idx));
        if item.collapsible {
            let arrow_offset = (indent_level as u16).saturating_mul(2);
            let arrow_x = area.x.saturating_add(arrow_offset);
            let arrow_w = 3u16.min(content_w.saturating_sub(arrow_offset));
            if arrow_w > 0 {
                hitmap.add_line(arrow_x, y, arrow_w, HitTarget::SidebarToggle(idx));
            }
        }
    }

    if show_scrollbar {
        let max_off = items.len().saturating_sub(visible_rows);
        let mut sb_state = ScrollbarState::new(max_off.saturating_add(1))
            .position(offset.min(max_off))
            .viewport_content_length(visible_rows);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().fg(theme.divider).bg(theme.bg_sidebar))
            .thumb_style(Style::default().fg(theme.fg_muted).bg(theme.bg_sidebar));
        frame.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}
