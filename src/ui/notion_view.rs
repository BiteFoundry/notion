//! Signature Notion-Native Terminal Canvas (`src/ui/notion_view.rs`):
//! - Minimal 1-line Top Breadcrumb Bar (`Workspace ▾ / Parent / 📄 Page` + `[Width]` `[Sync]` `[? Help]`)
//! - Adaptive Main Canvas:
//!   - Editorial Page Canvas (Centered ~88 col measure vs Full Width, Page Header + Properties Strip,
//!     Interactive Blocks: Callouts, Toggles, To-do Checkboxes, Code Blocks, Unicode Tables,
//!     Inline Images, Child Page/Database Cards, + Comments Peek Drawer `C`)
//!   - Database View (`1 Table`, `2 Board` Kanban, `3 List`, Filter `f`, Sort `s`, New Item `n`).

use bitefoundry_html::layout::{
    pad_display_width, render_wrapped_table, wrap_styled_spans, TableAlign, TableBorder, TableSpec,
    WrapPrefix,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    config::PageWidthMode,
    models::{
        spans_plain_text, DatabaseViewMode, NotionBlockKind, NotionColor, NotionDatabase,
        NotionPageDetail, NotionPropertyValue, RichTextKind, RichTextSpan,
    },
    ui::{
        app::{
            AppState, CanvasBlockLayout, CanvasLinkHit, CanvasView, Focus, InlineBlockEditor,
            InlineSyncStatus, UiState, VisibleBlock,
        },
        composer::{text_layout, truncate_display},
        hitmap::{ButtonId, HitMap, HitTarget},
        theme::Theme,
        truncate_width,
    },
};

const EDITORIAL_MAX_COLS: u16 = 88;

/// Renders the minimal 1-line Notion top breadcrumb bar.
pub fn draw_breadcrumb_bar(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let bg = theme.bg_header;

    let search_pill = " [/ Find] ".to_string();
    let cmd_pill = " [⌘] ".to_string();
    let width_pill = format!(" [Width: {}] ", ui.page_width.label());
    let sync_pill = if app.syncing {
        " [Syncing...] ".to_string()
    } else {
        " [Sync] ".to_string()
    };
    let help_pill = " [? Help] ".to_string();

    let show_extra_right = area.width >= 96;
    let right_total = (width_pill.width()
        + sync_pill.width()
        + help_pill.width()
        + if show_extra_right {
            search_pill.width() + cmd_pill.width()
        } else {
            0
        }) as u16;
    let left_w = area.width.saturating_sub(right_total);

    // Left breadcrumb segments
    let mut x = area.x;
    let max_x = area.x.saturating_add(left_w);
    let mut spans: Vec<Span<'static>> = Vec::new();

    let sb_label = if ui.show_sidebar { " [☰]" } else { " [☰+]" };
    let sb_w = (sb_label.width() as u16).min(max_x.saturating_sub(x));
    if sb_w > 0 {
        spans.push(Span::styled(
            sb_label,
            Style::default().bg(bg).fg(theme.fg_muted),
        ));
        hitmap.add_line(x, area.y, sb_w, HitTarget::Button(ButtonId::ToggleSidebar));
        x = x.saturating_add(sb_w);
    }

    let ws_label = format!(" {} ▾ ", truncate_width(&app.workspace_name, 20));
    let ws_w = (ws_label.width() as u16).min(max_x.saturating_sub(x));
    if ws_w > 0 {
        spans.push(Span::styled(
            ws_label,
            Style::default()
                .bg(bg)
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ));
        hitmap.add_line(x, area.y, ws_w, HitTarget::Button(ButtonId::WorkspaceSwitcher));
        x = x.saturating_add(ws_w);
    }

    let crumbs = app.breadcrumb_labels();
    for (idx, crumb) in crumbs.iter().enumerate() {
        if x >= max_x {
            break;
        }
        let sep = "/ ";
        spans.push(Span::styled(sep, Style::default().bg(bg).fg(theme.fg_subtle)));
        x = x.saturating_add(sep.width() as u16);

        let seg = format!("{} ", truncate_width(crumb, 20));
        let seg_w = (seg.width() as u16).min(max_x.saturating_sub(x));
        if seg_w == 0 {
            break;
        }
        spans.push(Span::styled(seg, Style::default().bg(bg).fg(theme.fg_muted)));
        hitmap.add_line(x, area.y, seg_w, HitTarget::BreadcrumbSegment(idx));
        x = x.saturating_add(seg_w);
    }

    let current_title = match app.active_view {
        CanvasView::Page => app
            .active_page_detail
            .as_ref()
            .map(|d| format!("{} {}", d.page.display_icon(), d.page.display_title()))
            .unwrap_or_else(|| "📄 Welcome".to_string()),
        CanvasView::Database => app
            .active_database()
            .map(|d| format!("{} {}", d.display_icon(), d.display_title()))
            .unwrap_or_else(|| "🗂 Database".to_string()),
    };
    if x < max_x {
        spans.push(Span::styled("/ ", Style::default().bg(bg).fg(theme.fg_subtle)));
        let remain = max_x.saturating_sub(x + 2) as usize;
        spans.push(Span::styled(
            truncate_width(&current_title, remain),
            Style::default()
                .bg(bg)
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        ));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, left_w, 1),
    );

    // Right pills: [/ Find] [⌘] [Width: Centered|Full] [Sync] [? Help]
    let mut rx = area.x.saturating_add(left_w);
    let mut pills = Vec::with_capacity(5);
    if show_extra_right {
        pills.push((search_pill, ButtonId::Search, theme.fg_muted));
        pills.push((cmd_pill, ButtonId::CommandPalette, theme.fg_muted));
    }
    pills.push((width_pill, ButtonId::ToggleWidth, theme.fg_muted));
    pills.push((
        sync_pill,
        ButtonId::SyncNow,
        if app.syncing {
            theme.accent_soft
        } else {
            theme.fg_muted
        },
    ));
    pills.push((help_pill, ButtonId::Help, theme.fg_muted));
    for (label, btn, fg) in pills {
        let w = (label.width() as u16).min(area.right().saturating_sub(rx));
        if w == 0 {
            break;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(label, Style::default().bg(bg).fg(fg))),
            Rect::new(rx, area.y, w, 1),
        );
        hitmap.add_line(rx, area.y, w, HitTarget::Button(btn));
        rx = rx.saturating_add(w);
    }
}

/// Main Canvas dispatcher: automatically adapts between Notion Page Canvas and Database View.
pub fn draw(frame: &mut Frame, area: Rect, app: &AppState, ui: &mut UiState, hitmap: &mut HitMap) {
    match app.active_view {
        CanvasView::Page => draw_page_workspace(frame, area, app, ui, hitmap),
        CanvasView::Database => draw_database_view(frame, area, app, ui, hitmap),
    }
}

fn draw_page_workspace(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if ui.comments_drawer_open && area.width >= 72 {
        let drawer_w = 34u16.min(area.width / 3).max(26);
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(36),
                Constraint::Length(1),
                Constraint::Length(drawer_w),
            ])
            .split(area);
        draw_page_canvas(frame, split[0], app, ui, hitmap);
        let div_lines: Vec<Line<'static>> = (0..split[1].height)
            .map(|_| Line::from(Span::styled("│", Style::default().fg(ui.theme.border))))
            .collect();
        frame.render_widget(
            Paragraph::new(div_lines).style(Style::default().bg(ui.theme.bg_canvas)),
            split[1],
        );
        draw_comments_drawer(frame, split[2], app, ui, hitmap);
    } else {
        draw_page_canvas(frame, area, app, ui, hitmap);
    }
}

fn draw_page_canvas(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    hitmap.add(area, HitTarget::Pane(Focus::Canvas));

    let theme = ui.theme.clone();
    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_canvas).fg(theme.fg_primary)),
        area,
    );

    if area.width < 12 || area.height < 3 {
        return;
    }

    // Centered Editorial Measure (~88 cols) vs Full Width
    let content_area = match ui.page_width {
        PageWidthMode::Centered if area.width > EDITORIAL_MAX_COLS + 4 => {
            let pad = (area.width - EDITORIAL_MAX_COLS) / 2;
            Rect::new(area.x + pad, area.y, EDITORIAL_MAX_COLS, area.height)
        }
        _ => Rect::new(
            area.x + 2,
            area.y,
            area.width.saturating_sub(4).max(1),
            area.height,
        ),
    };

    let Some(detail) = app.active_page_detail.as_ref() else {
        ui.canvas_scroll = 0;
        ui.canvas_block_layouts.clear();
        ui.canvas_link_hits.clear();
        draw_empty_page_welcome(frame, content_area, &theme);
        return;
    };

    if ui
        .inline_editor
        .as_ref()
        .is_some_and(|ed| ed.page_id == detail.page.id)
    {
        ui.canvas_block_layouts.clear();
        ui.canvas_link_hits.clear();
        draw_inline_page_editor(frame, area, content_area, &theme, ui, hitmap);
        return;
    }

    let scene = build_page_canvas_scene(app, detail, ui, &theme, content_area.width);
    let rendered_lines = scene.lines;
    let inline_hits = scene.inline_hits;
    let selected_line_range = scene.selected_line_range;

    // Keep the selected block (or active block-editor cursor line) visible when navigating
    // with keyboard, while allowing MouseScroll to roam freely.
    if ui.canvas_follow_selected {
        let view_h = content_area.height as usize;
        let mut desired_scroll = ui.canvas_scroll as usize;
        if let Some(ref active) = scene.active_block_edit {
            let cursor_line = active.text_start_line.saturating_add(active.cursor_row);
            if cursor_line < desired_scroll {
                desired_scroll = cursor_line;
            } else if cursor_line >= desired_scroll.saturating_add(view_h) {
                desired_scroll = cursor_line.saturating_add(1).saturating_sub(view_h);
            }
            ui.canvas_scroll = desired_scroll.min(u16::MAX as usize) as u16;
        } else if let Some((sel_start, sel_end)) = selected_line_range {
            if sel_start < desired_scroll {
                desired_scroll = sel_start;
            } else if sel_end >= desired_scroll.saturating_add(view_h) {
                desired_scroll = sel_end.saturating_add(1).saturating_sub(view_h);
            }
            ui.canvas_scroll = desired_scroll.min(u16::MAX as usize) as u16;
        }
        ui.canvas_follow_selected = false;
    }

    // Compute and clamp viewport scroll in-place so ScrollBottom / wheel down never outruns content
    let max_scroll = rendered_lines
        .len()
        .saturating_sub(content_area.height as usize);
    let scroll = (ui.canvas_scroll as usize).min(max_scroll);
    ui.canvas_scroll = scroll as u16;

    // Record screen geometry of visible blocks & inline links for direct click-to-edit and link clicks.
    ui.canvas_block_layouts.clear();
    ui.canvas_link_hits.clear();
    let view_h = content_area.height as usize;
    let view_end = scroll.saturating_add(view_h);
    for meta in &scene.block_meta {
        if meta.end_line >= scroll && meta.start_line < view_end {
            let first_screen_row = content_area
                .y
                .saturating_add(meta.start_line.saturating_sub(scroll) as u16);
            let last_screen_row = content_area.y.saturating_add(
                meta.end_line
                    .saturating_sub(scroll)
                    .min(view_h.saturating_sub(1)) as u16,
            );
            let text_first_screen_row = content_area
                .y
                .saturating_add(meta.text_start_line.saturating_sub(scroll) as u16);
            ui.canvas_block_layouts.push(CanvasBlockLayout {
                visible_idx: meta.visible_idx,
                block_id: meta.block_id.clone(),
                first_screen_row,
                last_screen_row,
                text_first_screen_row,
                first_text_x: content_area.x.saturating_add(meta.first_prefix_w),
                cont_text_x: content_area.x.saturating_add(meta.cont_prefix_w),
                text_width: meta.text_width,
                has_leading_toggle: meta.has_leading_toggle,
            });
        }
    }

    for (row_offset, (line, target)) in rendered_lines
        .iter()
        .skip(scroll)
        .take(view_h)
        .enumerate()
    {
        let line_idx = scroll + row_offset;
        let y = content_area.y + row_offset as u16;
        let row_rect = Rect::new(content_area.x, y, content_area.width, 1);
        frame.render_widget(Paragraph::new(line.clone()), row_rect);
        if let Some(hit) = *target {
            hitmap.add_line(content_area.x, y, content_area.width, hit);
        }
        for &(hl_idx, rel_x, w, hit) in &inline_hits {
            if hl_idx == line_idx && rel_x < content_area.width {
                let clamped_w = w.min(content_area.width.saturating_sub(rel_x));
                if clamped_w > 0 {
                    hitmap.add_line(content_area.x.saturating_add(rel_x), y, clamped_w, hit);
                }
            }
        }
        for &(ll_idx, rel_x, w, ref url) in &scene.inline_links {
            if ll_idx == line_idx && rel_x < content_area.width {
                let clamped_w = w.min(content_area.width.saturating_sub(rel_x));
                if clamped_w > 0 {
                    ui.canvas_link_hits.push(CanvasLinkHit {
                        rect: Rect::new(content_area.x.saturating_add(rel_x), y, clamped_w, 1),
                        url: url.clone(),
                    });
                }
            }
        }
    }

    // Update active in-place block editor screen geometry and expose exact hardware terminal cursor.
    if let (Some(active), Some(bed)) = (scene.active_block_edit, ui.block_editor.as_mut()) {
        bed.last_text_x = content_area.x.saturating_add(active.prefix_w);
        bed.last_first_screen_row = content_area
            .y
            .saturating_add(active.text_start_line.saturating_sub(scroll) as u16);
        bed.last_screen_rows = active.total_text_rows;
        bed.last_wrap_width = active.wrap_w;
        bed.last_visual_lines = active.visual_lines;
        bed.last_cursor_visual_row = active.cursor_row;

        let cursor_line_idx = active.text_start_line.saturating_add(active.cursor_row);
        if ui.focus == Focus::Canvas && cursor_line_idx >= scroll && cursor_line_idx < view_end {
            let cy = content_area
                .y
                .saturating_add((cursor_line_idx - scroll) as u16)
                .min(content_area.bottom().saturating_sub(1));
            let cx = content_area
                .x
                .saturating_add(active.prefix_w)
                .saturating_add(active.cursor_col as u16)
                .min(content_area.right().saturating_sub(1));
            ui.cursor_pos = Some((cx, cy));
        }
    }

    if rendered_lines.len() > content_area.height as usize && area.width >= 4 {
        let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1))
            .position(scroll)
            .viewport_content_length(content_area.height as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().fg(theme.divider).bg(theme.bg_canvas))
            .thumb_style(Style::default().fg(theme.fg_muted).bg(theme.bg_canvas));
        frame.render_stateful_widget(scrollbar, area, &mut sb_state);
    }
}

