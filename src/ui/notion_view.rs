//! Signature Notion-Native Terminal Canvas (`src/ui/notion_view.rs`):
//! - Minimal 1-line Top Breadcrumb Bar (`Workspace ▾ / Parent / 📄 Page` + `[Width]` `[Sync]` `[? Help]`)
//! - Adaptive Main Canvas:
//!   A. Editorial Page Canvas (Centered ~88 col measure vs Full Width, Page Header + Properties Strip,
//!      Interactive Blocks: Callouts, Toggles, To-do Checkboxes, Code Blocks, Unicode Tables,
//!      Inline Images, Child Page/Database Cards, + Comments Peek Drawer `C`)
//!   B. Database View (`1 Table`, `2 Board` Kanban, `3 List`, Filter `f`, Sort `s`, New Item `n`).

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
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
        NotionPageDetail, NotionPropertyValue, RichTextSpan,
    },
    ui::{
        app::{AppState, CanvasView, Focus, InlineSyncStatus, UiState, VisibleBlock},
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
        draw_empty_page_welcome(frame, content_area, &theme);
        return;
    };

    if ui
        .inline_editor
        .as_ref()
        .is_some_and(|ed| ed.page_id == detail.page.id)
    {
        draw_inline_page_editor(frame, area, content_area, &theme, ui, hitmap);
        return;
    }

    let visible = app.visible_blocks(ui);
    let selected_idx = ui.selected_block.min(visible.len().saturating_sub(1));
    let canvas_focused = ui.focus == Focus::Canvas;

    let mut rendered_lines: Vec<(Line<'static>, Option<HitTarget>)> = Vec::new();
    let mut inline_hits: Vec<(usize, u16, u16, HitTarget)> = Vec::new();

    // Generous breathing room at top
    rendered_lines.push((Line::from(""), None));

    // Page Header: Large Icon + Bold Title + Favorite Star + Quick Pill Bar
    let fav_mark = if detail.page.is_favorite { " ★" } else { "" };
    rendered_lines.push((
        Line::from(vec![
            Span::styled(
                format!("{}  ", detail.page.display_icon()),
                Style::default().fg(theme.fg_primary),
            ),
            Span::styled(
                format!("{}{fav_mark}", detail.page.display_title()),
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        None,
    ));

    // Subtle metadata line (Last Edited + Comments count + Media count)
    let edited = detail
        .page
        .last_edited_time
        .as_deref()
        .map(short_timestamp)
        .unwrap_or_else(|| "Synced".into());
    let edited_prefix = format!("Edited {edited}  ·  ");
    let comment_badge = if detail.comments.is_empty() {
        "💬 0".to_string()
    } else {
        format!("💬 {}", detail.comments.len())
    };
    let meta_line_idx = rendered_lines.len();
    let comment_x = edited_prefix.width() as u16;
    let comment_w = comment_badge.width() as u16;
    if comment_w > 0 {
        inline_hits.push((
            meta_line_idx,
            comment_x,
            comment_w,
            HitTarget::Button(ButtonId::ToggleComments),
        ));
    }
    let mut meta_spans = vec![
        Span::styled(edited_prefix, theme.muted_style()),
        Span::styled(comment_badge, theme.accent_style()),
    ];
    if !detail.media.is_empty() {
        let sep = "  ·  ";
        let media_badge = format!("🖼 {} media", detail.media.len());
        let media_x = comment_x
            .saturating_add(comment_w)
            .saturating_add(sep.width() as u16);
        let media_w = media_badge.width() as u16;
        if media_w > 0 {
            inline_hits.push((
                meta_line_idx,
                media_x,
                media_w,
                HitTarget::Button(ButtonId::ImagePreview),
            ));
        }
        meta_spans.push(Span::styled(sep, theme.muted_style()));
        meta_spans.push(Span::styled(media_badge, theme.muted_style()));
    }
    rendered_lines.push((Line::from(meta_spans), None));

    // Clickable Page Quick Action Pill Bar
    let action_line_idx = rendered_lines.len();
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
    let action_pills: [(&str, ButtonId, bool); 8] = [
        (fav_pill, ButtonId::ToggleFavorite, detail.page.is_favorite),
        ("[+ Append]", ButtonId::QuickAppend, false),
        ("[+ Child]", ButtonId::NewChildPage, false),
        ("[✎ Edit]", ButtonId::EditPage, false),
        ("[↗ Web]", ButtonId::OpenWeb, false),
        ("[🔗 Links]", ButtonId::LinkExtractor, false),
        (href_pill, ButtonId::ToggleHrefs, ui.show_inline_hrefs),
        (&files_pill, ButtonId::Attachments, false),
    ];
    let mut action_spans: Vec<Span<'static>> = Vec::new();
    let mut ax = 0u16;
    for (pill_idx, (label, btn, active)) in action_pills.iter().enumerate() {
        let sep_w = if pill_idx == 0 { 0 } else { 1 };
        let w = label.width() as u16;
        if ax.saturating_add(sep_w + w) > content_area.width {
            break;
        }
        if pill_idx > 0 {
            action_spans.push(Span::raw(" "));
            ax = ax.saturating_add(1);
        }
        let style = if *active {
            theme.accent_style().add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg_muted).bg(theme.bg_card)
        };
        action_spans.push(Span::styled((*label).to_string(), style));
        inline_hits.push((action_line_idx, ax, w, HitTarget::Button(*btn)));
        ax = ax.saturating_add(w);
    }
    if !action_spans.is_empty() {
        rendered_lines.push((Line::from(action_spans), None));
    }

    // Properties Strip (for database rows or pages with properties)
    push_properties_strip(detail, &theme, &mut rendered_lines);

    // Subtle hairline divider
    let rule = "─".repeat(content_area.width.min(96) as usize);
    rendered_lines.push((
        Line::from(Span::styled(rule, Style::default().fg(theme.divider))),
        None,
    ));
    rendered_lines.push((Line::from(""), None));

    // Interactive Blocks
    let mut selected_line_range: Option<(usize, usize)> = None;
    if visible.is_empty() {
        rendered_lines.push((
            Line::from(Span::styled(
                "Empty page. Press `a` to append blocks (/h1, /todo, /callout, /code) or `e` to edit.",
                theme.muted_style().add_modifier(Modifier::ITALIC),
            )),
            None,
        ));
    } else {
        for (idx, vblk) in visible.iter().enumerate() {
            let is_sel = canvas_focused && idx == selected_idx;
            let start_line = rendered_lines.len();
            render_block_lines(
                vblk,
                idx,
                is_sel,
                content_area.width as usize,
                ui,
                &theme,
                &mut rendered_lines,
                &mut inline_hits,
            );
            let end_line = rendered_lines.len().saturating_sub(1);
            if idx == selected_idx && rendered_lines.len() > start_line {
                selected_line_range = Some((start_line, end_line));
            }
        }
    }

    // Keep the selected block visible when navigating with Up/Down/j/k, while allowing MouseScroll to roam freely
    if ui.canvas_follow_selected {
        if let Some((sel_start, sel_end)) = selected_line_range {
            let view_h = content_area.height as usize;
            let mut desired_scroll = ui.canvas_scroll as usize;
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

    for (row_offset, (line, target)) in rendered_lines
        .iter()
        .skip(scroll)
        .take(content_area.height as usize)
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
    let save_pill = "[Ctrl+S Save]";
    let done_pill = "[✓ Done]";
    let ext_pill = "[Ext $EDITOR]";
    let pos_text = format!("Ln {ln}, Col {col}");

    let mut right_items: Vec<(String, Option<ButtonId>, Style)> = vec![
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
    if content_area.width >= 66 {
        right_items.push((
            ext_pill.to_string(),
            Some(ButtonId::EditExternal),
            Style::default().fg(theme.fg_muted).bg(bar_bg),
        ));
    }
    if content_area.width >= 54 {
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
    let left_line = if left_budget >= edit_badge.width() + 3 + status_text.width() + 1 {
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
            if before_count % 2 == 0 {
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

fn push_properties_strip(
    detail: &NotionPageDetail,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
) {
    let mut prop_spans: Vec<Span<'static>> = Vec::new();
    for (name, val) in &detail.page.properties {
        if matches!(val, NotionPropertyValue::Title(_)) || val.is_empty() {
            continue;
        }
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
        prop_spans.push(Span::raw("   "));
    }

    if !prop_spans.is_empty() {
        out.push((Line::from(""), None));
        out.push((Line::from(prop_spans), None));
    }
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
            let mut spans = vec![cursor_gutter, Span::raw(format!("{indent}{prefix}"))];
            spans.extend(style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
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
            let mut spans = vec![cursor_gutter, Span::raw(format!("{indent}{prefix}"))];
            spans.extend(style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.accent_soft)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
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
            let mut spans = vec![cursor_gutter, Span::raw(format!("{indent}{prefix}"))];
            spans.extend(style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.fg_secondary)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
        }
        NotionBlockKind::Paragraph { rich_text, .. } => {
            let mut spans = vec![cursor_gutter, Span::raw(indent)];
            spans.extend(style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
        }
        NotionBlockKind::BulletedListItem { rich_text, .. } => {
            let mut spans = vec![
                cursor_gutter,
                Span::raw(indent),
                Span::styled("• ", theme.accent_style()),
            ];
            spans.extend(style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
        }
        NotionBlockKind::NumberedListItem { rich_text, .. } => {
            let num = vblk.numbered_index.max(1);
            let mut spans = vec![
                cursor_gutter,
                Span::raw(indent),
                Span::styled(format!("{num}. "), theme.accent_style()),
            ];
            spans.extend(style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
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
            let mut spans = vec![
                cursor_gutter,
                Span::raw(indent),
                Span::styled(box_glyph, box_style),
            ];
            spans.extend(style_rich_text(
                rich_text,
                text_base,
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((
                Line::from(spans).style(sel_bg),
                Some(HitTarget::TodoToggle(idx)),
            ));
        }
        NotionBlockKind::Toggle { rich_text, .. } => {
            let arrow = if vblk.is_toggle_open { "▾ " } else { "▸ " };
            let mut spans = vec![
                cursor_gutter,
                Span::raw(indent),
                Span::styled(arrow, theme.accent_style().add_modifier(Modifier::BOLD)),
            ];
            spans.extend(style_rich_text(
                rich_text,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
                theme,
                ui.show_inline_hrefs,
            ));
            if !vblk.is_toggle_open && !vblk.block.children.is_empty() {
                spans.push(Span::styled(
                    format!("  ({} items)", vblk.block.children.len()),
                    theme.muted_style(),
                ));
            }
            out.push((
                Line::from(spans).style(sel_bg),
                Some(HitTarget::ToggleBlock(idx)),
            ));
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
            let mut spans = vec![
                cursor_gutter,
                Span::raw(indent),
                Span::styled("┃ ", Style::default().fg(accent_col).bg(card_bg)),
                Span::styled(format!("{ic}  "), Style::default().bg(card_bg)),
            ];
            spans.extend(style_rich_text(
                rich_text,
                Style::default().fg(theme.fg_primary).bg(card_bg),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((
                Line::from(spans).style(Style::default().bg(card_bg)),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
        NotionBlockKind::Quote { rich_text, .. } => {
            let mut spans = vec![
                cursor_gutter,
                Span::raw(indent),
                Span::styled("│ ", Style::default().fg(theme.fg_muted)),
            ];
            spans.extend(style_rich_text(
                rich_text,
                theme.quote_style(),
                theme,
                ui.show_inline_hrefs,
            ));
            out.push((Line::from(spans).style(sel_bg), Some(HitTarget::CanvasBlock(idx))));
        }
        NotionBlockKind::Divider => {
            let w = max_width.saturating_sub(indent.len() + 4).max(12);
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
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
            let header_line_idx = out.len();
            let badge = format!("  {indent}╭─ {language} (y to copy) ");
            let mut header_spans = vec![Span::styled(
                badge.clone(),
                Style::default()
                    .fg(theme.fg_muted)
                    .bg(code_bg)
                    .add_modifier(Modifier::BOLD),
            )];
            if is_html {
                let preview_pill = "[▶ Preview HTML]";
                let pill_x = badge.width() as u16;
                let pill_w = preview_pill.width() as u16;
                header_spans.push(Span::styled(
                    preview_pill,
                    Style::default()
                        .fg(theme.bg_canvas)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ));
                inline_hits.push((
                    header_line_idx,
                    pill_x,
                    pill_w,
                    HitTarget::CodeHtmlPreview(idx),
                ));
            }
            out.push((Line::from(header_spans), Some(HitTarget::CanvasBlock(idx))));
            let raw = spans_plain_text(rich_text);
            for line in raw.lines() {
                out.push((
                    Line::from(vec![
                        cursor_gutter.clone(),
                        Span::raw(indent.clone()),
                        Span::styled(
                            format!("│ {line}"),
                            Style::default().fg(theme.code_fg).bg(code_bg),
                        ),
                    ])
                    .style(Style::default().bg(code_bg)),
                    Some(HitTarget::CanvasBlock(idx)),
                ));
            }
            out.push((
                Line::from(Span::styled(
                    format!("  {indent}╰────────────────────"),
                    Style::default().fg(theme.border),
                )),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
        NotionBlockKind::Table { .. } => {
            render_unicode_table(vblk, idx, selected, max_width, theme, out);
        }
        NotionBlockKind::Image { url, caption, .. } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() {
                "Image".to_string()
            } else {
                cap
            };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(
                        format!(" 🖼 [IMG: {label}] (press v to preview) "),
                        Style::default()
                            .fg(theme.accent_soft)
                            .bg(theme.bg_card)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if ui.show_inline_hrefs {
                            format!("  {url}")
                        } else {
                            String::new()
                        },
                        theme.muted_style(),
                    ),
                ])
                .style(sel_bg),
                Some(HitTarget::MediaChip(idx)),
            ));
        }
        NotionBlockKind::Video { url, caption, .. } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() {
                url.as_str()
            } else {
                cap.as_str()
            };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(
                        format!(" 🎬 [VIDEO: {label}] (Enter: open) "),
                        Style::default()
                            .fg(theme.accent_soft)
                            .bg(theme.bg_card)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if ui.show_inline_hrefs && !cap.is_empty() {
                            format!("  {url}")
                        } else {
                            String::new()
                        },
                        theme.muted_style(),
                    ),
                ])
                .style(sel_bg),
                Some(HitTarget::MediaChip(idx)),
            ));
        }
        NotionBlockKind::Audio { url, caption, .. } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() {
                url.as_str()
            } else {
                cap.as_str()
            };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(
                        format!(" 🎵 [AUDIO: {label}] (Enter: open) "),
                        Style::default()
                            .fg(theme.accent_soft)
                            .bg(theme.bg_card)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
                .style(sel_bg),
                Some(HitTarget::MediaChip(idx)),
            ));
        }
        NotionBlockKind::ChildPage { title } => {
            let card_bg = if selected {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(
                        format!(" 📄 {title}  → "),
                        Style::default()
                            .fg(theme.fg_primary)
                            .bg(card_bg)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    ),
                ])
                .style(Style::default().bg(card_bg)),
                Some(HitTarget::ChildCard(idx)),
            ));
        }
        NotionBlockKind::ChildDatabase { title } => {
            let card_bg = if selected {
                theme.bg_selected
            } else {
                theme.bg_card
            };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(
                        format!(" 🗂 {title}  [Database →] "),
                        Style::default()
                            .fg(theme.accent_soft)
                            .bg(card_bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])
                .style(Style::default().bg(card_bg)),
                Some(HitTarget::ChildCard(idx)),
            ));
        }
        NotionBlockKind::Bookmark { url, caption }
        | NotionBlockKind::Embed { url, caption } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() { url.as_str() } else { cap.as_str() };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(
                        format!(" 🔗 {label} "),
                        Style::default()
                            .fg(theme.accent_soft)
                            .bg(theme.bg_card)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ])
                .style(sel_bg),
                Some(HitTarget::ChildCard(idx)),
            ));
        }
        NotionBlockKind::LinkPreview { url } => {
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(format!(" 🔗 {url}"), Style::default().fg(theme.accent_soft)),
                ])
                .style(sel_bg),
                Some(HitTarget::ChildCard(idx)),
            ));
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
            } else {
                Span::styled(
                    format!(" 📎 {label} "),
                    Style::default().fg(theme.fg_secondary).bg(theme.bg_card),
                )
            };
            out.push((
                Line::from(vec![cursor_gutter, Span::raw(indent), chip]).style(sel_bg),
                Some(HitTarget::MediaChip(idx)),
            ));
        }
        NotionBlockKind::Pdf { url, caption } => {
            let cap = spans_plain_text(caption);
            let label = if cap.is_empty() { url.as_str() } else { cap.as_str() };
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(format!(" 📄 [PDF] {label} "), Style::default().fg(theme.warning).bg(theme.bg_card)),
                ])
                .style(sel_bg),
                Some(HitTarget::MediaChip(idx)),
            ));
        }
        NotionBlockKind::Equation { expression } => {
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(format!(" ∑  {expression} "), theme.code_style()),
                ])
                .style(sel_bg),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
        NotionBlockKind::TableOfContents { .. } => {
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled("[Table of Contents]", theme.muted_style()),
                ])
                .style(sel_bg),
                Some(HitTarget::CanvasBlock(idx)),
            ));
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
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(format!("[ ⚡ {label} ]"), pill_style),
                ])
                .style(sel_bg),
                Some(HitTarget::NotionButton(idx)),
            ));
        }
        NotionBlockKind::ColumnList
        | NotionBlockKind::Column
        | NotionBlockKind::SyncedBlock { .. }
        | NotionBlockKind::TableRow { .. } => {}
        NotionBlockKind::Unsupported { raw_type } => {
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(format!("[{raw_type}]"), theme.muted_style()),
                ])
                .style(sel_bg),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
    }
}

