//! Root UI module: Notion minimal-chrome breadcrumb bar, collapsible Workspace sidebar,
//! adaptive Page / Database canvas, status bar, and overlay stack.

pub mod action;
pub mod app;
pub mod composer;
pub mod event;
pub mod hitmap;
pub mod keymap;
pub mod nav_pane;
pub mod notification;
pub mod notion_view;
pub mod overlays;
pub mod theme;
pub mod toast;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::{
    app::{AppState, CanvasView, Focus, NarrowPane, UiState},
    hitmap::{ButtonId, HitMap, HitTarget},
    toast::ToastKind,
};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn truncate_width(s: &str, max_w: usize) -> String {
    if max_w == 0 {
        return String::new();
    }
    if s.width() <= max_w {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    let budget = max_w.saturating_sub(1);
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

pub fn draw(frame: &mut Frame, app: &mut AppState, ui: &mut UiState) -> Option<(u16, u16)> {
    let area = frame.area();
    ui.term_width = area.width;
    ui.term_height = area.height;
    ui.pending_pixel_graphics.clear();
    ui.cursor_pos = None;

    let mut hitmap = HitMap::new();
    hitmap.add(area, HitTarget::Pane(ui.focus));
    let theme = ui.theme.clone();

    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_canvas).fg(theme.fg_primary)),
        area,
    );

    if area.width < 12 || area.height < 4 {
        let status = Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1);
        draw_status_bar(frame, status, app, ui, &mut hitmap);
        ui.hitmap = hitmap;
        return None;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Minimal Notion Top Breadcrumb Bar
            Constraint::Min(3),    // Sidebar + Main Canvas
            Constraint::Length(1), // Subtle Footer Status Bar
        ])
        .split(area);
    let top_bar = rows[0];
    let body = rows[1];
    let status = rows[2];

    notion_view::draw_breadcrumb_bar(frame, top_bar, app, ui, &mut hitmap);

    if area.width < 80 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(2)])
            .split(body);
        draw_narrow_bar(frame, split[0], ui, &mut hitmap);
        match ui.narrow_pane {
            NarrowPane::Sidebar => nav_pane::draw(frame, split[1], app, ui, &mut hitmap),
            NarrowPane::Canvas => notion_view::draw(frame, split[1], app, ui, &mut hitmap),
        }
    } else if ui.show_sidebar {
        let nav_w = if area.width >= 120 { 28 } else { 24 };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(nav_w),
                Constraint::Length(1),
                Constraint::Min(36),
            ])
            .split(body);
        nav_pane::draw(frame, cols[0], app, ui, &mut hitmap);
        draw_vertical_divider(frame, cols[1], &theme);
        notion_view::draw(frame, cols[2], app, ui, &mut hitmap);
    } else {
        notion_view::draw(frame, body, app, ui, &mut hitmap);
    }

    draw_status_bar(frame, status, app, ui, &mut hitmap);

    // Render overlay stack bottom-to-top
    let mut overlays = std::mem::take(&mut ui.overlays);
    if !overlays.is_empty() {
        // Active overlay owns focus & terminal cursor instead of inline canvas editor
        ui.cursor_pos = None;
        for ov in &mut overlays {
            ui.pending_pixel_graphics.clear();
            if let Some(pos) = ov.draw(frame, area, app, ui, &mut hitmap) {
                ui.cursor_pos = Some(pos);
            }
        }
    }
    ui.overlays = overlays;

    if let Some((cx, cy)) = ui.cursor_pos {
        frame.set_cursor_position((cx, cy));
    }

    ui.hitmap = hitmap;
    ui.cursor_pos
}

fn draw_vertical_divider(frame: &mut Frame, area: Rect, theme: &theme::Theme) {
    let lines: Vec<Line<'static>> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(theme.border))))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg_canvas)),
        area,
    );
}