/// Renders the in-place inline Markdown editor directly inside the Page Canvas content area
/// (no floating popup modal), keeping breadcrumb bar, sidebar, width mode, and scroll intact.
fn draw_inline_page_editor(
    frame: &mut Frame,
    pane_area: Rect,
    content_area: Rect,
    theme: &Theme,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    let canvas_focused = ui.focus == Focus::Canvas;
    let Some(ed) = ui.inline_editor.as_mut() else {
        return;
    };

    if content_area.width < 8 || content_area.height < 2 {
        ed.last_editor_area = None;
        return;
    }

    // 1. Compact 1-line inline edit bar at top of Canvas content area
    let bar_rect = Rect::new(content_area.x, content_area.y, content_area.width, 1);
    let bar_bg = theme.bg_header;
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(bar_bg)),
        bar_rect,
    );

    let (raw_status_text, status_kind) = ed.sync_status();
    let status_text = if ed.confirm_discard {
        "⚠ Unsaved — Ctrl+S Save · Esc again Discard"
    } else {
        raw_status_text
    };
    let status_fg = if ed.confirm_discard {
        theme.warning
    } else {
        match status_kind {
            InlineSyncStatus::Synced => theme.success,
            InlineSyncStatus::Syncing => theme.accent_soft,
            InlineSyncStatus::Edited => theme.warning,
            InlineSyncStatus::Error => theme.error,
        }
    };

    let (ln, col) = ed.cursor_line_col();
    let render_pill = "[Render]";
    let save_pill = "[Ctrl+S Save]";
    let done_pill = "[✓ Done]";
    let ext_pill = "[Ext $EDITOR]";
    let pos_text = format!("Ln {ln}, Col {col}");

    let mut right_items: Vec<(String, Option<ButtonId>, Style)> = vec![
        (
            render_pill.to_string(),
            Some(ButtonId::ToggleEditorMode),
            Style::default()
                .fg(theme.accent)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
        ),
        (
            save_pill.to_string(),
            Some(ButtonId::SaveInlineEdit),
            Style::default()
                .fg(theme.accent_soft)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
        ),
        (
            done_pill.to_string(),
            Some(ButtonId::FinishInlineEdit),
            Style::default()
                .fg(theme.success)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if content_area.width >= 74 {
        right_items.push((
            ext_pill.to_string(),
            Some(ButtonId::EditExternal),
            Style::default().fg(theme.fg_muted).bg(bar_bg),
        ));
    }
    if content_area.width >= 62 {
        right_items.push((
            pos_text,
            None,
            Style::default().fg(theme.fg_muted).bg(bar_bg),
        ));
    }

    let right_w: u16 = right_items
        .iter()
        .enumerate()
        .map(|(i, (label, _, _))| label.width() as u16 + u16::from(i > 0))
        .sum::<u16>()
        .saturating_add(1);

    let left_budget = content_area.width.saturating_sub(right_w) as usize;
    let edit_badge = "✎ EDITING";
    let left_line = if left_budget > edit_badge.width() + 3 + status_text.width() {
        Line::from(vec![
            Span::styled(
                format!(" {edit_badge}"),
                Style::default()
                    .fg(theme.accent)
                    .bg(bar_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(theme.fg_subtle).bg(bar_bg)),
            Span::styled(
                status_text,
                Style::default()
                    .fg(status_fg)
                    .bg(bar_bg)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(Span::styled(
            truncate_width(&format!(" {edit_badge} · {status_text}"), left_budget),
            Style::default()
                .fg(status_fg)
                .bg(bar_bg)
                .add_modifier(Modifier::BOLD),
        ))
    };
    if left_budget > 0 {
        frame.render_widget(
            Paragraph::new(left_line).style(Style::default().bg(bar_bg)),
            Rect::new(bar_rect.x, bar_rect.y, left_budget as u16, 1),
        );
    }

    let mut rx = bar_rect.right().saturating_sub(right_w);
    for (idx, (label, btn, style)) in right_items.into_iter().enumerate() {
        if idx > 0 {
            rx = rx.saturating_add(1);
        }
        let w = (label.width() as u16).min(bar_rect.right().saturating_sub(rx));
        if w == 0 {
            break;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(label, style)),
            Rect::new(rx, bar_rect.y, w, 1),
        );
        if let Some(button_id) = btn {
            hitmap.add_line(rx, bar_rect.y, w, HitTarget::Button(button_id));
        }
        rx = rx.saturating_add(w);
    }

    // 2. Editable Markdown Body directly below the 1-line edit bar
    let body_y = content_area.y.saturating_add(1);
    let body_h = content_area.height.saturating_sub(1).max(1);
    let editor_area = Rect::new(content_area.x, body_y, content_area.width, body_h);
    ed.last_viewport_height = body_h;
    ed.last_editor_area = Some(editor_area);
    ed.composer.last_inner_width = editor_area.width;

    let text = ed.composer.text();
    let cursor = ed.composer.cursor();
    let selection = ed.composer.selection_range();
    let (visual, cursor_row, cursor_col) =
        text_layout(text, editor_area.width.max(1) as usize, cursor);

    let total_rows = visual.len();
    let view_h = body_h as usize;
    let max_scroll = total_rows.saturating_sub(view_h);

    let mut offset = ed.scroll_y as usize;
    if ed.follow_cursor {
        if cursor_row < offset {
            offset = cursor_row;
        } else if cursor_row >= offset.saturating_add(view_h) {
            offset = cursor_row.saturating_add(1).saturating_sub(view_h);
        }
    }
    offset = offset.min(max_scroll);
    ed.scroll_y = offset.min(u16::MAX as usize) as u16;
    ed.last_visual_lines = visual.clone();

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(view_h);
    if text.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_display(
                "Type Markdown here (# Heading, - [ ] Todo, > Quote, ```code```)...",
                editor_area.width as usize,
            ),
            theme.muted_style().add_modifier(Modifier::ITALIC),
        )));
    } else {
        let mut in_code_fence = code_fence_state_before(text, visual.get(offset).map_or(0, |r| r.0));
        for &(start, end) in visual.iter().skip(offset).take(view_h) {
            let is_logical_start = start == 0 || text.as_bytes().get(start - 1) == Some(&b'\n');
            let (logical_start, logical_end) = logical_line_bounds(text, start);
            let logical_line = text.get(logical_start..logical_end).unwrap_or("");
            let is_fence_delim = logical_line.trim_start().starts_with("```");

            let line = highlight_markdown_visual_slice(
                text,
                start,
                end,
                logical_start,
                logical_line,
                in_code_fence,
                is_fence_delim,
                selection,
                theme,
            );
            lines.push(line);

            if is_logical_start && is_fence_delim {
                in_code_fence = !in_code_fence;
            }
        }
    }

    let body_surface = Style::default().bg(theme.bg_canvas).fg(theme.fg_primary);
    frame.render_widget(Paragraph::new(lines).style(body_surface), editor_area);
    hitmap.add(editor_area, HitTarget::InlineEditorBody);

    if total_rows > view_h && pane_area.width >= 4 {
        let sb_area = Rect::new(
            pane_area.right().saturating_sub(1),
            body_y,
            1,
            body_h,
        );
        let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1))
            .position(offset)
            .viewport_content_length(view_h);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("┃")
            .track_style(Style::default().fg(theme.divider).bg(theme.bg_canvas))
            .thumb_style(Style::default().fg(theme.accent).bg(theme.bg_canvas));
        frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
    }

    // 3. Live terminal cursor position
    if canvas_focused && cursor_row >= offset && cursor_row < offset.saturating_add(view_h) {
        let cy = editor_area
            .y
            .saturating_add((cursor_row - offset) as u16)
            .min(editor_area.bottom().saturating_sub(1));
        let cx = editor_area
            .x
            .saturating_add(cursor_col as u16)
            .min(editor_area.right().saturating_sub(1));
        ui.cursor_pos = Some((cx, cy));
    }
}

fn code_fence_state_before(text: &str, byte_offset: usize) -> bool {
    let prefix = text.get(..byte_offset.min(text.len())).unwrap_or("");
    let mut in_fence = false;
    for line in prefix.split('\n') {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
    }
    // If byte_offset is in the middle of a fence delimiter line itself, do not flip twice.
    if byte_offset < text.len()
        && byte_offset > 0
        && text.as_bytes().get(byte_offset - 1) != Some(&b'\n')
    {
        if let Some(cur_line) = prefix.rsplit('\n').next() {
            if cur_line.trim_start().starts_with("```") {
                in_fence = !in_fence;
            }
        }
    }
    in_fence
}

fn logical_line_bounds(text: &str, pos: usize) -> (usize, usize) {
    let clamped = pos.min(text.len());
    let start = text[..clamped].rfind('\n').map_or(0, |i| i + 1);
    let end = text[clamped..]
        .find('\n')
        .map_or(text.len(), |i| clamped + i);
    (start, end)
}

#[allow(clippy::too_many_arguments)]
fn highlight_markdown_visual_slice(
    text: &str,
    start: usize,
    end: usize,
    logical_start: usize,
    logical_line: &str,
    in_code_fence: bool,
    is_fence_delim: bool,
    selection: Option<(usize, usize)>,
    theme: &Theme,
) -> Line<'static> {
    let slice = text.get(start..end).unwrap_or("");
    if slice.is_empty() {
        return Line::from("");
    }

    let base_default = Style::default().fg(theme.fg_primary).bg(theme.bg_canvas);
    let (prefix_end_byte, prefix_style, body_style, allow_inline_code) = if is_fence_delim {
        let fence_style = Style::default()
            .fg(theme.accent_soft)
            .bg(theme.bg_code)
            .add_modifier(Modifier::BOLD);
        (logical_start + logical_line.len(), fence_style, fence_style, false)
    } else if in_code_fence {
        let code_style = Style::default().fg(theme.code_fg).bg(theme.bg_code);
        (logical_start, code_style, code_style, false)
    } else {
        classify_markdown_line(logical_start, logical_line, theme, base_default)
    };

    // Count backticks before `start` on the same logical line so wrapped inline code keeps styling.
    let mut in_inline_code = false;
    if allow_inline_code && start > logical_start {
        if let Some(prev) = text.get(logical_start..start) {
            for ch in prev.chars() {
                if ch == '`' {
                    in_inline_code = !in_inline_code;
                }
            }
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;

    for (offset, ch) in slice.char_indices() {
        let pos = start + offset;
        let is_tick = allow_inline_code && ch == '`';
        if is_tick && !in_inline_code {
            in_inline_code = true;
        }

        let selected = selection
            .map(|(from, to)| pos >= from && pos < to)
            .unwrap_or(false);
        let style = if selected {
            theme.selection()
        } else if pos < prefix_end_byte {
            prefix_style
        } else if in_inline_code {
            theme.code_style()
        } else {
            body_style
        };

        if is_tick && in_inline_code {
            // Closing backtick ends inline code after styling the backtick itself.
            let before_count = text
                .get(logical_start..=pos)
                .map(|s| s.chars().filter(|&c| c == '`').count())
                .unwrap_or(0);
            if before_count.is_multiple_of(2) {
                in_inline_code = false;
            }
        }

        if run_style != Some(style) {
            if !run.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut run),
                    run_style.unwrap_or(base_default),
                ));
            }
            run_style = Some(style);
        }
        if ch == '\t' {
            run.push_str("    ");
        } else {
            run.push(ch);
        }
    }

    if !run.is_empty() {
        spans.push(Span::styled(run, run_style.unwrap_or(base_default)));
    }
    Line::from(spans)
}

fn classify_markdown_line(
    logical_start: usize,
    logical_line: &str,
    theme: &Theme,
    base_default: Style,
) -> (usize, Style, Style, bool) {
    let lead_ws = logical_line.len() - logical_line.trim_start_matches([' ', '\t']).len();
    let trimmed = &logical_line[lead_ws..];

    if trimmed == "---" || trimmed == "***" || trimmed == "___" {
        let st = Style::default().fg(theme.fg_muted).bg(theme.bg_canvas);
        return (logical_start + logical_line.len(), st, st, false);
    }
    if trimmed.starts_with("### ") {
        let p_end = logical_start + lead_ws + 4;
        let p_st = Style::default()
            .fg(theme.accent)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        let b_st = Style::default()
            .fg(theme.fg_secondary)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        return (p_end, p_st, b_st, true);
    }
    if trimmed.starts_with("## ") {
        let p_end = logical_start + lead_ws + 3;
        let p_st = Style::default()
            .fg(theme.accent)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        let b_st = Style::default()
            .fg(theme.accent_soft)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        return (p_end, p_st, b_st, true);
    }
    if trimmed.starts_with("# ") {
        let p_end = logical_start + lead_ws + 2;
        let p_st = Style::default()
            .fg(theme.accent)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        let b_st = Style::default()
            .fg(theme.fg_primary)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        return (p_end, p_st, b_st, true);
    }
    if trimmed.starts_with("- [x] ")
        || trimmed.starts_with("- [X] ")
        || trimmed.starts_with("* [x] ")
        || trimmed.starts_with("* [X] ")
    {
        let p_end = logical_start + lead_ws + 6;
        let p_st = Style::default()
            .fg(theme.success)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        let b_st = Style::default()
            .fg(theme.fg_muted)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::CROSSED_OUT);
        return (p_end, p_st, b_st, true);
    }
    if trimmed.starts_with("- [ ] ") || trimmed.starts_with("* [ ] ") {
        let p_end = logical_start + lead_ws + 6;
        let p_st = Style::default()
            .fg(theme.warning)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        return (p_end, p_st, base_default, true);
    }
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        let p_end = logical_start + lead_ws + 2;
        let p_st = Style::default()
            .fg(theme.accent)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        return (p_end, p_st, base_default, true);
    }
    if let Some(dot) = trimmed.find(". ") {
        if dot > 0 && dot <= 4 && trimmed[..dot].chars().all(|c| c.is_ascii_digit()) {
            let p_end = logical_start + lead_ws + dot + 2;
            let p_st = Style::default()
                .fg(theme.accent)
                .bg(theme.bg_canvas)
                .add_modifier(Modifier::BOLD);
            return (p_end, p_st, base_default, true);
        }
    }
    if trimmed.starts_with("> ") || trimmed == ">" {
        let p_end = logical_start + lead_ws + trimmed.len().min(2);
        let p_st = Style::default()
            .fg(theme.accent)
            .bg(theme.bg_canvas)
            .add_modifier(Modifier::BOLD);
        let b_st = theme.quote_style().bg(theme.bg_canvas);
        return (p_end, p_st, b_st, true);
    }
    (logical_start, base_default, base_default, true)
}

struct BlockRenderMeta {
    visible_idx: usize,
    block_id: String,
    start_line: usize,
    end_line: usize,
    text_start_line: usize,
    first_prefix_w: u16,
    cont_prefix_w: u16,
    text_width: u16,
    has_leading_toggle: bool,
}

struct ActiveBlockRenderInfo {
    text_start_line: usize,
    prefix_w: u16,
    wrap_w: u16,
    visual_lines: Vec<(usize, usize)>,
    cursor_row: usize,
    cursor_col: usize,
    total_text_rows: u16,
}

