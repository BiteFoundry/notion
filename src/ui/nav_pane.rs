//! Iconic Notion Left Workspace Sidebar (`src/ui/nav_pane.rs`):
//! - ★ Favorites
//! - Workspace hierarchical collapsible tree (`▸`/`▾`, emoji/type icons, indent guides)
//! - Databases quick section
//! - Recent / Trash

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use crate::ui::{
    app::{AppState, CanvasView, Focus, UiState},
    hitmap::{HitMap, HitTarget},
};

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

    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)),
        area,
    );

    if area.width < 6 || area.height < 2 {
        return;
    }

    let items = app.build_sidebar_items(ui);
    if items.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("  (Syncing workspace...)", theme.muted_style()))
                .style(Style::default().bg(theme.bg_sidebar)),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
        return;
    }

    let visible_rows = area.height as usize;
    let selected = ui.sidebar_selected.min(items.len().saturating_sub(1));
    let offset = selection_offset(selected, visible_rows, items.len());

    for (row_idx, item) in items.iter().skip(offset).take(visible_rows).enumerate() {
        let idx = offset + row_idx;
        let y = area.y + row_idx as u16;
        let row_rect = Rect::new(area.x, y, area.width, 1);

        if item.is_section_header {
            let header = if item.icon.is_empty() {
                format!("  {}", item.label)
            } else {
                format!("  {} {}", item.icon, item.label)
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&header, area.width as usize),
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

        let indent = "  ".repeat(item.depth);
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
        let label_budget = (area.width as usize)
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
        hitmap.add_line(area.x, y, area.width, HitTarget::SidebarRow(idx));
    }
}
