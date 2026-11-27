//! Interactive HTML Viewer Overlay (`Overlay::HtmlPreview`) backed by `bitefoundry-html`.
//!
//! - `0 MB` idle on static HTML; on-demand QuickJS-NG sandbox when `<script>` / `on*` handlers exist,
//!   dropped immediately when the overlay closes.
//! - Keyboard focus cycling (`Tab` / `Shift+Tab`), activation (`Enter` / `Space`), inline `<input>`
//!   editing, mouse `click_at`, Rendered ↔ Raw toggle (`m`), and lazy localhost Axum serving (`o`).

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use bitefoundry_html::{HtmlEffect, HtmlTheme, HtmlViewSession, SourceKind};
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::{self, FitBox, RenderedTerminalImage, TerminalImageProtocol},
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
        theme::Theme,
    },
};

struct CachedHtmlGraphic {
    source: Arc<[u8]>,
    rendered: RenderedTerminalImage,
}

type GraphicEnv = ((u16, u16), (u8, u8, u8));

pub struct HtmlPreviewOverlay {
    pub title: String,
    pub session: HtmlViewSession,
    pub body_rect: Rect,
    pub last_max_scroll: u16,
    graphic_cache: HashMap<(usize, u16, TerminalImageProtocol), CachedHtmlGraphic>,
    last_graphic_env: Option<GraphicEnv>,
}

impl Drop for HtmlPreviewOverlay {
    fn drop(&mut self) {
        self.graphic_cache.clear();
    }
}

impl fmt::Debug for HtmlPreviewOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HtmlPreviewOverlay")
            .field("title", &self.title)
            .field("raw_mode", &self.session.raw_mode)
            .field("js_active", &self.session.js_active)
            .field("scroll_y", &self.session.scroll_y)
            .field("focused_control", &self.session.focused_control)
            .field("editing_input", &self.session.editing_input)
            .field("cached_graphics", &self.graphic_cache.len())
            .finish()
    }
}