struct PageCanvasScene {
    lines: Vec<(Line<'static>, Option<HitTarget>)>,
    inline_hits: Vec<(usize, u16, u16, HitTarget)>,
    selected_line_range: Option<(usize, usize)>,
    block_meta: Vec<BlockRenderMeta>,
    inline_links: Vec<(usize, u16, u16, String)>,
    active_block_edit: Option<ActiveBlockRenderInfo>,
}

fn build_page_canvas_scene(
    app: &AppState,
    detail: &NotionPageDetail,
    ui: &UiState,
    theme: &Theme,
    content_width: u16,
) -> PageCanvasScene {
    let content_w = content_width as usize;
    let visible = app.visible_blocks(ui);
    let selected_idx = ui.selected_block.min(visible.len().saturating_sub(1));
    let canvas_focused = ui.focus == Focus::Canvas;
    let active_bed = ui
        .block_editor
        .as_ref()
        .filter(|b| b.page_id == detail.page.id);

    let mut rendered_lines: Vec<(Line<'static>, Option<HitTarget>)> = Vec::new();
    let mut inline_hits: Vec<(usize, u16, u16, HitTarget)> = Vec::new();
    let mut block_meta: Vec<BlockRenderMeta> = Vec::new();
    let mut inline_links: Vec<(usize, u16, u16, String)> = Vec::new();
    let mut active_block_edit: Option<ActiveBlockRenderInfo> = None;

    // Generous breathing room at top
    rendered_lines.push((Line::from(""), None));

    // Page Header: Large Icon + Bold Title + Favorite Star (wrapped losslessly)
    let fav_mark = if detail.page.is_favorite { " ★" } else { "" };
    let icon_prefix = format!("{}  ", detail.page.display_icon());
    let icon_pad = " ".repeat(icon_prefix.width());
    let title_first = [Span::styled(
        icon_prefix,
        Style::default().fg(theme.fg_primary),
    )];
    let title_cont = [Span::raw(icon_pad)];
    let title_spans = [Span::styled(
        format!("{}{fav_mark}", detail.page.display_title()),
        Style::default()
            .fg(theme.fg_primary)
            .add_modifier(Modifier::BOLD),
    )];
    for line in wrap_styled_spans(
        &title_spans,
        content_w,
        WrapPrefix {
            first: &title_first,
            cont: &title_cont,
        },
    ) {
        rendered_lines.push((line, None));
    }

    // Subtle metadata line (Last Edited + Comments count + Media count), flow-wrapped on narrow viewports
    let edited = detail
        .page
        .last_edited_time
        .as_deref()
        .map(short_timestamp)
        .unwrap_or_else(|| "Synced".into());
    let edited_seg = format!("Edited {edited}");
    let comment_badge = if detail.comments.is_empty() {
        "💬 0".to_string()
    } else {
        format!("💬 {}", detail.comments.len())
    };
    let mut meta_items: Vec<(String, Style, Option<HitTarget>)> = vec![
        (edited_seg, theme.muted_style(), None),
        (
            comment_badge,
            theme.accent_style(),
            Some(HitTarget::Button(ButtonId::ToggleComments)),
        ),
    ];
    if !detail.media.is_empty() {
        meta_items.push((
            format!("🖼 {} media", detail.media.len()),
            theme.muted_style(),
            Some(HitTarget::Button(ButtonId::ImagePreview)),
        ));
    }
    let meta_sep = "  ·  ";
    let meta_sep_w = meta_sep.width() as u16;
    let mut meta_spans: Vec<Span<'static>> = Vec::new();
    let mut mx = 0u16;
    for (label, style, hit) in meta_items {
        let w = label.width() as u16;
        let sep_w = if meta_spans.is_empty() { 0 } else { meta_sep_w };
        if !meta_spans.is_empty() && mx.saturating_add(sep_w).saturating_add(w) > content_width {
            rendered_lines.push((Line::from(std::mem::take(&mut meta_spans)), None));
            mx = 0;
        }
        if !meta_spans.is_empty() {
            meta_spans.push(Span::styled(meta_sep, theme.muted_style()));
            mx = mx.saturating_add(meta_sep_w);
        }
        if w > content_width {
            for wrapped in
                wrap_styled_spans(&[Span::styled(label, style)], content_w, WrapPrefix::empty())
            {
                let l_idx = rendered_lines.len();
                let lw = (wrapped.width() as u16).min(content_width);
                if let Some(target) = hit {
                    if lw > 0 {
                        inline_hits.push((l_idx, 0, lw, target));
                    }
                }
                rendered_lines.push((wrapped, None));
            }
            mx = 0;
        } else {
            let l_idx = rendered_lines.len();
            meta_spans.push(Span::styled(label, style));
            if let Some(target) = hit {
                if w > 0 {
                    inline_hits.push((l_idx, mx, w, target));
                }
            }
            mx = mx.saturating_add(w);
        }
    }
    if !meta_spans.is_empty() {
        rendered_lines.push((Line::from(meta_spans), None));
    }

    // Clickable Page Quick Action Pill Bar (flow-wrapped across lines, preserving exact inline_hits)
    let fav_pill = if detail.page.is_favorite {
        "[★ Fav]"
    } else {
        "[☆ Fav]"
    };
    let href_pill = if ui.show_inline_hrefs {
        "[hrefs:on]"
    } else {
        "[hrefs]"
    };
    let files_pill = if detail.media.is_empty() {
        "[📎 Files]".to_string()
    } else {
        format!("[📎 Files:{}]", detail.media.len())
    };
    let action_pills: [(&str, ButtonId, bool); 9] = [
        (fav_pill, ButtonId::ToggleFavorite, detail.page.is_favorite),
        ("[+ Append]", ButtonId::QuickAppend, false),
        ("[+ Child]", ButtonId::NewChildPage, false),
        ("[✎ Edit]", ButtonId::EditPage, false),
        ("[Src]", ButtonId::ToggleEditorMode, false),
        ("[↗ Web]", ButtonId::OpenWeb, false),
        ("[🔗 Links]", ButtonId::LinkExtractor, false),
        (href_pill, ButtonId::ToggleHrefs, ui.show_inline_hrefs),
        (&files_pill, ButtonId::Attachments, false),
    ];
    let mut action_spans: Vec<Span<'static>> = Vec::new();
    let mut ax = 0u16;
    for (label, btn, active) in action_pills {
        let w = label.width() as u16;
        let sep_w = if action_spans.is_empty() { 0 } else { 1 };
        if !action_spans.is_empty() && ax.saturating_add(sep_w).saturating_add(w) > content_width {
            rendered_lines.push((Line::from(std::mem::take(&mut action_spans)), None));
            ax = 0;
        }
        let style = if active {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_card)
        };
        if !action_spans.is_empty() {
            action_spans.push(Span::raw(" "));
            ax = ax.saturating_add(1);
        }
        if w > content_width {
            for wrapped in wrap_styled_spans(
                &[Span::styled(label.to_string(), style)],
                content_w,
                WrapPrefix::empty(),
            ) {
                let l_idx = rendered_lines.len();
                let lw = (wrapped.width() as u16).min(content_width);
                if lw > 0 {
                    inline_hits.push((l_idx, 0, lw, HitTarget::Button(btn)));
                }
                rendered_lines.push((wrapped, None));
            }
            ax = 0;
        } else {
            let action_line_idx = rendered_lines.len();
            action_spans.push(Span::styled(label.to_string(), style));
            inline_hits.push((action_line_idx, ax, w, HitTarget::Button(btn)));
            ax = ax.saturating_add(w);
        }
    }
    if !action_spans.is_empty() {
        rendered_lines.push((Line::from(action_spans), None));
    }

    // Properties Strip (for database rows or pages with properties)
    push_properties_strip(detail, content_w, theme, &mut rendered_lines);

    // Subtle hairline divider
    let rule = "─".repeat(content_width.min(96) as usize);
    rendered_lines.push((
        Line::from(Span::styled(rule, Style::default().fg(theme.divider))),
        None,
    ));
    rendered_lines.push((Line::from(""), None));

    // Interactive Blocks
    let mut selected_line_range: Option<(usize, usize)> = None;
    if visible.is_empty() {
        let notice = [Span::styled(
            "Empty page. Press `a` to append blocks (/h1, /todo, /callout, /code) or `e` to edit.",
            theme.muted_style().add_modifier(Modifier::ITALIC),
        )];
        for line in wrap_styled_spans(&notice, content_w, WrapPrefix::empty()) {
            rendered_lines.push((line, None));
        }
    } else {
        for (idx, vblk) in visible.iter().enumerate() {
            let is_active_edit = active_bed.is_some_and(|b| b.block_id == vblk.block.id);
            let is_sel = canvas_focused && (is_active_edit || (active_bed.is_none() && idx == selected_idx));
            let start_line = rendered_lines.len();
            if let Some(bed) = active_bed.filter(|_| is_active_edit) {
                let info = render_active_block_editor_lines(
                    bed,
                    vblk,
                    idx,
                    content_w,
                    theme,
                    &mut rendered_lines,
                );
                let end_line = rendered_lines.len().saturating_sub(1);
                let has_leading_toggle = matches!(
                    bed.kind,
                    NotionBlockKind::ToDo { .. }
                        | NotionBlockKind::Toggle { .. }
                        | NotionBlockKind::Heading1 { is_toggleable: true, .. }
                        | NotionBlockKind::Heading2 { is_toggleable: true, .. }
                        | NotionBlockKind::Heading3 { is_toggleable: true, .. }
                );
                if rendered_lines.len() > start_line {
                    block_meta.push(BlockRenderMeta {
                        visible_idx: idx,
                        block_id: vblk.block.id.clone(),
                        start_line,
                        end_line,
                        text_start_line: info.text_start_line,
                        first_prefix_w: info.prefix_w,
                        cont_prefix_w: info.prefix_w,
                        text_width: info.wrap_w,
                        has_leading_toggle,
                    });
                    selected_line_range = Some((start_line, end_line));
                }
                active_block_edit = Some(info);
            } else {
                render_block_lines(
                    vblk,
                    idx,
                    is_sel,
                    content_w,
                    ui,
                    theme,
                    &mut rendered_lines,
                    &mut inline_hits,
                );
                let end_line = rendered_lines.len().saturating_sub(1);
                if rendered_lines.len() > start_line {
                    let (first_prefix_w, cont_prefix_w, has_leading_toggle, text_line_offset) =
                        rendered_block_prefix_metrics(vblk, content_w);
                    let text_start_line = start_line.saturating_add(text_line_offset).min(end_line);
                    let text_width = (content_w.saturating_sub(first_prefix_w as usize)).max(1) as u16;
                    block_meta.push(BlockRenderMeta {
                        visible_idx: idx,
                        block_id: vblk.block.id.clone(),
                        start_line,
                        end_line,
                        text_start_line,
                        first_prefix_w,
                        cont_prefix_w,
                        text_width,
                        has_leading_toggle,
                    });
                    collect_block_inline_link_hits(
                        vblk,
                        start_line,
                        end_line,
                        theme,
                        &rendered_lines,
                        &mut inline_links,
                    );
                }
                if active_bed.is_none() && idx == selected_idx && rendered_lines.len() > start_line {
                    selected_line_range = Some((start_line, end_line));
                }
            }
        }
    }

    PageCanvasScene {
        lines: rendered_lines,
        inline_hits,
        selected_line_range,
        block_meta,
        inline_links,
        active_block_edit,
    }
}

/// Renders the currently focused Notion block IN PLACE with live soft-wrapping, exact prefix
/// alignment, and active cursor tracking while all sibling blocks stay rendered around it.
fn render_active_block_editor_lines(
    bed: &InlineBlockEditor,
    vblk: &VisibleBlock,
    idx: usize,
    max_width: usize,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
) -> ActiveBlockRenderInfo {
    let indent = "  ".repeat(vblk.depth);
    let edit_bg = theme.bg_selected;
    let edit_gutter = Span::styled(
        "▎ ",
        Style::default()
            .fg(theme.accent)
            .bg(edit_bg)
            .add_modifier(Modifier::BOLD),
    );
    let indent_span = Span::styled(indent.clone(), Style::default().bg(edit_bg));

    let (first_prefix, cont_prefix, body_style, hit, is_heading1, code_lang): (
        Vec<Span<'static>>,
        Vec<Span<'static>>,
        Style,
        HitTarget,
        bool,
        Option<String>,
    ) = match &bed.kind {
        NotionBlockKind::Heading1 { is_toggleable, .. } => {
            let p = if *is_toggleable {
                if vblk.is_toggle_open { "▾ " } else { "▸ " }
            } else {
                ""
            };
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(p.to_string(), Style::default().fg(theme.accent).bg(edit_bg)),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(" ".repeat(p.width()), Style::default().bg(edit_bg)),
            ];
            let hit = if *is_toggleable {
                HitTarget::ToggleBlock(idx)
            } else {
                HitTarget::CanvasBlock(idx)
            };
            (
                first,
                cont,
                Style::default()
                    .fg(theme.fg_primary)
                    .bg(edit_bg)
                    .add_modifier(Modifier::BOLD),
                hit,
                true,
                None,
            )
        }
        NotionBlockKind::Heading2 { is_toggleable, .. } => {
            let p = if *is_toggleable {
                if vblk.is_toggle_open { "▾ " } else { "▸ " }
            } else {
                ""
            };
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(p.to_string(), Style::default().fg(theme.accent).bg(edit_bg)),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(" ".repeat(p.width()), Style::default().bg(edit_bg)),
            ];
            let hit = if *is_toggleable {
                HitTarget::ToggleBlock(idx)
            } else {
                HitTarget::CanvasBlock(idx)
            };
            (
                first,
                cont,
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(edit_bg)
                    .add_modifier(Modifier::BOLD),
                hit,
                false,
                None,
            )
        }
        NotionBlockKind::Heading3 { is_toggleable, .. } => {
            let p = if *is_toggleable {
                if vblk.is_toggle_open { "▾ " } else { "▸ " }
            } else {
                ""
            };
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(p.to_string(), Style::default().fg(theme.accent).bg(edit_bg)),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(" ".repeat(p.width()), Style::default().bg(edit_bg)),
            ];
            let hit = if *is_toggleable {
                HitTarget::ToggleBlock(idx)
            } else {
                HitTarget::CanvasBlock(idx)
            };
            (
                first,
                cont,
                Style::default()
                    .fg(theme.fg_secondary)
                    .bg(edit_bg)
                    .add_modifier(Modifier::BOLD),
                hit,
                false,
                None,
            )
        }
        NotionBlockKind::BulletedListItem { .. } => {
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled("• ", theme.accent_style().bg(edit_bg)),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled("  ", Style::default().bg(edit_bg)),
            ];
            (
                first,
                cont,
                Style::default().fg(theme.fg_primary).bg(edit_bg),
                HitTarget::CanvasBlock(idx),
                false,
                None,
            )
        }
        NotionBlockKind::NumberedListItem { .. } => {
            let num = vblk.numbered_index.max(1);
            let marker = format!("{num}. ");
            let pad = " ".repeat(marker.width());
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(marker, theme.accent_style().bg(edit_bg)),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(pad, Style::default().bg(edit_bg)),
            ];
            (
                first,
                cont,
                Style::default().fg(theme.fg_primary).bg(edit_bg),
                HitTarget::CanvasBlock(idx),
                false,
                None,
            )
        }
        NotionBlockKind::ToDo { checked, .. } => {
            let (glyph, gstyle, bstyle) = if *checked {
                (
                    "[✓] ",
                    Style::default()
                        .fg(theme.success)
                        .bg(edit_bg)
                        .add_modifier(Modifier::BOLD),
                    Style::default()
                        .fg(theme.fg_muted)
                        .bg(edit_bg)
                        .add_modifier(Modifier::CROSSED_OUT),
                )
            } else {
                (
                    "[ ] ",
                    Style::default().fg(theme.fg_muted).bg(edit_bg),
                    Style::default().fg(theme.fg_primary).bg(edit_bg),
                )
            };
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(glyph, gstyle),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(" ".repeat(glyph.width()), Style::default().bg(edit_bg)),
            ];
            (first, cont, bstyle, HitTarget::TodoToggle(idx), false, None)
        }
        NotionBlockKind::Toggle { .. } => {
            let arrow = if vblk.is_toggle_open { "▾ " } else { "▸ " };
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled(
                    arrow,
                    theme.accent_style().bg(edit_bg).add_modifier(Modifier::BOLD),
                ),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                Span::styled("  ", Style::default().bg(edit_bg)),
            ];
            (
                first,
                cont,
                Style::default()
                    .fg(theme.fg_primary)
                    .bg(edit_bg)
                    .add_modifier(Modifier::BOLD),
                HitTarget::ToggleBlock(idx),
                false,
                None,
            )
        }
        NotionBlockKind::Callout { icon, color, .. } => {
            let ic = icon.as_deref().unwrap_or("💡");
            let accent_col = if *color == NotionColor::Default {
                theme.accent
            } else {
                theme.notion_text_color(*color)
            };
            let bar = Span::styled("┃ ", Style::default().fg(accent_col).bg(edit_bg));
            let icon_seg = format!("{ic}  ");
            let icon_pad = " ".repeat(icon_seg.width());
            let first = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                bar.clone(),
                Span::styled(icon_seg, Style::default().bg(edit_bg)),
            ];
            let cont = vec![
                edit_gutter.clone(),
                indent_span.clone(),
                bar,
                Span::styled(icon_pad, Style::default().bg(edit_bg)),
            ];
            (
                first,
                cont,
                Style::default().fg(theme.fg_primary).bg(edit_bg),
                HitTarget::CanvasBlock(idx),
                false,
                None,
            )
        }
        NotionBlockKind::Quote { .. } => {
            let bar = Span::styled("│ ", Style::default().fg(theme.fg_muted).bg(edit_bg));
            let prefix = vec![edit_gutter.clone(), indent_span.clone(), bar];
            (
                prefix.clone(),
                prefix,
                theme.quote_style().bg(edit_bg),
                HitTarget::CanvasBlock(idx),
                false,
                None,
            )
        }
        NotionBlockKind::Code { language, .. } => {
            let bar = Span::styled("│ ", Style::default().fg(theme.code_fg).bg(edit_bg));
            let prefix = vec![edit_gutter.clone(), indent_span.clone(), bar];
            (
                prefix.clone(),
                prefix,
                Style::default().fg(theme.code_fg).bg(edit_bg),
                HitTarget::CanvasBlock(idx),
                false,
                Some(language.clone()),
            )
        }
        _ => {
            let prefix = vec![edit_gutter.clone(), indent_span.clone()];
            (
                prefix.clone(),
                prefix,
                Style::default().fg(theme.fg_primary).bg(edit_bg),
                HitTarget::CanvasBlock(idx),
                false,
                None,
            )
        }
    };

    if let Some(ref lang) = code_lang {
        let badge = format!("  {indent}╭─ {lang} (editing · Shift+Enter/Enter newline) ");
        out.push((
            Line::from(Span::styled(
                truncate_width(&badge, max_width),
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(edit_bg)
                    .add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(edit_bg)),
            Some(hit),
        ));
    }

    let prefix_w: usize = first_prefix.iter().map(Span::width).sum();
    let wrap_w = max_width.saturating_sub(prefix_w).max(1);
    let text = bed.composer.text();
    let cursor = bed.composer.cursor();
    let selection = bed.composer.selection_range();
    let (visual, cursor_row, cursor_col) = text_layout(text, wrap_w, cursor);
    let text_start_line = out.len();

    if text.is_empty() {
        let placeholder = if code_lang.is_some() {
            "Type code..."
        } else {
            "Type text, or # / - / [] / > ..."
        };
        let mut spans = first_prefix;
        spans.push(Span::styled(
            truncate_display(placeholder, wrap_w),
            theme
                .muted_style()
                .bg(edit_bg)
                .add_modifier(Modifier::ITALIC),
        ));
        out.push((Line::from(spans).style(Style::default().bg(edit_bg)), Some(hit)));
    } else {
        for (v_idx, &(start, end)) in visual.iter().enumerate() {
            let mut spans = if v_idx == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            let slice = text.get(start..end).unwrap_or("");
            let mut run = String::new();
            let mut run_selected = false;
            for (offset, ch) in slice.char_indices() {
                let pos = start + offset;
                let selected = selection
                    .map(|(from, to)| pos >= from && pos < to)
                    .unwrap_or(false);
                if !run.is_empty() && selected != run_selected {
                    let st = if run_selected {
                        theme.selection().add_modifier(Modifier::REVERSED)
                    } else {
                        body_style
                    };
                    spans.push(Span::styled(std::mem::take(&mut run), st));
                }
                run_selected = selected;
                if ch == '\t' {
                    run.push_str("    ");
                } else {
                    run.push(ch);
                }
            }
            if !run.is_empty() {
                let st = if run_selected {
                    theme.selection().add_modifier(Modifier::REVERSED)
                } else {
                    body_style
                };
                spans.push(Span::styled(run, st));
            }
            out.push((Line::from(spans).style(Style::default().bg(edit_bg)), Some(hit)));
        }
    }

    let total_text_rows = out.len().saturating_sub(text_start_line).max(1) as u16;

    if code_lang.is_some() {
        let footer_prefix_w = 2 + indent.width();
        let footer = if footer_prefix_w + 2 > max_width {
            let lead = " ".repeat(max_width.saturating_sub(2));
            format!("{lead}╰─")
        } else {
            let dashes = max_width.saturating_sub(footer_prefix_w + 1).min(20);
            format!("  {indent}╰{}", "─".repeat(dashes))
        };
        out.push((
            Line::from(Span::styled(footer, Style::default().fg(theme.accent))),
            Some(hit),
        ));
    } else if is_heading1 {
        out.push((Line::from(""), None));
    }

    ActiveBlockRenderInfo {
        text_start_line,
        prefix_w: prefix_w.min(u16::MAX as usize) as u16,
        wrap_w: wrap_w.min(u16::MAX as usize) as u16,
        visual_lines: visual,
        cursor_row,
        cursor_col,
        total_text_rows,
    }
}