fn draw_narrow_bar(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let pills = [
        (" [Sidebar] ", NarrowPane::Sidebar, ButtonId::NarrowSidebar),
        (" [Canvas] ", NarrowPane::Canvas, ButtonId::NarrowCanvas),
    ];
    let mut x = area.x;
    let mut spans = Vec::new();
    for (label, pane, btn) in pills {
        let active = ui.narrow_pane == pane;
        let style = if active {
            theme.selection().add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(theme.bg_header).fg(theme.fg_muted)
        };
        let w = (label.width() as u16).min(area.right().saturating_sub(x));
        if w == 0 {
            break;
        }
        spans.push(Span::styled(label, style));
        hitmap.add_line(x, area.y, w, HitTarget::Button(btn));
        x = x.saturating_add(w);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg_header)),
        area,
    );
}

fn draw_status_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bg = theme.bg_header;

    let mut left_spans: Vec<Span<'static>> = Vec::new();
    if let Some(toast) = ui.toasts.latest() {
        let fg = match toast.kind {
            ToastKind::Info => theme.accent_soft,
            ToastKind::Success => theme.success,
            ToastKind::Warning => theme.warning,
            ToastKind::Error => theme.error,
        };
        left_spans.push(Span::styled(
            format!(" {} ", toast.message),
            Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
        ));
    } else if let Some(task) = ui.tasks.first_running() {
        let spin = SPINNER_FRAMES[ui.spinner_tick % SPINNER_FRAMES.len()];
        left_spans.push(Span::styled(
            format!(" {spin} {}... ", task.label),
            Style::default().bg(bg).fg(theme.accent_soft),
        ));
    } else {
        let spin = if app.syncing || ui.poll_retrying {
            format!("{} ", SPINNER_FRAMES[ui.spinner_tick % SPINNER_FRAMES.len()])
        } else {
            String::new()
        };
        let focus_tag = match ui.focus {
            Focus::Sidebar => "Sidebar",
            Focus::Canvas if ui.inline_editor.is_some() || ui.block_editor.is_some() => "Editing",
            Focus::Canvas if app.active_view == CanvasView::Database => "Database",
            Focus::Canvas => "Page",
            Focus::Comments => "Comments",
            Focus::Overlay => "Overlay",
        };
        let hints = if ui.block_editor.is_some() {
            "Enter New Block · Shift+Enter Newline · ↑/↓ Prev/Next Block · Ctrl+S Save · Esc Done"
        } else if ui.inline_editor.is_some() {
            "Ctrl+S Save · Esc Done/Discard · Ctrl+Z/Y Undo/Redo · Ctrl+V Paste · Ctrl+E $EDITOR"
        } else {
            match app.active_view {
                CanvasView::Page => {
                    "Click/Enter Edit Block · a Append · e Full Edit · n New · C Comments · Ctrl+K Find · ? Help"
                }
                CanvasView::Database => {
                    "1 Table · 2 Board · 3 List · f Filter · s Sort · m Move · n New · Ctrl+K Find"
                }
            }
        };
        left_spans.push(Span::styled(
            format!(" {spin}[{focus_tag}] "),
            Style::default()
                .bg(bg)
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
        left_spans.push(Span::styled(
            hints.to_string(),
            Style::default().bg(bg).fg(theme.fg_muted),
        ));
    }

    let right_pills = [
        (" Ctrl+K Cmd ", ButtonId::CommandPalette),
        (" Ctrl+S WS ", ButtonId::WorkspaceSwitcher),
        (" Ctrl+, ", ButtonId::Settings),
    ];
    let right_w: u16 = right_pills.iter().map(|(l, _)| l.width() as u16).sum();
    let left_w = area.width.saturating_sub(right_w);

    frame.render_widget(
        Paragraph::new(Line::from(left_spans)).style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, left_w, 1),
    );

    let mut rx = area.x.saturating_add(left_w);
    for (label, btn) in right_pills {
        let w = (label.width() as u16).min(area.right().saturating_sub(rx));
        if w == 0 {
            break;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(
                label,
                Style::default().bg(bg).fg(theme.fg_muted),
            )),
            Rect::new(rx, area.y, w, 1),
        );
        hitmap.add_line(rx, area.y, w, HitTarget::Button(btn));
        rx = rx.saturating_add(w);
    }
}