impl HtmlPreviewOverlay {
    pub fn new(title: impl Into<String>, raw_html: String) -> Self {
        let title_str = title.into();
        let mut session = HtmlViewSession::new(
            (!title_str.trim().is_empty()).then_some(title_str.clone()),
            raw_html,
        );
        let resolved_title = session
            .title()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if title_str.trim().is_empty() {
                    "HTML Preview".to_string()
                } else {
                    title_str
                }
            });
        // Surface any initial status/toast from startup scripts without losing it.
        let init_effects = session.take_pending_effects();
        for eff in init_effects {
            if let HtmlEffect::Toast(msg) = eff {
                session.last_status = Some(msg);
            }
        }
        Self {
            title: resolved_title,
            session,
            body_rect: Rect::default(),
            last_max_scroll: 0,
            graphic_cache: HashMap::new(),
            last_graphic_env: None,
        }
    }

    pub fn new_markdown(title: impl Into<String>, raw_md: String) -> Self {
        let title_str = title.into();
        let session = HtmlViewSession::new_markdown(
            (!title_str.trim().is_empty()).then_some(title_str.clone()),
            raw_md,
        );
        let resolved_title = session
            .title()
            .filter(|t| !t.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if title_str.trim().is_empty() {
                    "Markdown Preview".to_string()
                } else {
                    title_str
                }
            });
        Self {
            title: resolved_title,
            session,
            body_rect: Rect::default(),
            last_max_scroll: 0,
            graphic_cache: HashMap::new(),
            last_graphic_env: None,
        }
    }

    pub fn accepts_text(&self) -> bool {
        self.session.editing_input.is_some()
    }

    pub fn toggle_raw_mode(&mut self) {
        if self.session.editing_input.is_some() {
            let _ = self.session.finish_editing();
        }
        let raw = self.session.toggle_raw_mode();
        self.session.scroll_y = 0;
        self.session.last_status = Some(match (self.session.source_kind(), raw) {
            (SourceKind::Markdown, true) => "Raw Markdown source view".to_string(),
            (SourceKind::Markdown, false) => "Rendered Markdown view".to_string(),
            (SourceKind::Html, true) => "Raw HTML source view".to_string(),
            (SourceKind::Html, false) => "Rendered interactive view".to_string(),
        });
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(24).min(area.width),
            area.height.saturating_sub(2).max(12).min(area.height),
        )
    }

    pub fn html_theme_from(theme: &Theme) -> HtmlTheme {
        if theme.no_color {
            return HtmlTheme::no_color();
        }
        if theme.ansi16_only {
            return HtmlTheme {
                text: Color::White,
                muted: Color::DarkGray,
                heading: Color::LightCyan,
                accent: Color::Cyan,
                link: Color::LightCyan,
                code_fg: Color::Yellow,
                code_bg: Color::Black,
                quote_border: Color::DarkGray,
                border: Color::DarkGray,
                control_fg: Color::White,
                control_bg: Color::DarkGray,
                focus_fg: Color::Black,
                focus_bg: Color::Cyan,
                raw_tag: Color::LightCyan,
                raw_attr: Color::Yellow,
            };
        }
        HtmlTheme {
            text: theme.fg_primary,
            muted: theme.fg_muted,
            heading: theme.accent_soft,
            accent: theme.accent,
            link: theme.accent_soft,
            code_fg: theme.code_fg,
            code_bg: theme.bg_code,
            quote_border: theme.fg_muted,
            border: theme.border,
            control_fg: theme.fg_primary,
            control_bg: theme.bg_card,
            focus_fg: theme.bg_canvas,
            focus_bg: theme.accent,
            raw_tag: theme.accent_soft,
            raw_attr: theme.warning,
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        if self.session.editing_input.is_some() {
            return match action {
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                Action::CloseOverlay | Action::Activate | Action::OverlaySubmit => {
                    let effs = self.session.finish_editing();
                    self.apply_effects(effs)
                }
                Action::Tab => {
                    let _ = self.session.finish_editing();
                    self.session.focus_next_control();
                    OverlayOutcome::Handled
                }
                Action::PrevFocus => {
                    let _ = self.session.finish_editing();
                    self.session.focus_prev_control();
                    OverlayOutcome::Handled
                }
                Action::Backspace => {
                    self.session.backspace();
                    OverlayOutcome::Handled
                }
                Action::Input(ch) => {
                    self.session.input_char(*ch);
                    OverlayOutcome::Handled
                }
                Action::Paste(text) => {
                    for ch in text.chars() {
                        if ch != '\r' && ch != '\n' {
                            self.session.input_char(ch);
                        }
                    }
                    OverlayOutcome::Handled
                }
                _ => OverlayOutcome::Handled,
            };
        }

        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::Tab => {
                self.session.focus_next_control();
                OverlayOutcome::Handled
            }
            Action::PrevFocus => {
                self.session.focus_prev_control();
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit | Action::Input(' ') => {
                let effs = self.session.activate_focused();
                self.apply_effects(effs)
            }
            Action::Input('m') | Action::Input('M') | Action::Input('r') | Action::Input('R') => {
                self.toggle_raw_mode();
                OverlayOutcome::Handled
            }
            Action::OpenActiveInBrowser | Action::Input('o') | Action::Input('O') => {
                OverlayOutcome::Dispatch(Action::ServeActiveHtml)
            }
            Action::MoveUp | Action::Input('k') => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.session.scroll_y = self.session.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.session.scroll_y = self.session.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.session.scroll_y = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.session.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.session.scroll_y =
                        self.session.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    self.session.scroll_y =
                        self.session.scroll_y.saturating_add(*delta as u16);
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    /// Maps a terminal mouse click `(col, row)` inside the overlay body to `(line_index, rel_col)`
    /// and invokes `HtmlViewSession::click_at`.
    pub fn handle_click(&mut self, col: u16, row: u16) -> OverlayOutcome {
        if self.body_rect.width == 0 || self.body_rect.height == 0 {
            return OverlayOutcome::Handled;
        }
        if col >= self.body_rect.x
            && col < self.body_rect.right()
            && row >= self.body_rect.y
            && row < self.body_rect.bottom()
        {
            let line_index = self.session.scroll_y as usize + (row - self.body_rect.y) as usize;
            let rel_col = col - self.body_rect.x;
            let effs = self.session.click_at(line_index, rel_col);
            return self.apply_effects(effs);
        }
        OverlayOutcome::Handled
    }

    fn apply_effects(&mut self, effects: Vec<HtmlEffect>) -> OverlayOutcome {
        let mut open_url: Option<String> = None;
        let mut toast: Option<String> = None;

        for eff in effects {
            match eff {
                HtmlEffect::None | HtmlEffect::DomChanged => {}
                HtmlEffect::OpenUrl(url) => {
                    open_url = Some(url);
                }
                HtmlEffect::Toast(msg) => {
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
                HtmlEffect::FormSubmitted { action, fields } => {
                    let pairs = fields
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let msg = if action.trim().is_empty() {
                        format!("Form submitted ({pairs})")
                    } else {
                        format!("Form -> {action} ({pairs})")
                    };
                    self.session.last_status = Some(msg.clone());
                    toast = Some(msg);
                }
            }
        }

        if let Some(url) = open_url {
            return OverlayOutcome::Dispatch(Action::OpenUrl(url));
        }
        if let Some(msg) = toast {
            return OverlayOutcome::Dispatch(Action::ShowToast(msg));
        }
        OverlayOutcome::Handled
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        _app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let mode_pill = if self.session.raw_mode {
            "[m: Raw]"
        } else {
            "[m: Rendered]"
        };
        let serve_pill = "[o: Serve]";
        let close_pill = "[✕ Esc]";
        let pills_w = (mode_pill.width() + 1 + serve_pill.width() + 1 + close_pill.width()) as u16;
        let show_pills = rect.width > pills_w + 8;

        let (icon, mode_badge) = match self.session.source_kind() {
            SourceKind::Markdown => ("📝", "MD"),
            SourceKind::Html => ("🌐", if self.session.js_active { "JS" } else { "Static" }),
        };
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_pills { pills_w + 2 } else { 0 })
            as usize;
        let header = format!(
            " {icon} {} · {} ",
            crate::ui::truncate_width(&self.title, 34),
            mode_badge
        );

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg_primary))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&header, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_pills {
            let mut px = rect.right().saturating_sub(pills_w + 1);
            for (label, btn, style) in [
                (
                    mode_pill,
                    ButtonId::HtmlToggleMode,
                    Style::default()
                        .fg(theme.accent_soft)
                        .bg(theme.bg_elevated)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    serve_pill,
                    ButtonId::HtmlServeBrowser,
                    Style::default()
                        .fg(theme.success)
                        .bg(theme.bg_elevated)
                        .add_modifier(Modifier::BOLD),
                ),
                (
                    close_pill,
                    ButtonId::ConfirmCancel,
                    Style::default().fg(theme.fg_primary).bg(theme.bg_elevated),
                ),
            ] {
                let w = label.width() as u16;
                frame.render_widget(
                    Paragraph::new(Span::styled(label, style)),
                    Rect::new(px, rect.y, w, 1),
                );
                hitmap.add_line(px, rect.y, w, HitTarget::Button(btn));
                px = px.saturating_add(w + 1);
            }
        }

        if inner.width == 0 || inner.height == 0 {
            self.body_rect = Rect::default();
            return;
        }

        let body_h = inner.height.saturating_sub(1).max(1);
        let footer_y = inner.y + inner.height.saturating_sub(1);
        let html_theme = Self::html_theme_from(&theme);

        // Reserve 1 column for a potential scrollbar upfront so `HtmlViewSession`'s 1-slot
        // layout cache hits on every frame without 2-pass width thrashing.
        let content_w = if inner.width > 3 {
            inner.width.saturating_sub(1)
        } else {
            inner.width
        };

        let (total_rows, controls_len, graphics, sy, visible_lines) = {
            let cur_scroll = self.session.scroll_y;
            let rendered = self
                .session
                .render(content_w, &html_theme, ui.show_inline_hrefs);
            let total_rows = rendered.lines.len();
            let max_scroll = total_rows
                .saturating_sub(body_h as usize)
                .min(u16::MAX as usize) as u16;
            let sy = cur_scroll.min(max_scroll);
            let view_start = sy as usize;
            let view_end = view_start.saturating_add(body_h as usize).min(total_rows);
            (
                total_rows,
                rendered.controls.len(),
                rendered.graphics.clone(),
                sy,
                rendered.lines[view_start..view_end].to_vec(),
            )
        };
        let show_scrollbar = total_rows > body_h as usize && inner.width > 3;
        let max_scroll = total_rows.saturating_sub(body_h as usize).min(u16::MAX as usize) as u16;
        self.last_max_scroll = max_scroll;
        self.session.scroll_y = sy;
        let view_start = sy as usize;
        let view_end = view_start.saturating_add(body_h as usize).min(total_rows);
        let mut visible_lines = visible_lines;

        self.body_rect = Rect::new(inner.x, inner.y, content_w, body_h);

        let mut pixel_skip_rects: Vec<Rect> = Vec::new();
        if !graphics.is_empty() && content_w > 0 && body_h > 0 {
            let proto = ui.graphics_caps.protocol;
            let bg_rgb = img_preview::color_to_rgb(theme.bg_elevated, (24, 24, 30));
            let env_sig = (ui.graphics_caps.cell_px, bg_rgb);
            if self.last_graphic_env != Some(env_sig) {
                self.graphic_cache.clear();
                self.last_graphic_env = Some(env_sig);
            }

            for g in &graphics {
                if g.line_count == 0 {
                    continue;
                }
                let g_start = g.line_start;
                let g_end = g.line_start.saturating_add(g.line_count);
                if g_end <= view_start || g_start >= view_end {
                    continue;
                }

                let cache_key = (g.node_id.0, content_w, proto);
                let cache_hit = self
                    .graphic_cache
                    .get(&cache_key)
                    .is_some_and(|c| Arc::ptr_eq(&c.source, &g.image_bytes));
                if !cache_hit {
                    let slot_rows = g.line_count.min(u16::MAX as usize) as u16;
                    let fit = FitBox::fixed(content_w, slot_rows);
                    let decoded = if proto == TerminalImageProtocol::Halfblock {
                        img_preview::decode_and_render_halfblocks_with_cell_px(
                            &g.image_bytes,
                            fit,
                            bg_rgb,
                            ui.graphics_caps.cell_px,
                        )
                    } else {
                        img_preview::decode_and_render_for_protocol(
                            &g.image_bytes,
                            fit,
                            bg_rgb,
                            ui.graphics_caps,
                        )
                    };
                    if let Ok(rendered_img) = decoded {
                        self.graphic_cache.insert(
                            cache_key,
                            CachedHtmlGraphic {
                                source: Arc::clone(&g.image_bytes),
                                rendered: rendered_img,
                            },
                        );
                    }
                }

                let Some(cached) = self.graphic_cache.get(&cache_key) else {
                    continue;
                };

                if proto == TerminalImageProtocol::Halfblock {
                    let vis_start = g_start.max(view_start);
                    let vis_end = g_end.min(view_end);
                    for doc_row in vis_start..vis_end {
                        let row_off = doc_row - g_start;
                        let vis_idx = doc_row - view_start;
                        if let Some(slot) = visible_lines.get_mut(vis_idx) {
                            if let Some(hb_line) = cached.rendered.lines.get(row_off) {
                                *slot = hb_line.clone();
                            } else {
                                *slot = Line::default();
                            }
                        }
                    }
                } else {
                    let vis_start = g_start.max(view_start);
                    let vis_end = g_end.min(view_end);
                    let crop_top = (vis_start - g_start) as u16;
                    let vis_rows = (vis_end - vis_start) as u16;
                    let screen_y = self
                        .body_rect
                        .y
                        .saturating_add((vis_start - view_start) as u16);

                    if let Some((raw_payload, draw_cols, draw_rows)) = cached
                        .rendered
                        .viewport_pixel_payload(0, crop_top, content_w, vis_rows)
                    {
                        let actual_cols = draw_cols.min(content_w);
                        let actual_rows = draw_rows.min(vis_rows);
                        if actual_cols > 0 && actual_rows > 0 {
                            let payload = if proto == TerminalImageProtocol::Kitty {
                                let unique_id = img_preview::KITTY_PREVIEW_IMAGE_ID
                                    .saturating_add(1)
                                    .saturating_add((g.node_id.0 % 60_000) as u32);
                                let old_id =
                                    format!(",i={},", img_preview::KITTY_PREVIEW_IMAGE_ID);
                                let new_id = format!(",i={unique_id},");
                                raw_payload.replacen(&old_id, &new_id, 1)
                            } else {
                                raw_payload
                            };
                            let g_rect =
                                Rect::new(self.body_rect.x, screen_y, actual_cols, actual_rows);
                            pixel_skip_rects.push(g_rect);
                            ui.pending_pixel_graphics.push(PendingPixelGraphic {
                                cache_key: format!(
                                    "html:{}:{}:{}#crop:0,{},{},{}",
                                    g.node_id.0,
                                    content_w,
                                    proto.as_str(),
                                    crop_top,
                                    content_w,
                                    vis_rows
                                ),
                                rect: g_rect,
                                protocol: proto,
                                payload: Some(payload),
                            });
                        }
                    }
                }
            }
        }

        frame.render_widget(
            Paragraph::new(visible_lines)
                .style(Style::default().bg(theme.bg_elevated).fg(theme.fg_primary)),
            self.body_rect,
        );

        if !pixel_skip_rects.is_empty() {
            let buf = frame.buffer_mut();
            for g_rect in pixel_skip_rects {
                for row in g_rect.y..g_rect.bottom() {
                    for col in g_rect.x..g_rect.right() {
                        if let Some(cell) = buf.cell_mut((col, row)) {
                            cell.set_symbol(" ");
                            cell.set_skip(true);
                        }
                    }
                }
            }
        }

        if show_scrollbar {
            let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, body_h);
            let mut sb_state = ScrollbarState::new(max_scroll.saturating_add(1) as usize)
                .position(sy as usize);
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(Style::default().fg(theme.accent))
                .track_style(theme.muted_style());
            frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
        }

        if inner.height >= 2 {
            let footer_text = if self.session.editing_input.is_some() {
                " Editing text input · Type to edit · Backspace delete · Enter/Esc finish "
                    .to_string()
            } else if let Some(ref st) = self.session.last_status {
                format!(" {st} ")
            } else {
                format!(
                    " Tab/Shift+Tab Focus ({controls_len} controls) · Enter/Click Activate · j/k/Wheel Scroll · m Mode · o Serve · Esc Close "
                )
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&footer_text, inner.width as usize),
                    Style::default().fg(theme.fg_muted).bg(theme.bg_header),
                ))
                .style(Style::default().bg(theme.bg_header)),
                Rect::new(inner.x, footer_y, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "html-js")]
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        notion::NotionClient,
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
    };

    fn make_test_state() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::from_token(cfg.clone(), "ntn_html_test_token");
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, client, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    #[cfg(feature = "html-js")]
    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[cfg(feature = "html-js")]
    #[test]
    fn test_html_preview_onclick_js_dom_mutation_mouse_click_at_and_raw_toggle() {
        let html = r#"<!DOCTYPE html>
<html>
<head><title>Interactive Counter</title></head>
<body>
  <h1>Demo</h1>
  <p id="status">Count: 0</p>
  <button id="inc" onclick="document.getElementById('status').textContent = 'Count: 1'; alert('Incremented!')">Increment</button>
</body>
</html>"#;

        let (app, mut ui) = make_test_state();
        let mut ov = HtmlPreviewOverlay::new("counter.html", html.to_string());
        assert!(ov.session.js_active, "onclick handler must activate QuickJS");

        let area = Rect::new(0, 0, 100, 28);
        let mut terminal = Terminal::new(TestBackend::new(100, 28)).unwrap();
        let mut hitmap = HitMap::new();

        terminal
            .draw(|frame| {
                hitmap.clear();
                ov.draw(frame, area, &app, &mut ui, &mut hitmap);
            })
            .unwrap();

        let screen0 = buffer_text(&terminal);
        assert!(screen0.contains("Count: 0"));
        assert!(screen0.contains("[m: Rendered]"));
        assert!(screen0.contains("[o: Serve]"));
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlToggleMode))
            .is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlServeBrowser))
            .is_some());
        assert!(hitmap
            .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
            .is_some());

        // Locate the rendered button coordinates and click it via handle_click -> click_at!
        let html_theme = HtmlPreviewOverlay::html_theme_from(&ui.theme);
        let ctrl = ov
            .session
            .render(ov.body_rect.width, &html_theme, false)
            .controls[0]
            .clone();
        let click_col = ov.body_rect.x + ctrl.col_start + 1;
        let click_row = ov.body_rect.y + ctrl.line as u16;

        let click_outcome = ov.handle_click(click_col, click_row);
        assert_eq!(
            click_outcome,
            OverlayOutcome::Dispatch(Action::ShowToast("Incremented!".to_string()))
        );

        // Re-render and verify JS DOM mutation is reflected live!
        terminal
            .draw(|frame| {
                hitmap.clear();
                ov.draw(frame, area, &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let screen1 = buffer_text(&terminal);
        assert!(screen1.contains("Count: 1"), "DOM mutation should show Count: 1");

        // Toggle Raw mode with 'm'
        assert_eq!(ov.handle(&Action::Input('m')), OverlayOutcome::Handled);
        assert!(ov.session.raw_mode);
        terminal
            .draw(|frame| {
                hitmap.clear();
                ov.draw(frame, area, &app, &mut ui, &mut hitmap);
            })
            .unwrap();
        let screen_raw = buffer_text(&terminal);
        assert!(screen_raw.contains("[m: Raw]"));
        assert!(screen_raw.contains("<!DOCTYPE html>"));

        // Press 'o' to dispatch ServeActiveHtml
        assert_eq!(
            ov.handle(&Action::Input('o')),
            OverlayOutcome::Dispatch(Action::ServeActiveHtml)
        );
    }

    #[tokio::test]
    async fn test_html_preview_native_server_serves_over_localhost() {
        let (app, _ui) = make_test_state();
        let url = app
            .html_server
            .serve_html("Notion Preview", "<h1>Hello Notion HTML</h1>")
            .await
            .unwrap();
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(url.contains("/view/"));

        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/html"));
        let body = resp.text().await.unwrap();
        assert!(body.contains("<h1>Hello Notion HTML</h1>"));
        app.html_server.shutdown();
    }

    #[test]
    fn test_html_preview_keyboard_focus_input_editing_and_form_submission() {
        let html = r#"
            <form action="/apply">
              <input type="text" name="query" value="notion" />
              <button type="submit">Go</button>
            </form>
            <details><summary>More Info</summary><p>Hidden detail text</p></details>
        "#;
        let mut ov = HtmlPreviewOverlay::new("Form Test", html.to_string());
        assert!(!ov.session.js_active, "static HTML should not spawn QuickJS");

        // Tab to text input, Enter to edit, type & backspace, Enter to finish
        assert_eq!(ov.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(ov.session.focused_control, Some(0));
        assert_eq!(ov.handle(&Action::Activate), OverlayOutcome::Handled);
        assert!(ov.accepts_text());
        assert_eq!(ov.handle(&Action::Input('!')), OverlayOutcome::Handled);
        assert_eq!(ov.handle(&Action::Backspace), OverlayOutcome::Handled);
        assert_eq!(ov.handle(&Action::Input('2')), OverlayOutcome::Handled);
        assert_eq!(ov.handle(&Action::Activate), OverlayOutcome::Handled);
        assert!(!ov.accepts_text());

        // Tab to submit button, Space/Enter submits form with updated value
        assert_eq!(ov.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(ov.session.focused_control, Some(1));
        match ov.handle(&Action::Input(' ')) {
            OverlayOutcome::Dispatch(Action::ShowToast(msg)) => {
                assert!(msg.contains("/apply"));
                assert!(msg.contains("query=notion2"));
            }
            other => panic!("expected form submission toast, got {other:?}"),
        }

        // Tab to details summary, Shift+Tab back, Tab forward, activate toggles details open
        assert_eq!(ov.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(ov.session.focused_control, Some(2));
        assert_eq!(ov.handle(&Action::PrevFocus), OverlayOutcome::Handled);
        assert_eq!(ov.session.focused_control, Some(1));
        assert_eq!(ov.handle(&Action::Tab), OverlayOutcome::Handled);
        assert_eq!(ov.handle(&Action::Activate), OverlayOutcome::Handled);
        let theme = HtmlTheme::dark();
        let rendered = ov.session.render(80, &theme, false);
        let joined: String = rendered
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect();
        assert!(joined.contains("Hidden detail text"));
    }

    #[cfg(all(feature = "svg-graphics", feature = "html-js"))]
    #[test]
    fn test_html_preview_inline_svg_renders_halfblock_and_preserves_button_click_coordinates() {
        let html = r##"<!DOCTYPE html>
<html>
<body>
  <h1>Graphic Report</h1>
  <svg width="120" height="60"><rect width="120" height="60" fill="#e11d48"/></svg>
  <button onclick="alert('Clicked Below SVG!')">Confirm Action</button>
</body>
</html>"##;

        let (app, mut ui) = make_test_state();
        ui.graphics_caps = img_preview::TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (8, 16),
            cell_px_exact: true,
        };

        let mut ov = HtmlPreviewOverlay::new("graphic.html", html.to_string());
        let area = Rect::new(0, 0, 100, 32);
        let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
        let mut hitmap = HitMap::new();

        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                hitmap.clear();
                ov.draw(frame, area, &app, &mut ui, &mut hitmap);
            })
            .unwrap();

        let html_theme = HtmlPreviewOverlay::html_theme_from(&ui.theme);
        let rendered = ov.session.render(ov.body_rect.width, &html_theme, false);
        assert_eq!(rendered.graphics.len(), 1, "expected 1 inline SVG graphic");
        let graphic = &rendered.graphics[0];
        assert!(graphic.line_count >= 3);

        // Verify reserved slot contains Halfblock `▀` glyphs in the rendered TestBackend buffer
        let buf = terminal.backend().buffer();
        let mut halfblock_cells = 0usize;
        for line_idx in graphic.line_start..graphic.line_start + graphic.line_count {
            let row = ov.body_rect.y + line_idx as u16;
            if row < ov.body_rect.bottom() {
                for col in ov.body_rect.x..ov.body_rect.right() {
                    if buf[(col, row)].symbol() == "▀" {
                        halfblock_cells += 1;
                    }
                }
            }
        }
        assert!(
            halfblock_cells > 0,
            "expected Halfblock '▀' glyphs inside reserved SVG slot"
        );

        // Verify <button> below the SVG retains exact click coordinates via click_at(line, col)
        assert_eq!(rendered.controls.len(), 1);
        let btn = rendered.controls[0].clone();
        assert!(
            btn.line >= graphic.line_start + graphic.line_count,
            "button line {} must sit below graphic slot {}..{}",
            btn.line,
            graphic.line_start,
            graphic.line_start + graphic.line_count
        );
        let click_col = ov.body_rect.x + btn.col_start + 1;
        let click_row = ov.body_rect.y + btn.line as u16;
        assert_eq!(
            ov.handle_click(click_col, click_row),
            OverlayOutcome::Dispatch(Action::ShowToast("Clicked Below SVG!".to_string()))
        );

        // Also verify pixel protocol (Kitty) pushes to pending_pixel_graphics and marks cells skip=true
        ui.graphics_caps.protocol = TerminalImageProtocol::Kitty;
        let mut frame_cell_skipped = false;
        terminal
            .draw(|frame| {
                ui.pending_pixel_graphics.clear();
                hitmap.clear();
                ov.draw(frame, area, &app, &mut ui, &mut hitmap);
                if let Some(pg) = ui.pending_pixel_graphics.first() {
                    frame_cell_skipped = frame.buffer_mut()[(pg.rect.x, pg.rect.y)].skip;
                }
            })
            .unwrap();
        assert_eq!(ui.pending_pixel_graphics.len(), 1);
        let pg = &ui.pending_pixel_graphics[0];
        assert!(pg.cache_key.starts_with("html:"));
        assert_eq!(pg.protocol, TerminalImageProtocol::Kitty);
        assert!(pg.payload.as_deref().is_some_and(|p| p.starts_with("\x1b_G")));
        assert!(
            frame_cell_skipped,
            "pixel graphic cells must be marked skip=true in frame buffer"
        );
    }
}