/// Computes `(first_prefix_w, cont_prefix_w, has_leading_toggle, text_line_offset)` for a
/// rendered (non-editing) block so mouse clicks map accurately to the clicked text column.
fn rendered_block_prefix_metrics(vblk: &VisibleBlock, max_width: usize) -> (u16, u16, bool, usize) {
    let indent_w = vblk.depth.saturating_mul(2);
    let base = 2usize.saturating_add(indent_w);
    match &vblk.block.kind {
        NotionBlockKind::Heading1 { is_toggleable, .. }
        | NotionBlockKind::Heading2 { is_toggleable, .. }
        | NotionBlockKind::Heading3 { is_toggleable, .. } => {
            let w = base.saturating_add(if *is_toggleable { 2 } else { 0 });
            (w as u16, w as u16, *is_toggleable, 0)
        }
        NotionBlockKind::Paragraph { .. } => (base as u16, base as u16, false, 0),
        NotionBlockKind::BulletedListItem { .. } => {
            let w = base.saturating_add(2);
            (w as u16, w as u16, false, 0)
        }
        NotionBlockKind::NumberedListItem { .. } => {
            let marker = format!("{}. ", vblk.numbered_index.max(1));
            let w = base.saturating_add(marker.width());
            (w as u16, w as u16, false, 0)
        }
        NotionBlockKind::ToDo { .. } => {
            let w = base.saturating_add(4);
            (w as u16, w as u16, true, 0)
        }
        NotionBlockKind::Toggle { .. } => {
            let w = base.saturating_add(2);
            (w as u16, w as u16, true, 0)
        }
        NotionBlockKind::Callout { icon, .. } => {
            let ic = icon.as_deref().unwrap_or("💡");
            let cont = base.saturating_add(2);
            let first = cont.saturating_add(ic.width()).saturating_add(2);
            (first as u16, cont as u16, false, 0)
        }
        NotionBlockKind::Quote { .. } => {
            let w = base.saturating_add(2);
            (w as u16, w as u16, false, 0)
        }
        NotionBlockKind::Code { language, .. } => {
            let is_html = crate::ui::action::is_html_code_language(language);
            let badge = format!("  {}╭─ {language} (y to copy) ", "  ".repeat(vblk.depth));
            let combined_w = badge.width() + if is_html { "[▶ Preview HTML]".width() } else { 0 };
            let header_rows = if combined_w <= max_width {
                1
            } else {
                let badge_rows = badge.width().div_ceil(max_width.max(1));
                badge_rows + usize::from(is_html)
            };
            let w = base.saturating_add(2);
            (w as u16, w as u16, false, header_rows)
        }
        _ => (base as u16, base as u16, false, 0),
    }
}

