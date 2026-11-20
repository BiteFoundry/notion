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
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
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
        app::{AppState, CanvasView, Focus, UiState, VisibleBlock},
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

    let width_pill = format!(" [Width: {}] ", ui.page_width.label());
    let sync_pill = if app.syncing {
        " [Syncing...] ".to_string()
    } else {
        " [Sync] ".to_string()
    };
    let help_pill = " [? Help] ".to_string();

    let right_total = (width_pill.width() + sync_pill.width() + help_pill.width()) as u16;
    let left_w = area.width.saturating_sub(right_total);

    // Left breadcrumb segments
    let mut x = area.x;
    let max_x = area.x.saturating_add(left_w);
    let mut spans: Vec<Span<'static>> = Vec::new();

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

    // Right pills: [Width: Centered|Full] [Sync] [? Help]
    let mut rx = area.x.saturating_add(left_w);
    let pills = [
        (width_pill, ButtonId::ToggleWidth, theme.fg_muted),
        (
            sync_pill,
            ButtonId::SyncNow,
            if app.syncing {
                theme.accent_soft
            } else {
                theme.fg_muted
            },
        ),
        (help_pill, ButtonId::Help, theme.fg_muted),
    ];
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
        draw_comments_drawer(frame, split[2], app, ui);
    } else {
        draw_page_canvas(frame, area, app, ui, hitmap);
    }
}

fn draw_page_canvas(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &UiState,
    hitmap: &mut HitMap,
) {
    let theme = &ui.theme;
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
        draw_empty_page_welcome(frame, content_area, theme);
        return;
    };

    let visible = app.visible_blocks(ui);
    let selected_idx = ui.selected_block.min(visible.len().saturating_sub(1));
    let canvas_focused = ui.focus == Focus::Canvas;

    let mut rendered_lines: Vec<(Line<'static>, Option<HitTarget>)> = Vec::new();

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
    let comment_badge = if detail.comments.is_empty() {
        "💬 0".to_string()
    } else {
        format!("💬 {}", detail.comments.len())
    };
    rendered_lines.push((
        Line::from(vec![
            Span::styled(format!("Edited {edited}  ·  "), theme.muted_style()),
            Span::styled(comment_badge, theme.accent_style()),
            Span::styled(
                if detail.media.is_empty() {
                    String::new()
                } else {
                    format!("  ·  🖼 {} media", detail.media.len())
                },
                theme.muted_style(),
            ),
        ]),
        None,
    ));

    // Properties Strip (for database rows or pages with properties)
    push_properties_strip(detail, theme, &mut rendered_lines);

    // Subtle hairline divider
    let rule = "─".repeat(content_area.width.min(96) as usize);
    rendered_lines.push((
        Line::from(Span::styled(rule, Style::default().fg(theme.divider))),
        None,
    ));
    rendered_lines.push((Line::from(""), None));

    // Interactive Blocks
    if visible.is_empty() {
        rendered_lines.push((
            Line::from(Span::styled(
                "Empty page. Press `a` to append blocks (/h1, /todo, /callout, /code) or `e` to edit in $EDITOR.",
                theme.muted_style().add_modifier(Modifier::ITALIC),
            )),
            None,
        ));
    } else {
        for (idx, vblk) in visible.iter().enumerate() {
            let is_sel = canvas_focused && idx == selected_idx;
            render_block_lines(
                vblk,
                idx,
                is_sel,
                content_area.width as usize,
                ui,
                theme,
                &mut rendered_lines,
            );
        }
    }

    // Compute viewport scroll
    let max_scroll = rendered_lines
        .len()
        .saturating_sub(content_area.height as usize);
    let scroll = (ui.canvas_scroll as usize).min(max_scroll);

    for (row_offset, (line, target)) in rendered_lines
        .iter()
        .skip(scroll)
        .take(content_area.height as usize)
        .enumerate()
    {
        let y = content_area.y + row_offset as u16;
        let row_rect = Rect::new(content_area.x, y, content_area.width, 1);
        frame.render_widget(Paragraph::new(line.clone()), row_rect);
        if let Some(hit) = *target {
            hitmap.add_line(content_area.x, y, content_area.width, hit);
        }
    }
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

fn render_block_lines(
    vblk: &VisibleBlock,
    idx: usize,
    selected: bool,
    max_width: usize,
    ui: &UiState,
    theme: &Theme,
    out: &mut Vec<(Line<'static>, Option<HitTarget>)>,
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
            let badge = format!("  {indent}╭─ {language} (y to copy) ");
            out.push((
                Line::from(Span::styled(
                    badge,
                    Style::default()
                        .fg(theme.fg_muted)
                        .bg(code_bg)
                        .add_modifier(Modifier::BOLD),
                )),
                Some(HitTarget::CanvasBlock(idx)),
            ));
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
                Some(HitTarget::CanvasBlock(idx)),
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
                Some(HitTarget::CanvasBlock(idx)),
            ));
        }
        NotionBlockKind::File { url, name, .. } => {
            let label = name.as_deref().unwrap_or(url.as_str());
            out.push((
                Line::from(vec![
                    cursor_gutter,
                    Span::raw(indent),
                    Span::styled(format!(" 📎 {label} "), Style::default().fg(theme.fg_secondary).bg(theme.bg_card)),
                ])
                .style(sel_bg),
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

fn draw_comments_drawer(frame: &mut Frame, area: Rect, app: &AppState, ui: &UiState) {
    let theme = &ui.theme;
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

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " 💬 Comments ",
                theme.title(focused).add_modifier(Modifier::BOLD),
            ),
            Span::styled("(C to close)", theme.muted_style()),
        ]))
        .style(Style::default().bg(theme.bg_sidebar)),
        chunks[0],
    );

    let comments = app
        .active_page_detail
        .as_ref()
        .map(|d| d.comments.as_slice())
        .unwrap_or(&[]);

    if comments.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                " No comments yet.\n Type below and press Enter.",
                theme.muted_style(),
            ))
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(theme.bg_sidebar)),
            chunks[1],
        );
    } else {
        let mut lines = Vec::new();
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                title,
                Style::default()
                    .fg(theme.fg_primary)
                    .add_modifier(Modifier::BOLD),
            )),
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