fn render_unicode_table(
    vblk: &VisibleBlock,
    idx: usize,
    selected: bool,
    max_width: usize,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for child in &vblk.block.children {
        if let NotionBlockKind::TableRow { cells } = &child.kind {
            rows.push(cells.iter().map(|c| spans_plain_text(c)).collect());
        }
    }
    if rows.is_empty() {
        return;
    }
    let col_count = rows.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let max_col_w = ((max_width.saturating_sub(col_count * 3 + 4)) / col_count).clamp(8, 28);
    let mut widths = vec![4usize; col_count];
    for r in &rows {
        for (c, cell) in r.iter().enumerate() {
            widths[c] = widths[c].max(cell.width().min(max_col_w));
        }
    }

    let border_style = if selected {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.border)
    };
    let top = format!(
        "  ┌{}┐",
        widths
            .iter()
            .map(|w| "─".repeat(*w + 2))
            .collect::<Vec<_>>()
            .join("┬")
    );
    let mid = format!(
        "  ├{}┤",
        widths
            .iter()
            .map(|w| "─".repeat(*w + 2))
            .collect::<Vec<_>>()
            .join("┼")
    );
    let bot = format!(
        "  └{}┘",
        widths
            .iter()
            .map(|w| "─".repeat(*w + 2))
            .collect::<Vec<_>>()
            .join("┴")
    );

    out.push((
        Line::from(Span::styled(top, border_style)),
        Some(HitTarget::CanvasBlock(idx)),
    ));
    for (r_idx, r) in rows.iter().enumerate() {
        let mut spans = vec![Span::styled("  │", border_style)];
        for (c, &w) in widths.iter().enumerate() {
            let text = r.get(c).map(String::as_str).unwrap_or("");
            let trunc = truncate_width(text, w);
            let pad = w.saturating_sub(trunc.width());
            let cell_style = if r_idx == 0 {
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg_secondary)
            };
            spans.push(Span::styled(format!(" {trunc}{} ", " ".repeat(pad)), cell_style));
            spans.push(Span::styled("│", border_style));
        }
        out.push((Line::from(spans), Some(HitTarget::CanvasBlock(idx))));
        if r_idx == 0 && rows.len() > 1 {
            out.push((
                Line::from(Span::styled(mid.clone(), border_style)),
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
    }
    out.push((
        Line::from(Span::styled(bot, border_style)),
        Some(HitTarget::CanvasBlock(idx)),
    ));
}

fn style_rich_text(
    spans: &[RichTextSpan],
    base: Style,
    theme: &Theme,
    show_hrefs: bool,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for sp in spans {
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
            lines.push(Line::from(Span::styled(
                format!("  {}", c.body),
                Style::default().fg(theme.fg_primary),
            )));
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
        let limit = crate::sync::TUI_DATABASE_ROW_LIMIT;
        title_line.push(Span::styled(
            format!("   {limit}+ rows · showing first {limit}"),
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
        ((area.width as usize).saturating_sub(title_w + 4) / extra_cols.len()).clamp(10, 22)
    };

    // Header row
    let mut header_spans = vec![Span::styled(
        format!(" {:<title_w$} ", "Name"),
        theme.muted_style().add_modifier(Modifier::BOLD),
    )];
    for col in &extra_cols {
        header_spans.push(Span::styled(
            format!("│ {:<col_w$} ", truncate_width(col, col_w)),
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
        let pad_title = title_w.saturating_sub(trunc_title.width());
        let mut spans = vec![Span::styled(
            format!(" {trunc_title}{} ", " ".repeat(pad_title)),
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
            let pad = width.saturating_sub(1);
            out.push(Span::styled(
                format!("{glyph}{}", " ".repeat(pad)),
                Style::default()
                    .fg(if *b { theme.success } else { theme.fg_muted })
                    .bg(row_bg),
            ));
        }
        _ => {
            let txt = truncate_width(&v.to_plain_string(), width);
            let pad = width.saturating_sub(txt.width());
            out.push(Span::styled(
                format!("{txt}{}", " ".repeat(pad)),
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

    let visible_cols = ((area.width as usize) / 24).clamp(1, cols.len().min(5));
    let start_col = ui
        .db_board_col
        .saturating_sub(visible_cols.saturating_sub(1));
    let slice = &cols[start_col..(start_col + visible_cols).min(cols.len())];
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
        let is_active_col = focused && abs_col == ui.db_board_col;
        let inner = Rect::new(
            col_rect.x,
            col_rect.y,
            col_rect.width.saturating_sub(1).max(1),
            col_rect.height,
        );
        if inner.height < 2 {
            continue;
        }

        // Pill header + card count
        let pill_text = format!(" {} ", truncate_width(&col.label, inner.width.saturating_sub(8) as usize));
        let count_text = format!("  {}", col.cards.len());
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
        for (row_idx, card) in col.cards.iter().enumerate() {
            if y + 3 > inner.bottom() {
                break;
            }
            let card_rect = Rect::new(inner.x, y, inner.width, 3);
            let is_sel = is_active_col && row_idx == ui.db_board_row;
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