/// Records clickable screen coordinates for inline rich-text hyperlinks inside rendered blocks.
fn collect_block_inline_link_hits(
    vblk: &VisibleBlock,
    start_line: usize,
    end_line: usize,
    theme: &Theme,
    rendered_lines: &[(Line<'static>, Option<HitTarget>)],
    out_links: &mut Vec<(usize, u16, u16, String)>,
) {
    let rich_text: &[RichTextSpan] = match &vblk.block.kind {
        NotionBlockKind::Paragraph { rich_text, .. }
        | NotionBlockKind::Heading1 { rich_text, .. }
        | NotionBlockKind::Heading2 { rich_text, .. }
        | NotionBlockKind::Heading3 { rich_text, .. }
        | NotionBlockKind::BulletedListItem { rich_text, .. }
        | NotionBlockKind::NumberedListItem { rich_text, .. }
        | NotionBlockKind::ToDo { rich_text, .. }
        | NotionBlockKind::Toggle { rich_text, .. }
        | NotionBlockKind::Callout { rich_text, .. }
        | NotionBlockKind::Quote { rich_text, .. } => rich_text,
        _ => return,
    };
    let link_spans: Vec<(&str, &str)> = rich_text
        .iter()
        .filter_map(|sp| {
            let url = sp.href.as_deref().map(str::trim).filter(|u| !u.is_empty())?;
            let text = sp.plain_text.as_str();
            (!text.is_empty()).then_some((text, url))
        })
        .collect();
    if link_spans.is_empty() {
        return;
    }

    for (l_idx, (line, _)) in rendered_lines
        .iter()
        .enumerate()
        .take(end_line.saturating_add(1))
        .skip(start_line)
    {
        let mut x = 0usize;
        for sp in &line.spans {
            let w = sp.width();
            if w > 0
                && sp.style.fg == Some(theme.accent_soft)
                && sp.style.add_modifier.contains(Modifier::UNDERLINED)
            {
                let content = sp.content.as_ref();
                if let Some((_, url)) = link_spans
                    .iter()
                    .find(|(lt, _)| lt.contains(content) || content.contains(lt))
                {
                    out_links.push((l_idx, x as u16, w as u16, (*url).to_string()));
                }
            }
            x = x.saturating_add(w);
        }
    }
}

fn push_wrapped_block(
    spans: &[Span<'static>],
    max_width: usize,
    first_prefix: &[Span<'static>],
    cont_prefix: &[Span<'static>],
    line_style: Style,
    hit: Option<HitTarget>,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
) {
    for line in wrap_styled_spans(
        spans,
        max_width,
        WrapPrefix {
            first: first_prefix,
            cont: cont_prefix,
        },
    ) {
        out.push((line.style(line_style), hit));
    }
}

fn push_properties_strip(
    detail: &NotionPageDetail,
    max_width: usize,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
) {
    let mut prop_spans: Vec<Span<'static>> = Vec::new();
    let mut first_prop = true;
    for (name, val) in &detail.page.properties {
        if matches!(val, NotionPropertyValue::Title(_)) || val.is_empty() {
            continue;
        }
        if !first_prop {
            prop_spans.push(Span::raw("   "));
        }
        first_prop = false;
        prop_spans.push(Span::styled(format!("{name}: "), theme.muted_style()));
        match val {
            NotionPropertyValue::Status(Some(opt)) | NotionPropertyValue::Select(Some(opt)) => {
                prop_spans.push(Span::styled(
                    format!(" {} ", opt.name),
                    theme.notion_pill_style(opt.color).add_modifier(Modifier::BOLD),
                ));
            }
            NotionPropertyValue::MultiSelect(opts) => {
                for (i, opt) in opts.iter().enumerate() {
                    if i > 0 {
                        prop_spans.push(Span::raw(" "));
                    }
                    prop_spans.push(Span::styled(
                        format!(" {} ", opt.name),
                        theme.notion_pill_style(opt.color),
                    ));
                }
            }
            NotionPropertyValue::Checkbox(b) => {
                prop_spans.push(Span::styled(
                    if *b { "☑ Yes" } else { "☐ No" },
                    Style::default().fg(if *b { theme.success } else { theme.fg_muted }),
                ));
            }
            _ => {
                prop_spans.push(Span::styled(
                    val.to_plain_string(),
                    Style::default().fg(theme.fg_secondary),
                ));
            }
        }
    }

    if !prop_spans.is_empty() {
        out.push((Line::from(""), None));
        for line in wrap_styled_spans(&prop_spans, max_width, WrapPrefix::empty()) {
            out.push((line, None));
        }
    }
}

/// Muted suffix for a toggle or toggleable heading: its child count while collapsed, or, when its
/// children are not loaded yet, a loading / expandable cue instead of blank space.
fn toggle_children_hint(vblk: &VisibleBlock, ui: &UiState) -> Option<String> {
    let blk = &vblk.block;
    if !blk.children.is_empty() {
        return (!vblk.is_toggle_open).then(|| format!("  ({} items)", blk.children.len()));
    }
    if !blk.has_children {
        return None;
    }
    Some(if ui.hydrating_blocks.contains(&blk.id) {
        "  (loading...)".to_string()
    } else if vblk.is_toggle_open {
        "  (not loaded · toggle to retry)".to_string()
    } else {
        "  (expand to load)".to_string()
    })
}

/// Database title suffix shown when the database has more rows than were read: the rows the
/// latest read actually fetched and the row limit it ran with (background poll vs explicit refresh).
fn database_truncation_banner(fetched_rows: usize) -> String {
    let limit = if fetched_rows > crate::sync::TUI_DATABASE_ROW_LIMIT {
        crate::sync::EXPLICIT_DATABASE_ROW_LIMIT
    } else {
        crate::sync::TUI_DATABASE_ROW_LIMIT
    };
    format!("   {fetched_rows}+ rows · showing first {fetched_rows} · {limit}-row limit")
}

#[allow(clippy::too_many_arguments)]
fn render_block_lines(
    vblk: &VisibleBlock,
    idx: usize,
    selected: bool,
    max_width: usize,
    ui: &UiState,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
    inline_hits: &mut Vec<(usize, u16, u16, HitTarget)>,
) {
    let indent = "  ".repeat(vblk.depth);
    let sel_bg = if selected {
        theme.selection()
    } else {
        Style::default().bg(theme.bg_canvas)
    };
    let cursor_gutter = if selected {
        Span::styled("▎ ", Style::default().fg(theme.accent).bg(theme.bg_selected))
    } else {
        Span::styled("  ", Style::default().bg(theme.bg_canvas))
    };

    match &vblk.block.kind {
        NotionBlockKind::Heading1 {
            rich_text,
            is_toggleable,
            ..
        } => {
            let prefix = if *is_toggleable {
                if vblk.is_toggle_open { "▾ " } else { "▸ " }
            } else {
                ""
            };
            let first = [
                cursor_gutter.clone(),
                Span::raw(format!("{indent}{prefix}")),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(format!("{indent}{}", " ".repeat(prefix.width()))),
            ];
            let mut body = style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            );
            let hit = if *is_toggleable {
                if let Some(hint) = toggle_children_hint(vblk, ui) {
                    body.push(Span::styled(hint, theme.muted_style()));
                }
                HitTarget::ToggleBlock(idx)
            } else {
                HitTarget::CanvasBlock(idx)
            };
            push_wrapped_block(&body, max_width, &first, &cont, sel_bg, Some(hit), out);
            out.push((Line::from(""), None));
        }
        NotionBlockKind::Heading2 {
            rich_text,
            is_toggleable,
            ..
        } => {
            let prefix = if *is_toggleable {
                if vblk.is_toggle_open { "▾ " } else { "▸ " }
            } else {
                ""
            };
            let first = [
                cursor_gutter.clone(),
                Span::raw(format!("{indent}{prefix}")),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(format!("{indent}{}", " ".repeat(prefix.width()))),
            ];
            let mut body = style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            );
            let hit = if *is_toggleable {
                if let Some(hint) = toggle_children_hint(vblk, ui) {
                    body.push(Span::styled(hint, theme.muted_style()));
                }
                HitTarget::ToggleBlock(idx)
            } else {
                HitTarget::CanvasBlock(idx)
            };
            push_wrapped_block(&body, max_width, &first, &cont, sel_bg, Some(hit), out);
        }
        NotionBlockKind::Heading3 {
            rich_text,
            is_toggleable,
            ..
        } => {
            let prefix = if *is_toggleable {
                if vblk.is_toggle_open { "▾ " } else { "▸ " }
            } else {
                ""
            };
            let first = [
                cursor_gutter.clone(),
                Span::raw(format!("{indent}{prefix}")),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(format!("{indent}{}", " ".repeat(prefix.width()))),
            ];
            let mut body = style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.fg_secondary)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            );
            let hit = if *is_toggleable {
                if let Some(hint) = toggle_children_hint(vblk, ui) {
                    body.push(Span::styled(hint, theme.muted_style()));
                }
                HitTarget::ToggleBlock(idx)
            } else {
                HitTarget::CanvasBlock(idx)
            };
            push_wrapped_block(&body, max_width, &first, &cont, sel_bg, Some(hit), out);
        }
        NotionBlockKind::Paragraph { rich_text, .. } => {
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary),
                theme,
                ui.show_inline_hrefs,
            );
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::BulletedListItem { rich_text, .. } => {
            let marker = "• ";
            let first = [
                cursor_gutter.clone(),
                Span::raw(indent.clone()),
                Span::styled(marker, theme.accent_style()),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(format!("{indent}{}", " ".repeat(marker.width()))),
            ];
            let body = style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary),
                theme,
                ui.show_inline_hrefs,
            );
            push_wrapped_block(
                &body,
                max_width,
                &first,
                &cont,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::NumberedListItem { rich_text, .. } => {
            let num = vblk.numbered_index.max(1);
            let marker = format!("{num}. ");
            let marker_pad = " ".repeat(marker.width());
            let first = [
                cursor_gutter.clone(),
                Span::raw(indent.clone()),
                Span::styled(marker, theme.accent_style()),
            ];
            let cont = [cursor_gutter, Span::raw(format!("{indent}{marker_pad}"))];
            let body = style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary),
                theme,
                ui.show_inline_hrefs,
            );
            push_wrapped_block(
                &body,
                max_width,
                &first,
                &cont,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::ToDo {
            rich_text, checked, ..
        } => {
            let (box_glyph, box_style, text_base) = if *checked {
                (
                    "[✓] ",
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                    Style::default()
                        .fg(theme.fg_muted)
                        .add_modifier(Modifier::CROSSED_OUT),
                )
            } else {
                (
                    "[ ] ",
                    Style::default().fg(theme.fg_muted),
                    Style::default().fg(theme.fg_primary),
                )
            };
            let first = [
                cursor_gutter.clone(),
                Span::raw(indent.clone()),
                Span::styled(box_glyph, box_style),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(format!("{indent}{}", " ".repeat(box_glyph.width()))),
            ];
            let body = style_rich_text(rich_text, text_base, theme, ui.show_inline_hrefs);
            push_wrapped_block(
                &body,
                max_width,
                &first,
                &cont,
                sel_bg,
                Some(HitTarget::TodoToggle(idx)),
                out,
            );
        }
        NotionBlockKind::Toggle { rich_text, .. } => {
            let arrow = if vblk.is_toggle_open { "▾ " } else { "▸ " };
            let first = [
                cursor_gutter.clone(),
                Span::raw(indent.clone()),
                Span::styled(arrow, theme.accent_style().add_modifier(Modifier::BOLD)),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(format!("{indent}{}", " ".repeat(arrow.width()))),
            ];
            let mut body = style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            );
            if let Some(hint) = toggle_children_hint(vblk, ui) {
                body.push(Span::styled(hint, theme.muted_style()));
            }
            push_wrapped_block(
                &body,
                max_width,
                &first,
                &cont,
                sel_bg,
                Some(HitTarget::ToggleBlock(idx)),
                out,
            );
        }
        NotionBlockKind::Callout {
            rich_text,
            icon,
            color,
        } => {
            let ic = icon.as_deref().unwrap_or("💡");
            let accent_col = if *color == NotionColor::Default {
                theme.accent
            } else {
                theme.notion_text_color(*color)
            };
            let card_bg = if selected {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let bar = Span::styled("┃ ", Style::default().fg(accent_col).bg(card_bg));
            let first = [
                cursor_gutter.clone(),
                Span::raw(indent.clone()),
                bar.clone(),
                Span::styled(format!("{ic}  "), Style::default().bg(card_bg)),
            ];
            let cont = [cursor_gutter, Span::raw(indent), bar];
            let body = style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary).bg(card_bg),
                theme,
                ui.show_inline_hrefs,
            );
            push_wrapped_block(
                &body,
                max_width,
                &first,
                &cont,
                Style::default().bg(card_bg),
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::Quote { rich_text, .. } => {
            let bar = Span::styled("│ ", Style::default().fg(theme.fg_muted));
            let prefix = [cursor_gutter, Span::raw(indent), bar];
            let body = style_rich_text(
                rich_text,
                theme.quote_style(),
                theme,
                ui.show_inline_hrefs,
            );
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::Divider => {
            let max_indent_w = max_width.saturating_sub(3);
            let clamped_indent = if indent.width() > max_indent_w {
                " ".repeat(max_indent_w)
            } else {
                indent
            };
            let avail = max_width.saturating_sub(2 + clamped_indent.width());
            let w = avail.saturating_sub(2).max(1).min(avail);
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(clamped_indent),
                    Span::styled("─".repeat(w), Style::default().fg(theme.divider)),
                ])
                .style(sel_bg),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
        NotionBlockKind::Code {
            rich_text,
            language,
            ..
        } => {
            let code_bg = if selected {
                theme.bg_selected
            } else {
                theme.bg_code
            };
            let is_html = crate::ui::action::is_html_code_language(language);
            let badge = format!("  {indent}╭─ {language} (y to copy) ");
            let badge_style = Style::default()
                .fg(theme.fg_muted)
                .bg(code_bg)
                .add_modifier(Modifier::BOLD);
            let preview_pill = "[▶ Preview HTML]";
            let preview_style = Style::default()
                .fg(theme.bg_canvas)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD);
            let combined_w = badge.width() + if is_html { preview_pill.width() } else { 0 };
            if combined_w <= max_width {
                let header_line_idx = out.len();
                let mut header_spans = vec![Span::styled(badge.clone(), badge_style)];
                if is_html {
                    let pill_x = badge.width() as u16;
                    let pill_w = preview_pill.width() as u16;
                    header_spans.push(Span::styled(preview_pill, preview_style));
                    inline_hits.push((
                        header_line_idx,
                        pill_x,
                        pill_w,
                        HitTarget::CodeHtmlPreview(idx),
                    ));
                }
                out.push((
                    Line::from(header_spans).style(Style::default().bg(code_bg)),
                    Some(HitTarget::CanvasBlock(idx)),
                ));
            } else {
                for line in wrap_styled_spans(
                    &[Span::styled(badge, badge_style)],
                    max_width,
                    WrapPrefix::empty(),
                ) {
                    out.push((
                        line.style(Style::default().bg(code_bg)),
                        Some(HitTarget::CanvasBlock(idx)),
                    ));
                }
                if is_html {
                    let pill_prefix = [Span::styled(
                        format!("  {indent}"),
                        Style::default().bg(code_bg),
                    )];
                    for line in wrap_styled_spans(
                        &[Span::styled(preview_pill, preview_style)],
                        max_width,
                        WrapPrefix {
                            first: &pill_prefix,
                            cont: &pill_prefix,
                        },
                    ) {
                        let line_idx = out.len();
                        let lead_w = (2 + indent.width()).min(max_width) as u16;
                        let pill_w = (line.width() as u16).saturating_sub(lead_w);
                        if pill_w > 0 {
                            inline_hits.push((
                                line_idx,
                                lead_w,
                                pill_w,
                                HitTarget::CodeHtmlPreview(idx),
                            ));
                        }
                        out.push((
                            line.style(Style::default().bg(code_bg)),
                            Some(HitTarget::CanvasBlock(idx)),
                        ));
                    }
                }
            }
            let raw = spans_plain_text(rich_text);
            let trimmed_raw = raw
                .strip_suffix("\r\n")
                .or_else(|| raw.strip_suffix('\n'))
                .unwrap_or(&raw);
            if !trimmed_raw.is_empty() {
                let code_prefix = [
                    cursor_gutter.clone(),
                    Span::raw(indent.clone()),
                    Span::styled("│ ", Style::default().fg(theme.code_fg).bg(code_bg)),
                ];
                let code_body = [Span::styled(
                    trimmed_raw.to_string(),
                    Style::default().fg(theme.code_fg).bg(code_bg),
                )];
                push_wrapped_block(
                    &code_body,
                    max_width,
                    &code_prefix,
                    &code_prefix,
                    Style::default().bg(code_bg),
                    Some(HitTarget::CanvasBlock(idx)),
                    out,
                );
            }
            let footer_prefix_w = 2 + indent.width();
            let footer = if footer_prefix_w + 2 > max_width {
                let lead = " ".repeat(max_width.saturating_sub(2));
                format!("{lead}╰─")
            } else {
                let dashes = max_width.saturating_sub(footer_prefix_w + 1).min(20);
                format!("  {indent}╰{}", "─".repeat(dashes))
            };
            out.push((
                Line::from(Span::styled(footer, Style::default().fg(theme.border))),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
        NotionBlockKind::Table { .. } => {
            render_unicode_table(vblk, idx, selected, max_width, ui.show_inline_hrefs, theme, out);
        }
        NotionBlockKind::Image { url, caption, .. } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() {
                "Image".to_string()
            } else {
                cap
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let mut body = vec![Span::styled(
                format!(" 🖼 [IMG: {label}] (press v to preview) "),
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD),
            )];
            if ui.show_inline_hrefs {
                body.push(Span::styled(format!("  {url}"), theme.muted_style()));
            }
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::MediaChip(idx)),
                out,
            );
        }
        NotionBlockKind::Video { url, caption, .. } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() {
                url.as_str()
            } else {
                cap.as_str()
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let mut body = vec![Span::styled(
                format!(" 🎬 [VIDEO: {label}] (Enter: open) "),
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD),
            )];
            if ui.show_inline_hrefs && !cap.is_empty() {
                body.push(Span::styled(format!("  {url}"), theme.muted_style()));
            }
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::MediaChip(idx)),
                out,
            );
        }
        NotionBlockKind::Audio { url, caption, .. } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() {
                url.as_str()
            } else {
                cap.as_str()
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(
                format!(" 🎵 [AUDIO: {label}] (Enter: open) "),
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD),
            )];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::MediaChip(idx)),
                out,
            );
        }
        NotionBlockKind::ChildPage { title } => {
            let card_bg = if selected {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(
                format!(" 📄 {title}  → "),
                Style::default()
                    .fg(theme.fg_primary)
                    .bg(card_bg)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                Style::default().bg(card_bg),
                Some(HitTarget::ChildCard(idx)),
                out,
            );
        }
        NotionBlockKind::ChildDatabase { title } => {
            let card_bg = if selected {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(
                format!(" 🗂 {title}  [Database →] "),
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(card_bg)
                    .add_modifier(Modifier::BOLD),
            )];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                Style::default().bg(card_bg),
                Some(HitTarget::ChildCard(idx)),
                out,
            );
        }
        NotionBlockKind::Bookmark { url, caption }
        | NotionBlockKind::Embed { url, caption } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() { url.as_str() } else { cap.as_str() };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(
                format!(" 🔗 {label} "),
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::UNDERLINED),
            )];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::ChildCard(idx)),
                out,
            );
        }
        NotionBlockKind::LinkPreview { url } => {
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(
                format!(" 🔗 {url}"),
                Style::default().fg(theme.accent_soft),
            )];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::ChildCard(idx)),
                out,
            );
        }
        NotionBlockKind::File { url, name, .. } => {
            let label = name.as_deref().unwrap_or(url.as_str());
            let chip = if crate::ui::action::is_html_file_ref(url, name.as_deref()) {
                Span::styled(
                    format!(" 🌐 [HTML] {label} (Enter/v: preview) "),
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.bg_card)
                        .add_modifier(Modifier::BOLD),
                )
            } else if crate::ui::action::is_markdown_file_ref(url, name.as_deref()) {
                Span::styled(
                    format!(" 📝 [MD] {label} (Enter/v: preview) "),
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.bg_card)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    format!(" 📎 {label} "),
                    Style::default().fg(theme.fg_secondary).bg(theme.bg_card),
                )
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            push_wrapped_block(
                &[chip],
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::MediaChip(idx)),
                out,
            );
        }
        NotionBlockKind::Pdf { url, caption } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() { url.as_str() } else { cap.as_str() };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(
                format!(" 📄 [PDF] {label} "),
                Style::default().fg(theme.warning).bg(theme.bg_card),
            )];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::MediaChip(idx)),
                out,
            );
        }
        NotionBlockKind::Equation { expression } => {
            let badge_style = math_badge_style(theme);
            let expr_style = math_style(theme);
            let first = [
                cursor_gutter.clone(),
                Span::raw(indent.clone()),
                Span::styled(" ∑ ", badge_style),
            ];
            let cont = [
                cursor_gutter,
                Span::raw(indent),
                Span::styled(" │ ", badge_style),
            ];
            let expr_text = if expression.is_empty() {
                " ".to_string()
            } else {
                expression
                    .lines()
                    .map(|l| format!(" {l} "))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            let body = [Span::styled(expr_text, expr_style)];
            push_wrapped_block(
                &body,
                max_width,
                &first,
                &cont,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::TableOfContents { .. } => {
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled("[Table of Contents]", theme.muted_style())];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
        NotionBlockKind::Button { rich_text } => {
            let raw_label = spans_plain_text(rich_text);
            let label = if raw_label.trim().is_empty() {
                "Button"
            } else {
                raw_label.trim()
            };
            let pill_style = if selected {
                Style::default()
                    .fg(theme.bg_canvas)
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.accent_soft)
                    .bg(theme.bg_card)
                    .add_modifier(Modifier::BOLD)
            };
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(format!("[ ⚡ {label} ]"), pill_style)];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::NotionButton(idx)),
                out,
            );
        }
        NotionBlockKind::ColumnList
        | NotionBlockKind::Column
        | NotionBlockKind::SyncedBlock { .. }
        | NotionBlockKind::TableRow { .. } => {}
        NotionBlockKind::Unsupported { raw_type } => {
            let prefix = [cursor_gutter, Span::raw(indent)];
            let body = [Span::styled(format!("[{raw_type}]"), theme.muted_style())];
            push_wrapped_block(
                &body,
                max_width,
                &prefix,
                &prefix,
                sel_bg,
                Some(HitTarget::CanvasBlock(idx)),
                out,
            );
        }
    }
}

fn render_unicode_table(
    vblk: &VisibleBlock,
    idx: usize,
    selected: bool,
    max_width: usize,
    show_hrefs: bool,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
) {
    let header_style = Style::default()
        .fg(theme.fg_primary)
        .add_modifier(Modifier::BOLD);
    let cell_style = Style::default().fg(theme.fg_secondary);

    let mut raw_rows: Vec<&Vec<Vec<RichTextSpan>>> = Vec::new();
    for child in &vblk.block.children {
        if let NotionBlockKind::TableRow { cells } = &child.kind {
            raw_rows.push(cells);
        }
    }
    if raw_rows.is_empty() {
        return;
    }

    let border_style = if selected {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.border)
    };
    let cursor_gutter = if selected {
        Span::styled("▎ ", Style::default().fg(theme.accent).bg(theme.bg_selected))
    } else {
        Span::styled("  ", Style::default().bg(theme.bg_canvas))
    };
    let indent = "  ".repeat(vblk.depth);
    let mut prefix = vec![cursor_gutter];
    if !indent.is_empty() {
        prefix.push(Span::raw(indent));
    }

    let (headers, body_rows, eff_cell_style) = if raw_rows.len() > 1 {
        let hdr: Vec<Vec<Span<'static>>> = raw_rows[0]
            .iter()
            .map(|c| style_rich_text(c, header_style, theme, show_hrefs))
            .collect();
        let body: Vec<Vec<Vec<Span<'static>>>> = raw_rows[1..]
            .iter()
            .map(|r| {
                r.iter()
                    .map(|c| style_rich_text(c, cell_style, theme, show_hrefs))
                    .collect()
            })
            .collect();
        (hdr, body, cell_style)
    } else {
        let single: Vec<Vec<Vec<Span<'static>>>> = vec![raw_rows[0]
            .iter()
            .map(|c| style_rich_text(c, header_style, theme, show_hrefs))
            .collect()];
        (Vec::new(), single, header_style)
    };

    let spec = TableSpec {
        headers: &headers,
        rows: &body_rows,
        alignments: &[],
        border: TableBorder::Box,
        prefix: &prefix,
        border_style,
        header_style,
        cell_style: eff_cell_style,
    };

    for line in render_wrapped_table(&spec, max_width) {
        out.push((line, Some(HitTarget::CanvasBlock(idx))));
    }
}

fn math_style(theme: &Theme) -> Style {
    if theme.no_color {
        Style::default().add_modifier(Modifier::ITALIC | Modifier::BOLD)
    } else if theme.ansi16_only {
        Style::default()
            .fg(Color::Magenta)
            .bg(theme.bg_code)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default()
            .fg(theme.notion_text_color(NotionColor::Purple))
            .bg(theme.bg_code)
            .add_modifier(Modifier::ITALIC)
    }
}

fn math_badge_style(theme: &Theme) -> Style {
    if theme.no_color {
        Style::default().add_modifier(Modifier::BOLD)
    } else if theme.ansi16_only {
        Style::default()
            .fg(Color::LightMagenta)
            .bg(theme.bg_code)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.accent)
            .bg(theme.bg_code)
            .add_modifier(Modifier::BOLD)
    }
}

fn style_rich_text(
    spans: &[RichTextSpan],
    base: Style,
    theme: &Theme,
    show_hrefs: bool,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for sp in spans {
        if let RichTextKind::Equation { expression } = &sp.kind {
            let expr = if expression.is_empty() {
                sp.plain_text.as_str()
            } else {
                expression.as_str()
            };
            let mut st = math_style(theme);
            if sp.annotations.bold {
                st = st.add_modifier(Modifier::BOLD);
            }
            if sp.annotations.underline {
                st = st.add_modifier(Modifier::UNDERLINED);
            }
            if sp.annotations.strikethrough {
                st = st.add_modifier(Modifier::CROSSED_OUT);
            }
            if sp.annotations.color != NotionColor::Default {
                if sp.annotations.is_background {
                    st = theme.notion_pill_style(sp.annotations.color);
                } else {
                    st = st.fg(theme.notion_text_color(sp.annotations.color));
                }
            }
            out.push(Span::styled(format!(" {expr} "), st));
            continue;
        }
        let mut st = base;
        if sp.annotations.bold {
            st = st.add_modifier(Modifier::BOLD);
        }
        if sp.annotations.italic {
            st = st.add_modifier(Modifier::ITALIC);
        }
        if sp.annotations.underline {
            st = st.add_modifier(Modifier::UNDERLINED);
        }
        if sp.annotations.strikethrough {
            st = st.add_modifier(Modifier::CROSSED_OUT);
        }
        if sp.annotations.code {
            st = theme.code_style();
        }
        if sp.annotations.color != NotionColor::Default {
            if sp.annotations.is_background {
                st = theme.notion_pill_style(sp.annotations.color);
            } else {
                st = st.fg(theme.notion_text_color(sp.annotations.color));
            }
        }
        if sp.href.is_some() {
            st = st.fg(theme.accent_soft).add_modifier(Modifier::UNDERLINED);
        }
        out.push(Span::styled(sp.plain_text.clone(), st));
        if show_hrefs {
            if let Some(ref href) = sp.href {
                out.push(Span::styled(format!(" [{href}]"), theme.muted_style()));
            }
        }
    }
    out
}

fn rich_text_to_indented_lines(
    spans: &[RichTextSpan],
    indent: &str,
    base: Style,
    theme: &Theme,
    show_hrefs: bool,
) -> Vec<Line<'static>> {
    let styled = style_rich_text(spans, base, theme, show_hrefs);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = vec![Span::raw(indent.to_string())];
    for sp in styled {
        let content = sp.content.into_owned();
        let st = sp.style;
        let mut parts = content.split('\n');
        if let Some(first) = parts.next() {
            if !first.is_empty() {
                cur.push(Span::styled(first.to_string(), st));
            }
        }
        for next in parts {
            lines.push(Line::from(std::mem::take(&mut cur)));
            cur.push(Span::raw(indent.to_string()));
            if !next.is_empty() {
                cur.push(Span::styled(next.to_string(), st));
            }
        }
    }
    lines.push(Line::from(cur));
    lines
}

fn draw_comments_drawer(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    hitmap.add(area, HitTarget::Pane(Focus::Comments));

    let theme = ui.theme.clone();
    let focused = ui.focus == Focus::Comments;
    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_sidebar).fg(theme.fg_primary)),
        area,
    );

    if area.width < 10 || area.height < 5 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(3),
        ])
        .split(area);

    let title_label = " 💬 Comments ";
    let close_label = "(C to close)";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                title_label,
                theme.title(focused).add_modifier(Modifier::BOLD),
            ),
            Span::styled(close_label, theme.muted_style()),
        ]))
        .style(Style::default().bg(theme.bg_sidebar)),
        chunks[0],
    );
    let close_x = chunks[0].x.saturating_add(title_label.width() as u16);
    let close_w = (close_label.width() as u16).min(chunks[0].right().saturating_sub(close_x));
    if close_w > 0 {
        hitmap.add_line(
            close_x,
            chunks[0].y,
            close_w,
            HitTarget::Button(ButtonId::ToggleComments),
        );
    }

    let comments = app
        .active_page_detail
        .as_ref()
        .map(|d| d.comments.as_slice())
        .unwrap_or(&[]);
    let comments_unavailable = app.active_page_detail.as_ref()
        .is_some_and(|detail| detail.comments_error.is_some());

    if comments.is_empty() {
        ui.comments_scroll = 0;
        frame.render_widget(
            Paragraph::new(Span::styled(
                if comments_unavailable {
                    " Could not load comments."
                } else {
                    " No comments yet.\n Type below and press Enter."
                },
                theme.muted_style(),
            ))
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(theme.bg_sidebar)),
            chunks[1],
        );
    } else {
        let mut lines = Vec::new();
        if comments_unavailable {
            lines.push(Line::from(Span::styled(" Could not refresh cached comments.", theme.muted_style())));
        }
        for c in comments {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", c.author),
                    Style::default()
                        .fg(theme.accent_soft)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(short_timestamp(&c.created_time), theme.muted_style()),
            ]));
            let body_base = Style::default().fg(theme.fg_primary);
            if !c.rich_text.is_empty() {
                lines.extend(rich_text_to_indented_lines(
                    &c.rich_text,
                    "  ",
                    body_base,
                    &theme,
                    ui.show_inline_hrefs,
                ));
            } else {
                for body_line in c.body.split('\n') {
                    lines.push(Line::from(Span::styled(
                        format!("  {body_line}"),
                        body_base,
                    )));
                }
            }
            lines.push(Line::from(""));
        }
        let wrap_w = chunks[1].width.max(1) as usize;
        let visual_lines: usize = lines
            .iter()
            .map(|l| {
                let w = l.width();
                if w == 0 {
                    1
                } else {
                    w.div_ceil(wrap_w)
                }
            })
            .sum();
        let max_scroll = visual_lines.saturating_sub(chunks[1].height as usize);
        ui.comments_scroll = (ui.comments_scroll as usize).min(max_scroll) as u16;
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((ui.comments_scroll, 0))
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(theme.bg_sidebar)),
            chunks[1],
        );
    }

    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme.border_style(focused))
        .title(Span::styled(" Add comment (Enter) ", theme.muted_style()))
        .style(Style::default().bg(theme.bg_composer));
    let prompt_text = if ui.comment_reply_input.is_empty() {
        Line::from(Span::styled("Write a comment...", theme.muted_style()))
    } else {
        Line::from(vec![
            Span::raw(ui.comment_reply_input.clone()),
            Span::styled("▏", theme.accent_style()),
        ])
    };
    frame.render_widget(Paragraph::new(prompt_text).block(input_block), chunks[2]);
}

// ============================================================================
// B. Notion Database View (Table / Board Kanban / List)
// ============================================================================

fn draw_database_view(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    hitmap.add(area, HitTarget::Pane(Focus::Canvas));

    let theme = &ui.theme;
    frame.render_widget(
        Block::default()
            .borders(Borders::NONE)
            .style(Style::default().bg(theme.bg_canvas).fg(theme.fg_primary)),
        area,
    );

    if area.width < 20 || area.height < 6 {
        return;
    }

    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(1),
    );
    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Database title & description
            Constraint::Length(1), // View switcher bar: [1 Table] [2 Board] [3 List] + [f Filter] [s Sort] [n New Item]
            Constraint::Length(1), // Hairline divider
            Constraint::Min(3),    // Active View (Table / Board / List)
        ])
        .split(inner);

    let db = app.active_database();
    let title = db
        .map(|d| format!("{}  {}", d.display_icon(), d.display_title()))
        .unwrap_or_else(|| "🗂  Database".to_string());
    let desc = db.map(|d| d.description.as_str()).unwrap_or("");
    let mut title_line = vec![Span::styled(
        title,
        Style::default()
            .fg(theme.fg_primary)
            .add_modifier(Modifier::BOLD),
    )];
    if app.active_database_rows_truncated {
        title_line.push(Span::styled(
            database_truncation_banner(if app.active_database_rows_fetched > 0 {
                app.active_database_rows_fetched
            } else {
                app.active_database_rows.len()
            }),
            theme.muted_style(),
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(title_line),
            Line::from(Span::styled(desc.to_string(), theme.muted_style())),
        ]),
        rows_layout[0],
    );

    // View switcher & filter/sort/new-item bar
    draw_db_toolbar(frame, rows_layout[1], ui, hitmap);

    let rule = "─".repeat(rows_layout[2].width as usize);
    frame.render_widget(
        Paragraph::new(Span::styled(rule, Style::default().fg(theme.divider))),
        rows_layout[2],
    );

    match ui.db_view_mode {
        DatabaseViewMode::Table => draw_db_table(frame, rows_layout[3], app, db, ui, hitmap),
        DatabaseViewMode::Board => draw_db_board(frame, rows_layout[3], app, ui, hitmap),
        DatabaseViewMode::List => draw_db_list(frame, rows_layout[3], app, ui, hitmap),
    }
}

fn draw_db_toolbar(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let tabs = [
        (" [1 Table] ", DatabaseViewMode::Table),
        (" [2 Board] ", DatabaseViewMode::Board),
        (" [3 List] ", DatabaseViewMode::List),
    ];
    let mut x = area.x;
    for (label, mode) in tabs {
        let active = ui.db_view_mode == mode;
        let style = if active {
            Style::default()
                .bg(theme.bg_selected)
                .fg(theme.accent_soft)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style()
        };
        let w = (label.width() as u16).min(area.right().saturating_sub(x));
        if w == 0 {
            break;
        }
        frame.render_widget(Paragraph::new(Span::styled(label, style)), Rect::new(x, area.y, w, 1));
        hitmap.add_line(x, area.y, w, HitTarget::DbViewTab(mode));
        x = x.saturating_add(w + 1);
    }

    let filter_label = if ui.db_filter.is_empty() {
        " [f Filter] ".to_string()
    } else {
        format!(" [f Filter: {}] ", truncate_width(&ui.db_filter, 14))
    };
    let sort_label = format!(" [s Sort: {}] ", ui.db_sort.label());
    let new_label = " [n New Item] ".to_string();

    for (label, btn, highlight) in [
        (filter_label, ButtonId::DbFilter, !ui.db_filter.is_empty()),
        (sort_label, ButtonId::DbSort, false),
        (new_label, ButtonId::DbNewItem, true),
    ] {
        let w = (label.width() as u16).min(area.right().saturating_sub(x));
        if w == 0 {
            break;
        }
        let style = if highlight {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else {
            theme.muted_style()
        };
        frame.render_widget(Paragraph::new(Span::styled(label, style)), Rect::new(x, area.y, w, 1));
        hitmap.add_line(x, area.y, w, HitTarget::Button(btn));
        x = x.saturating_add(w + 1);
    }
}

fn draw_db_table(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    db: Option<&NotionDatabase>,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let rows = app.filtered_sorted_db_rows(ui);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No items match this view. Press `n` to create a new database item.",
                theme.muted_style(),
            )),
            area,
        );
        return;
    }

    // Choose up to 4 non-title property columns
    let extra_cols: Vec<String> = if let Some(d) = db {
        d.schema
            .values()
            .filter(|s| s.prop_type != "title")
            .take(4)
            .map(|s| s.name.clone())
            .collect()
    } else {
        rows.first()
            .map(|r| {
                r.properties
                    .iter()
                    .filter(|(_, v)| !matches!(v, NotionPropertyValue::Title(_)))
                    .take(4)
                    .map(|(k, _)| k.clone())
                    .collect()
            })
            .unwrap_or_default()
    };

    let title_w = (area.width as usize / 3).clamp(18, 36);
    let col_w = if extra_cols.is_empty() {
        16
    } else {
        let avail_extra =
            (area.width as usize).saturating_sub(title_w + 2 + extra_cols.len() * 3);
        (avail_extra / extra_cols.len()).clamp(6, 22)
    };

    // Header row
    let mut header_spans = vec![Span::styled(
        format!(
            " {} ",
            pad_display_width(&truncate_width("Name", title_w), title_w, TableAlign::Left)
        ),
        theme.muted_style().add_modifier(Modifier::BOLD),
    )];
    for col in &extra_cols {
        header_spans.push(Span::styled(
            format!(
                "│ {} ",
                pad_display_width(&truncate_width(col, col_w), col_w, TableAlign::Left)
            ),
            theme.muted_style().add_modifier(Modifier::BOLD),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(header_spans)).style(Style::default().bg(theme.bg_header)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    let list_h = area.height.saturating_sub(1) as usize;
    let selected = ui.db_selected_row.min(rows.len().saturating_sub(1));
    let offset = crate::ui::nav_pane::selection_offset(selected, list_h, rows.len());
    let focused = ui.focus == Focus::Canvas;

    for (i, row) in rows.iter().skip(offset).take(list_h).enumerate() {
        let idx = offset + i;
        let y = area.y + 1 + i as u16;
        let is_sel = focused && idx == selected;
        let row_bg = if is_sel {
            theme.bg_selected
        } else {
            theme.bg_canvas
        };

        let title_text = format!("{} {}", row.display_icon(), row.display_title());
        let trunc_title = truncate_width(&title_text, title_w);
        let mut spans = vec![Span::styled(
            format!(
                " {} ",
                pad_display_width(&trunc_title, title_w, TableAlign::Left)
            ),
            Style::default()
                .fg(theme.fg_primary)
                .bg(row_bg)
                .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
        )];

        for col in &extra_cols {
            spans.push(Span::styled("│ ", Style::default().fg(theme.divider).bg(row_bg)));
            push_table_cell_spans(row.properties.get(col), col_w, row_bg, theme, &mut spans);
            spans.push(Span::styled(" ", Style::default().bg(row_bg)));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(row_bg)),
            Rect::new(area.x, y, area.width, 1),
        );
        hitmap.add_line(area.x, y, area.width, HitTarget::DbRow(idx));
    }
}

fn push_table_cell_spans(
    val: Option<&NotionPropertyValue>,
    width: usize,
    row_bg: ratatui::style::Color,
    theme: &Theme,
    out: &mut Vec<Span<'static>>,
) {
    let Some(v) = val else {
        out.push(Span::styled(" ".repeat(width), Style::default().bg(row_bg)));
        return;
    };
    match v {
        NotionPropertyValue::Status(Some(opt)) | NotionPropertyValue::Select(Some(opt)) => {
            let label = format!(" {} ", truncate_width(&opt.name, width.saturating_sub(2)));
            let pad = width.saturating_sub(label.width());
            out.push(Span::styled(
                label,
                theme.notion_pill_style(opt.color).add_modifier(Modifier::BOLD),
            ));
            if pad > 0 {
                out.push(Span::styled(" ".repeat(pad), Style::default().bg(row_bg)));
            }
        }
        NotionPropertyValue::MultiSelect(opts) if !opts.is_empty() => {
            let first = &opts[0];
            let extra = if opts.len() > 1 {
                format!(" +{}", opts.len() - 1)
            } else {
                String::new()
            };
            let label = format!(
                " {} ",
                truncate_width(&first.name, width.saturating_sub(extra.len() + 2))
            );
            let used = label.width() + extra.len();
            let pad = width.saturating_sub(used);
            out.push(Span::styled(label, theme.notion_pill_style(first.color)));
            if !extra.is_empty() {
                out.push(Span::styled(extra, Style::default().fg(theme.fg_muted).bg(row_bg)));
            }
            if pad > 0 {
                out.push(Span::styled(" ".repeat(pad), Style::default().bg(row_bg)));
            }
        }
        NotionPropertyValue::Checkbox(b) => {
            let glyph = if *b { "☑" } else { "☐" };
            let pad = width.saturating_sub(glyph.width());
            out.push(Span::styled(
                format!("{glyph}{}", " ".repeat(pad)),
                Style::default()
                    .fg(if *b { theme.success } else { theme.fg_muted })
                    .bg(row_bg),
            ));
        }
        _ => {
            let txt = truncate_width(&v.to_plain_string(), width);
            out.push(Span::styled(
                pad_display_width(&txt, width, TableAlign::Left),
                Style::default().fg(theme.fg_secondary).bg(row_bg),
            ));
        }
    }
}

fn draw_db_board(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let cols = app.build_board_columns(ui);
    if cols.is_empty() {
        return;
    }

    let active_col = ui.db_board_col.min(cols.len().saturating_sub(1));
    let visible_cols = ((area.width as usize) / 24).clamp(1, cols.len().min(5));
    let start_col = active_col
        .saturating_sub(visible_cols.saturating_sub(1))
        .min(cols.len().saturating_sub(visible_cols));
    let end_col = (start_col + visible_cols).min(cols.len());
    let slice = &cols[start_col..end_col];
    let constraints: Vec<Constraint> = slice
        .iter()
        .map(|_| Constraint::Ratio(1, slice.len() as u32))
        .collect();
    let rects = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let focused = ui.focus == Focus::Canvas;

    for (rel_idx, (col, col_rect)) in slice.iter().zip(rects.iter()).enumerate() {
        let abs_col = start_col + rel_idx;
        let is_active_col = focused && abs_col == active_col;
        let inner = Rect::new(
            col_rect.x,
            col_rect.y,
            col_rect.width.saturating_sub(1).max(1),
            col_rect.height,
        );
        if inner.height < 2 {
            continue;
        }

        // Register column container hit target before cards so hovering headers or empty space
        // below cards targets this column for mouse wheel scrolling and right-click menus.
        hitmap.add(inner, HitTarget::DbBoardColumn(abs_col));

        let cards_area_h = inner.height.saturating_sub(2) as usize;
        let visible_cards = if cards_area_h < 3 {
            0
        } else {
            (cards_area_h - 3) / 4 + 1
        };
        let active_row = ui.db_board_row.min(col.cards.len().saturating_sub(1));
        let row_offset = if abs_col == active_col && visible_cards > 0 {
            crate::ui::nav_pane::selection_offset(active_row, visible_cards, col.cards.len())
        } else {
            0
        };

        // Pill header + card count (with scroll cue when windowed)
        let scroll_cue = if visible_cards > 0 && col.cards.len() > visible_cards {
            let up = if row_offset > 0 { "↑" } else { "" };
            let down = if row_offset + visible_cards < col.cards.len() { "↓" } else { "" };
            format!(" {up}{down}")
        } else {
            String::new()
        };
        let pill_text = format!(
            " {} ",
            truncate_width(&col.label, inner.width.saturating_sub(10) as usize)
        );
        let count_text = format!("  {}{scroll_cue}", col.cards.len());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    pill_text,
                    theme.notion_pill_style(col.color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(count_text, theme.muted_style()),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let mut y = inner.y + 2;
        for (row_idx, card) in col
            .cards
            .iter()
            .enumerate()
            .skip(row_offset)
            .take(visible_cards)
        {
            if y + 3 > inner.bottom() {
                break;
            }
            let card_rect = Rect::new(inner.x, y, inner.width, 3);
            let is_sel = is_active_col && row_idx == active_row;
            let card_bg = if is_sel {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            let card_block = Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(theme.border_style(is_sel))
                .style(Style::default().bg(card_bg));
            let card_title = format!(
                "{} {}",
                card.display_icon(),
                truncate_width(card.display_title(), inner.width.saturating_sub(5) as usize)
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    card_title,
                    Style::default()
                        .fg(theme.fg_primary)
                        .bg(card_bg)
                        .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
                ))
                .block(card_block),
                card_rect,
            );
            hitmap.add(
                card_rect,
                HitTarget::DbBoardCard {
                    col: abs_col,
                    row: row_idx,
                },
            );
            y += 4;
        }
    }
}

fn draw_db_list(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
    let rows = app.filtered_sorted_db_rows(ui);
    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "No items in list. Press `n` to add a new item.",
                theme.muted_style(),
            )),
            area,
        );
        return;
    }

    let list_h = area.height as usize;
    let selected = ui.db_selected_row.min(rows.len().saturating_sub(1));
    let offset = crate::ui::nav_pane::selection_offset(selected, list_h, rows.len());
    let focused = ui.focus == Focus::Canvas;

    for (i, row) in rows.iter().skip(offset).take(list_h).enumerate() {
        let idx = offset + i;
        let y = area.y + i as u16;
        let is_sel = focused && idx == selected;
        let bg = if is_sel {
            theme.bg_selected
        } else {
            theme.bg_canvas
        };

        // Find primary pill for right-aligned badge
        let pill = row.properties.values().find_map(|v| match v {
            NotionPropertyValue::Status(Some(o)) | NotionPropertyValue::Select(Some(o)) => {
                Some((o.name.clone(), o.color))
            }
            _ => None,
        });

        let right_w = pill.as_ref().map(|(n, _)| n.width() + 4).unwrap_or(0);
        let left_budget = (area.width as usize).saturating_sub(right_w + 4).max(10);
        let left = format!(
            " {} {}",
            row.display_icon(),
            truncate_width(row.display_title(), left_budget)
        );
        let pad = (area.width as usize).saturating_sub(left.width() + right_w);

        let mut spans = vec![
            Span::styled(
                left,
                Style::default()
                    .fg(theme.fg_primary)
                    .bg(bg)
                    .add_modifier(if is_sel { Modifier::BOLD } else { Modifier::empty() }),
            ),
            Span::styled(" ".repeat(pad), Style::default().bg(bg)),
        ];
        if let Some((pname, pcol)) = pill {
            spans.push(Span::styled(
                format!(" {pname} "),
                theme.notion_pill_style(pcol),
            ));
        }

        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(bg)),
            Rect::new(area.x, y, area.width, 1),
        );
        hitmap.add_line(area.x, y, area.width, HitTarget::DbRow(idx));
    }
}

fn draw_empty_page_welcome(frame: &mut Frame, area: Rect, theme: &Theme) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "📄  Welcome to @bitefoundry/notion",
            Style::default()
                .fg(theme.fg_primary)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Select a page or database from the Workspace sidebar, or press:",
            theme.muted_style(),
        )),
        Line::from("  • Ctrl+K / Ctrl+P  — Quick Find & Command Palette"),
        Line::from("  • Ctrl+S           — Switch Notion Workspace"),
        Line::from("  • Ctrl+R           — Sync Workspace from Notion API"),
        Line::from("  • ?                — View all keyboard shortcuts"),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn short_timestamp(ts: &str) -> String {
    if ts.len() >= 16 {
        ts[..16].replace('T', " ")
    } else {
        ts.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ratatui::{backend::TestBackend, Terminal};

    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        models::{
            NotionBlock, NotionPage, NotionPropertySchema, RichTextAnnotations, SelectOption,
        },
        notion::NotionClient,
        storage::Storage,
        sync::{FocusTarget, SyncController},
        ui,
    };

    fn make_test_app_and_ui() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_track_d_test_token");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, cmd_tx);

        let mut ui_state = UiState::from_config(&cfg);
        ui_state.show_sidebar = false;
        ui_state.focus = Focus::Canvas;
        let app = AppState::new(cfg, auth, client, storage, sync_ctrl, ui_tx);
        (app, ui_state)
    }

    /// Strips decorative wrapping chrome (box borders, quote/callout/code rails, gutter,
    /// and horizontal rules) plus whitespace, and reassembles multi-line box-table columns
    /// vertically per section so Centered and Full views can be compared sequence-for-sequence
    /// with zero character loss or reordering tolerance.
    fn normalize_visible_content(lines: &[String]) -> String {
        let strip_chrome = |s: &str, out: &mut String| {
            for ch in s.chars() {
                if ch.is_whitespace()
                    || matches!(
                        ch,
                        '│' | '┃' | '─' | '┌' | '┬' | '┐' | '├' | '┼' | '┤' | '└' | '┴' | '┘'
                            | '╭' | '╰' | '▎'
                    )
                {
                    continue;
                }
                out.push(ch);
            }
        };

        let flush_table_section = |cols: &mut Vec<String>, out: &mut String| {
            for col in cols.drain(..) {
                out.push_str(&col);
            }
        };

        let mut out = String::new();
        let mut in_box_table = false;
        let mut table_cols: Vec<String> = Vec::new();

        for line in lines {
            let trimmed = line.trim();
            if trimmed.contains('┌') && trimmed.contains('┐') {
                in_box_table = true;
                flush_table_section(&mut table_cols, &mut out);
                continue;
            }
            if in_box_table {
                if trimmed.contains('├') && trimmed.contains('┤') {
                    flush_table_section(&mut table_cols, &mut out);
                    continue;
                }
                if trimmed.contains('└') && trimmed.contains('┘') {
                    flush_table_section(&mut table_cols, &mut out);
                    in_box_table = false;
                    continue;
                }
                if let (Some(first), Some(last)) = (line.find('│'), line.rfind('│')) {
                    if last > first {
                        let inner = &line[first + '│'.len_utf8()..last];
                        for (c_idx, cell) in inner.split('│').enumerate() {
                            if c_idx >= table_cols.len() {
                                table_cols.resize_with(c_idx + 1, String::new);
                            }
                            strip_chrome(cell, &mut table_cols[c_idx]);
                        }
                        continue;
                    }
                }
            }
            strip_chrome(line, &mut out);
        }
        flush_table_section(&mut table_cols, &mut out);
        out
    }

    fn line_to_string(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn render_all_canvas_buffer_rows(
        app: &mut AppState,
        ui: &mut UiState,
        width: u16,
        height: u16,
        expected_content_x: u16,
        expected_content_w: u16,
    ) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let view_h = height.saturating_sub(2) as usize; // top breadcrumb + bottom status
        let detail = app.active_page_detail.as_ref().unwrap();
        let all_lines =
            build_page_canvas_scene(app, detail, ui, &ui.theme.clone(), expected_content_w).lines;
        let total = all_lines.len();

        let mut collected: Vec<String> = Vec::with_capacity(total);
        ui.canvas_follow_selected = false;
        let mut scroll = 0usize;
        while collected.len() < total {
            ui.canvas_scroll = scroll as u16;
            terminal
                .draw(|frame| {
                    ui::draw(frame, app, ui);
                })
                .unwrap();

            let actual_scroll = ui.canvas_scroll as usize;
            let buf = terminal.backend().buffer();
            for row_offset in 0..view_h {
                let line_idx = actual_scroll + row_offset;
                if line_idx < collected.len() || line_idx >= total {
                    continue;
                }
                let y = 1 + row_offset as u16;
                let mut row_str = String::new();
                let mut x = expected_content_x;
                let end_x = expected_content_x + expected_content_w;
                while x < end_x {
                    let sym = buf[(x, y)].symbol();
                    row_str.push_str(sym);
                    x = x.saturating_add(sym.width().max(1) as u16);
                }
                collected.push(row_str);
            }

            if collected.len() >= total {
                break;
            }
            let next_scroll = collected.len();
            if next_scroll <= scroll {
                break;
            }
            scroll = next_scroll;
        }
        collected
    }

    #[test]
    fn test_centered_and_full_width_modes_lossless_parity_and_shift_enter() {
        let (mut app, mut ui) = make_test_app_and_ui();
        app.active_view = CanvasView::Page;
        app.active_page_id = Some("page-track-d".into());

        // 6 diverse properties (Korean + ASCII + pills + checkbox)
        let mut properties = BTreeMap::new();
        properties.insert(
            "상태".into(),
            NotionPropertyValue::Status(Some(SelectOption {
                id: Some("st-1".into()),
                name: "진행중".into(),
                color: NotionColor::Blue,
            })),
        );
        properties.insert(
            "담당자".into(),
            NotionPropertyValue::People(vec!["김민수".into(), "이서연".into()]),
        );
        properties.insert(
            "우선순위".into(),
            NotionPropertyValue::Select(Some(SelectOption {
                id: Some("pr-1".into()),
                name: "긴급 P0".into(),
                color: NotionColor::Red,
            })),
        );
        properties.insert(
            "태그".into(),
            NotionPropertyValue::MultiSelect(vec![
                SelectOption {
                    id: Some("t-1".into()),
                    name: "레이아웃".into(),
                    color: NotionColor::Purple,
                },
                SelectOption {
                    id: Some("t-2".into()),
                    name: "무손실래핑".into(),
                    color: NotionColor::Green,
                },
            ]),
        );
        properties.insert("검토완료".into(), NotionPropertyValue::Checkbox(true));
        properties.insert(
            "목표일".into(),
            NotionPropertyValue::Date {
                start: Some("2026-11-25".into()),
                end: Some("2026-12-01".into()),
            },
        );

        // 80+ char Korean paragraph
        let korean_paragraph = "중앙정렬 팔십팔 컬럼 모드와 전체 폭 백오십육 컬럼 모드를 전환하더라도 한글 문단과 영문 식별자 그리고 강조 스타일이 단 한 글자도 조용히 잘리지 않고 온전히 줄바꿈되어 모두 표시되어야 합니다.";
        assert!(korean_paragraph.chars().count() >= 80);

        // 3-line Shift+Enter callout
        let callout_l1 = "콜아웃첫줄: Shift+Enter 첫 번째 안내 문구입니다.";
        let callout_l2 = "콜아웃둘째줄: 명시적 개행이 별도 행으로 보존되어야 합니다.";
        let callout_l3 = "콜아웃셋째줄: 마지막 줄까지 배경과 세로 막대가 유지됩니다.";

        // Long quote
        let quote_text = "좋은 터미널 레이아웃은 화면 폭이 좁아져도 정보를 조용히 버리지 않고 읽기 쉬운 행잉 인덴트로 자연스럽게 이어서 보여줍니다.";

        // Numbered list items
        let numbered_1 = "첫 번째 번호 항목으로 중앙 정렬 모드에서 두 줄 이상으로 깔끔하게 줄바꿈되어 번호 폭만큼 행잉 인덴트를 유지합니다.";
        let numbered_2 = "두 번째 번호 항목은 한글과 EnglishTokenWithoutBreaks_2026_TrackD_Verification 을 함께 포함합니다.";

        // 4-column Korean table (cells wider than the old 28-col hard cap + embedded Shift+Enter)
        let mk_cell = |s: &str| vec![RichTextSpan::plain(s)];
        let table_block = NotionBlock {
            id: "blk-table".into(),
            page_id: "page-track-d".into(),
            kind: NotionBlockKind::Table {
                table_width: 4,
                has_column_header: true,
                has_row_header: false,
            },
            has_children: true,
            children: vec![
                NotionBlock {
                    id: "tr-0".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::TableRow {
                        cells: vec![
                            mk_cell("항목명"),
                            mk_cell("담당자"),
                            mk_cell("진행상태"),
                            mk_cell("상세검증설명"),
                        ],
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "tr-1".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::TableRow {
                        cells: vec![
                            mk_cell("스타일래핑"),
                            mk_cell("김민수"),
                            mk_cell("진행중"),
                            mk_cell("기존 스물여덟 컬럼 상한을 제거하고 긴 한글 설명도 여러 줄로 무손실 표시합니다\n개행셀보존확인"),
                        ],
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "tr-2".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::TableRow {
                        cells: vec![
                            mk_cell("표정렬검증"),
                            mk_cell("이서연"),
                            mk_cell("완료"),
                            mk_cell("한글과 ASCII 폭을 unicode_width 로 계산해 세로 경계선이 정확히 일치합니다"),
                        ],
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        // 120+ char code block with tab indentation
        let code_line = "\tlet result = bitefoundry_html::layout::wrap_styled_spans(&styled_spans, content_width, WrapPrefix { first: &first_prefix, cont: &cont_prefix }); // end_of_120_char_code_marker";
        assert!(code_line.len() >= 120);

        app.active_page_detail = Some(NotionPageDetail {
            page: NotionPage {
                id: "page-track-d".into(),
                title: "트랙 D 무손실 래핑 검증 페이지 — Centered 88 vs Full 156 Parity Document Title".into(),
                icon: Some("📘".into()),
                is_favorite: true,
                properties,
                last_edited_time: Some("2026-11-25T16:30:00Z".into()),
                ..Default::default()
            },
            blocks: vec![
                NotionBlock {
                    id: "blk-p".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::Paragraph {
                        rich_text: vec![
                            RichTextSpan::plain(korean_paragraph),
                            RichTextSpan {
                                plain_text: " [강조키워드]".into(),
                                annotations: RichTextAnnotations {
                                    bold: true,
                                    color: NotionColor::Blue,
                                    ..Default::default()
                                },
                                ..Default::default()
                            },
                        ],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-callout".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::Callout {
                        rich_text: vec![RichTextSpan::plain(format!(
                            "{callout_l1}\n{callout_l2}\n{callout_l3}"
                        ))],
                        icon: Some("💡".into()),
                        color: NotionColor::Yellow,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-quote".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::Quote {
                        rich_text: vec![RichTextSpan::plain(quote_text)],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-num-1".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::NumberedListItem {
                        rich_text: vec![RichTextSpan::plain(numbered_1)],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "blk-num-2".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::NumberedListItem {
                        rich_text: vec![RichTextSpan::plain(numbered_2)],
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                table_block,
                NotionBlock {
                    id: "blk-code".into(),
                    page_id: "page-track-d".into(),
                    kind: NotionBlockKind::Code {
                        rich_text: vec![RichTextSpan::plain(code_line)],
                        language: "rust".into(),
                        caption: Vec::new(),
                    },
                    ..Default::default()
                },
            ],
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: Vec::new(),
        });

        let detail = app.active_page_detail.as_ref().unwrap();
        let theme = ui.theme.clone();

        // 1. Direct line-level width invariants at Centered (88) and Full (156)
        let centered_lines =
            build_page_canvas_scene(&app, detail, &ui, &theme, EDITORIAL_MAX_COLS).lines;
        let full_lines = build_page_canvas_scene(&app, detail, &ui, &theme, 156).lines;

        for (i, (line, _)) in centered_lines.iter().enumerate() {
            assert!(
                line.width() <= 88,
                "Centered line {i} width {} exceeded 88: {:?}",
                line.width(),
                line_to_string(line)
            );
        }
        for (i, (line, _)) in full_lines.iter().enumerate() {
            assert!(
                line.width() <= 156,
                "Full line {i} width {} exceeded 156: {:?}",
                line.width(),
                line_to_string(line)
            );
        }
        assert!(
            centered_lines.len() > full_lines.len(),
            "Centered (88) should wrap into more lines than Full (156)"
        );

        let centered_line_strs: Vec<String> =
            centered_lines.iter().map(|(l, _)| line_to_string(l)).collect();
        let full_line_strs: Vec<String> =
            full_lines.iter().map(|(l, _)| line_to_string(l)).collect();
        assert_eq!(
            normalize_visible_content(&centered_line_strs),
            normalize_visible_content(&full_line_strs),
            "Direct rendered lines must have 100% character parity between Centered (88) and Full (156)"
        );

        // Verify Callout continuation lines retain `┃ ` and card_bg
        let callout_l2_line = centered_lines
            .iter()
            .find(|(l, _)| line_to_string(l).contains("콜아웃둘째줄"))
            .expect("callout Shift+Enter line 2 present");
        assert!(
            line_to_string(&callout_l2_line.0).contains("┃ "),
            "callout continuation line must keep ┃ rail"
        );
        assert_eq!(
            callout_l2_line.0.style.bg,
            Some(theme.bg_card),
            "callout continuation line must keep card_bg"
        );

        // 2. End-to-end TestBackend::new(160, 40) rendering in Centered (88) vs Full (156)
        ui.page_width = PageWidthMode::Centered;
        let centered_buf_rows =
            render_all_canvas_buffer_rows(&mut app, &mut ui, 160, 40, (160 - 88) / 2, 88);

        ui.page_width = PageWidthMode::Full;
        let full_buf_rows = render_all_canvas_buffer_rows(&mut app, &mut ui, 160, 40, 2, 156);

        assert_eq!(
            normalize_visible_content(&centered_buf_rows),
            normalize_visible_content(&full_buf_rows),
            "TestBackend(160, 40) Centered (88) and Full (156) buffers must have 100% lossless content parity"
        );

        // Verify all 3 Shift+Enter callout lines appear on 3 distinct rows in BOTH modes
        for (mode_name, rows) in [("Centered", &centered_buf_rows), ("Full", &full_buf_rows)] {
            let r1 = rows
                .iter()
                .position(|r| r.contains("콜아웃첫줄"))
                .unwrap_or_else(|| panic!("{mode_name}: missing callout line 1"));
            let r2 = rows
                .iter()
                .position(|r| r.contains("콜아웃둘째줄"))
                .unwrap_or_else(|| panic!("{mode_name}: missing callout line 2"));
            let r3 = rows
                .iter()
                .position(|r| r.contains("콜아웃셋째줄"))
                .unwrap_or_else(|| panic!("{mode_name}: missing callout line 3"));
            assert!(
                r1 < r2 && r2 < r3,
                "{mode_name}: Shift+Enter callout lines must be on distinct ordered rows (got r1={r1}, r2={r2}, r3={r3})"
            );

            // Verify key markers from all content blocks survived in the rendered buffer
            let joined = rows.join("\n");
            assert!(
                joined.contains("end_of_120_char_code_marker"),
                "{mode_name}: 120-char code tail must not be truncated"
            );
            assert!(
                joined.contains("개행셀보존확인"),
                "{mode_name}: table cell embedded newline content must be preserved"
            );
            assert!(
                joined.contains("2026-12-01"),
                "{mode_name}: 6th property value must wrap and stay visible"
            );
            assert!(
                joined.contains("EnglishTokenWithoutBreaks_2026_TrackD_Verification"),
                "{mode_name}: long token in numbered list must stay lossless"
            );
        }

        // 3. Database Table View: Korean header `│` pipes must align exactly with data rows
        let mut schema = BTreeMap::new();
        schema.insert(
            "이름".into(),
            NotionPropertySchema {
                id: "title".into(),
                name: "이름".into(),
                prop_type: "title".into(),
                ..Default::default()
            },
        );
        schema.insert(
            "상태".into(),
            NotionPropertySchema {
                id: "status".into(),
                name: "상태".into(),
                prop_type: "status".into(),
                ..Default::default()
            },
        );
        schema.insert(
            "담당자".into(),
            NotionPropertySchema {
                id: "owner".into(),
                name: "담당자".into(),
                prop_type: "rich_text".into(),
                ..Default::default()
            },
        );
        schema.insert(
            "우선순위".into(),
            NotionPropertySchema {
                id: "priority".into(),
                name: "우선순위".into(),
                prop_type: "select".into(),
                ..Default::default()
            },
        );
        app.databases = vec![NotionDatabase {
            id: "db-kr".into(),
            title: "스프린트 보드".into(),
            schema,
            ..Default::default()
        }];
        app.active_database_id = Some("db-kr".into());
        app.active_view = CanvasView::Database;
        ui.db_view_mode = DatabaseViewMode::Table;

        let make_db_row = |id: &str, title: &str, st: &str, owner: &str, prio: &str| {
            let mut props = BTreeMap::new();
            props.insert(
                "상태".into(),
                NotionPropertyValue::Status(Some(SelectOption {
                    id: None,
                    name: st.into(),
                    color: NotionColor::Blue,
                })),
            );
            props.insert("담당자".into(), NotionPropertyValue::RichText(owner.into()));
            props.insert(
                "우선순위".into(),
                NotionPropertyValue::Select(Some(SelectOption {
                    id: None,
                    name: prio.into(),
                    color: NotionColor::Green,
                })),
            );
            NotionPage {
                id: id.into(),
                title: title.into(),
                icon: Some("📄".into()),
                properties: props,
                ..Default::default()
            }
        };
        app.active_database_rows = vec![
            make_db_row("r1", "한글 페이지 작업", "진행중", "김민수", "높음"),
            make_db_row("r2", "ASCII Task Item", "Done", "Alice", "Low"),
        ];

        let mut db_terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        db_terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let db_buf = db_terminal.backend().buffer();
        let row_pipe_cols = |y: u16| -> Vec<u16> {
            (0..160u16)
                .filter(|&x| db_buf[(x, y)].symbol() == "│")
                .collect()
        };
        let row_text = |y: u16| -> String {
            let mut s = String::new();
            let mut x = 0u16;
            while x < 160 {
                let sym = db_buf[(x, y)].symbol();
                s.push_str(sym);
                x = x.saturating_add(sym.width().max(1) as u16);
            }
            s
        };

        let header_y = (0..40u16)
            .find(|&y| {
                let txt = row_text(y);
                txt.contains("Name") && txt.contains("상태") && txt.contains("담당자")
            })
            .expect("DB table Korean header row rendered");
        let data_y1 = (0..40u16)
            .find(|&y| row_text(y).contains("한글 페이지 작업"))
            .expect("DB table Korean data row 1 rendered");
        let data_y2 = (0..40u16)
            .find(|&y| row_text(y).contains("ASCII Task Item"))
            .expect("DB table ASCII data row 2 rendered");

        let header_pipes = row_pipe_cols(header_y);
        assert_eq!(
            header_pipes.len(),
            3,
            "expected 3 column separator pipes for 3 extra Korean columns"
        );
        assert_eq!(
            header_pipes,
            row_pipe_cols(data_y1),
            "DB Korean header `│` columns must align exactly with Korean data row"
        );
        assert_eq!(
            header_pipes,
            row_pipe_cols(data_y2),
            "DB Korean header `│` columns must align exactly with ASCII data row"
        );
    }

    #[test]
    fn test_narrow_width_wrapping_action_pills_inline_hits_and_all_blocks() {
        let (mut app, mut ui) = make_test_app_and_ui();
        app.active_view = CanvasView::Page;
        app.active_page_id = Some("page-narrow".into());
        ui.show_inline_hrefs = true;
        ui.selected_block = 1;

        app.active_page_detail = Some(NotionPageDetail {
            page: NotionPage {
                id: "page-narrow".into(),
                title: "매우 긴 한글 제목과 Narrow Viewport Wrapping 검증 페이지".into(),
                icon: Some("📄".into()),
                is_favorite: false,
                ..Default::default()
            },
            blocks: vec![
                NotionBlock {
                    id: "b-h1".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::Heading1 {
                        rich_text: vec![RichTextSpan::plain("긴 제목 1단계가 좁은 폭에서 안전하게 줄바꿈되는지 확인")],
                        is_toggleable: true,
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "b-todo".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::ToDo {
                        rich_text: vec![RichTextSpan::plain("체크박스 할 일 항목이 여러 줄로 래핑될 때 모든 줄에 동일한 히트 타깃이 유지되어야 합니다")],
                        checked: false,
                        color: NotionColor::Default,
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "b-html".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::Code {
                        rich_text: vec![RichTextSpan::plain("<div>hello_world_very_long_html_code_block_line_for_narrow_wrap</div>")],
                        language: "html".into(),
                        caption: Vec::new(),
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "b-eq".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::Equation {
                        expression: "E = mc^2 + a_1 x_1 + a_2 x_2 + a_3 x_3 + a_4 x_4".into(),
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "b-file".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::File {
                        url: "https://example.com/very-long-report-preview.md".into(),
                        name: Some("매우긴마크다운파일이름미리보기.md".into()),
                        caption: Vec::new(),
                    },
                    ..Default::default()
                },
                NotionBlock {
                    id: "b-div".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::Divider,
                    ..Default::default()
                },
                NotionBlock {
                    id: "b-btn".into(),
                    page_id: "page-narrow".into(),
                    kind: NotionBlockKind::Button {
                        rich_text: vec![RichTextSpan::plain("긴 자동화 실행 버튼 라벨 테스트")],
                    },
                    ..Default::default()
                },
            ],
            comments: Vec::new(),
            comments_error: None,
            markdown: String::new(),
            media: Vec::new(),
        });

        let detail = app.active_page_detail.as_ref().unwrap();
        let theme = ui.theme.clone();
        let narrow_w = 36u16;
        let scene = build_page_canvas_scene(&app, detail, &ui, &theme, narrow_w);
        let (lines, inline_hits, selected_range) =
            (scene.lines, scene.inline_hits, scene.selected_line_range);

        // Width invariant across every single line
        for (i, (line, _)) in lines.iter().enumerate() {
            assert!(
                line.width() <= narrow_w as usize,
                "Narrow line {i} width {} exceeded {narrow_w}: {:?}",
                line.width(),
                line_to_string(line)
            );
        }

        // All 8 action pills must be flow-wrapped across multiple lines with exact inline_hits
        let expected_buttons = [
            ButtonId::ToggleFavorite,
            ButtonId::QuickAppend,
            ButtonId::NewChildPage,
            ButtonId::EditPage,
            ButtonId::OpenWeb,
            ButtonId::LinkExtractor,
            ButtonId::ToggleHrefs,
            ButtonId::Attachments,
        ];
        let mut pill_rows = std::collections::BTreeSet::new();
        for btn in expected_buttons {
            let hit = inline_hits
                .iter()
                .find(|&&(_, _, _, target)| target == HitTarget::Button(btn))
                .unwrap_or_else(|| panic!("missing wrapped action pill hit for {btn:?}"));
            pill_rows.insert(hit.0);
            assert!(
                hit.1.saturating_add(hit.2) <= narrow_w,
                "action pill {btn:?} hit {:?} exceeds width {narrow_w}",
                hit
            );
        }
        assert!(
            pill_rows.len() >= 2,
            "action pills should wrap across multiple rows at width {narrow_w}, got rows {pill_rows:?}"
        );

        // Selected multi-line ToDo block must span multiple lines with identical TodoToggle(1) hit
        let (sel_start, sel_end) = selected_range.expect("selected_line_range present");
        assert!(
            sel_end > sel_start,
            "wrapped ToDo block should span multiple lines (start={sel_start}, end={sel_end})"
        );
        for line in &lines[sel_start..=sel_end] {
            assert_eq!(
                line.1,
                Some(HitTarget::TodoToggle(1)),
                "every wrapped row of selected ToDo block must carry TodoToggle(1)"
            );
        }
    }

    #[test]
    fn toggle_hints_show_child_count_and_unloaded_children_cue() {
        use crate::models::{NotionColor, RichTextSpan};
        let mut ui = UiState::new();
        let heading = |children: Vec<NotionBlock>, has_children: bool| NotionBlock {
            id: "blk-h".into(),
            block_type: "heading_2".into(),
            has_children,
            kind: NotionBlockKind::Heading2 {
                rich_text: vec![RichTextSpan::plain("Section")],
                is_toggleable: true,
                color: NotionColor::Default,
            },
            children,
            ..Default::default()
        };
        let vb = |block: NotionBlock, open: bool| VisibleBlock {
            block,
            depth: 0,
            is_toggle_open: open,
            numbered_index: 0,
        };
        let two = vec![NotionBlock::default(), NotionBlock::default()];

        // Collapsed toggleable headings show their child count like toggles do.
        assert_eq!(
            toggle_children_hint(&vb(heading(two.clone(), true), false), &ui).as_deref(),
            Some("  (2 items)")
        );
        assert_eq!(toggle_children_hint(&vb(heading(two, true), true), &ui), None);
        // Children past the fetched tree: an expandable cue, then a loading cue while fetching.
        assert_eq!(
            toggle_children_hint(&vb(heading(Vec::new(), true), false), &ui).as_deref(),
            Some("  (expand to load)")
        );
        ui.hydrating_blocks.insert("blk-h".into());
        assert_eq!(
            toggle_children_hint(&vb(heading(Vec::new(), true), true), &ui).as_deref(),
            Some("  (loading...)")
        );
        assert_eq!(toggle_children_hint(&vb(heading(Vec::new(), false), false), &ui), None);
    }

    #[test]
    fn database_truncation_banner_reports_fetched_rows_and_active_limit() {
        assert_eq!(
            database_truncation_banner(crate::sync::TUI_DATABASE_ROW_LIMIT),
            "   250+ rows · showing first 250 · 250-row limit"
        );
        assert_eq!(
            database_truncation_banner(crate::sync::EXPLICIT_DATABASE_ROW_LIMIT),
            "   5000+ rows · showing first 5000 · 5000-row limit"
        );
    }
}
