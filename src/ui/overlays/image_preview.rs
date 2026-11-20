//! Full-screen HD inline terminal image preview modal (`v` / `Alt+V` or clicking `[IMG ...]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    img_preview::TerminalImageProtocol,
    models::NotionPageDetail,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, OverlayOutcome},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewTarget {
    pub page_id: String,
    pub media_index: usize,
    pub url: String,
    pub display_name: String,
    pub mime_type: String,
}

#[derive(Debug, Clone)]
pub struct ImagePreviewOverlay {
    pub items: Vec<PreviewTarget>,
    pub index: usize,
    pub zoom_pct: u16,
    pub scroll_y: u16,
    pub scroll_x: u16,
    pub error: Option<String>,
}

impl ImagePreviewOverlay {
    pub fn from_notion_page(
        detail: &NotionPageDetail,
        preferred_media_idx: Option<usize>,
    ) -> Option<Self> {
        let mut items = Vec::new();
        let mut start = 0usize;
        for (idx, m) in detail.media.iter().enumerate() {
            if !m.is_image {
                continue;
            }
            if preferred_media_idx == Some(idx) {
                start = items.len();
            }
            items.push(PreviewTarget {
                page_id: detail.page.id.clone(),
                media_index: idx,
                url: m.url.clone(),
                display_name: m.display_name.clone(),
                mime_type: m.mime_type.clone(),
            });
        }
        if items.is_empty() {
            return None;
        }
        Some(Self {
            items,
            index: start,
            zoom_pct: 100,
            scroll_y: 0,
            scroll_x: 0,
            error: None,
        })
    }

    pub fn current(&self) -> Option<&PreviewTarget> {
        self.items.get(self.index)
    }

    pub fn modal_rect(area: Rect) -> Rect {
        centered_rect(
            area,
            area.width.saturating_sub(4).max(20).min(area.width),
            area.height.saturating_sub(2).max(10).min(area.height),
        )
    }

    pub fn target_dimensions(&self, term_width: u16, term_height: u16) -> (u16, u16) {
        let rect = Self::modal_rect(Rect::new(0, 0, term_width.max(24), term_height.max(12)));
        let inner_w = rect.width.saturating_sub(2).max(12);
        let inner_h = rect.height.saturating_sub(3).max(6);
        let base_cols = rect.width.saturating_sub(4).max(12);
        let base_rows = rect.height.saturating_sub(4).max(6);
        // Allow tall/zoomed images to exceed viewport height so vertical scrolling works cleanly
        let max_cols = inner_w.saturating_mul(2).max(base_cols);
        let max_rows = inner_h.saturating_mul(4).max(base_rows);
        let scaled_cols = ((base_cols as u32 * self.zoom_pct as u32) / 100)
            .clamp(8, max_cols as u32) as u16;
        let scaled_rows = ((base_rows as u32 * self.zoom_pct as u32) / 100)
            .clamp(4, max_rows as u32) as u16;
        (scaled_cols, scaled_rows)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') | Action::Input('v') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                self.scroll_y = self.scroll_y.saturating_sub(2);
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.scroll_y = self.scroll_y.saturating_add(2);
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.scroll_y = self.scroll_y.saturating_sub(10);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.scroll_y = self.scroll_y.saturating_add(10);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                if *delta < 0 {
                    self.scroll_y = self.scroll_y.saturating_sub((-*delta) as u16);
                } else {
                    self.scroll_y = self.scroll_y.saturating_add(*delta as u16);
                }
                OverlayOutcome::Handled
            }
            Action::CursorLeft | Action::Input('[') => {
                if self.items.len() > 1 {
                    self.index = (self.index + self.items.len() - 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                    return OverlayOutcome::Dispatch(Action::LoadPreviewImage);
                }
                OverlayOutcome::Handled
            }
            Action::CursorRight | Action::Input(']') => {
                if self.items.len() > 1 {
                    self.index = (self.index + 1) % self.items.len();
                    self.scroll_y = 0;
                    self.scroll_x = 0;
                    self.error = None;
                    return OverlayOutcome::Dispatch(Action::LoadPreviewImage);
                }
                OverlayOutcome::Handled
            }
            Action::Input('+') | Action::Input('=') => {
                self.zoom_pct = (self.zoom_pct + 25).min(300);
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('-') => {
                self.zoom_pct = self.zoom_pct.saturating_sub(25).max(25);
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('0') => {
                self.zoom_pct = 100;
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Dispatch(Action::LoadPreviewImage)
            }
            Action::Input('p') => OverlayOutcome::Dispatch(Action::CycleImageProtocol),
            Action::Input('o') | Action::Input('O') => {
                if let Some(target) = self.current() {
                    OverlayOutcome::Dispatch(Action::OpenUrl(target.url.clone()))
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Input('d') => {
                if let Some(target) = self.current() {
                    OverlayOutcome::Dispatch(Action::DownloadMedia(target.media_index))
                } else {
                    OverlayOutcome::Handled
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &mut UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = ui.theme.clone();
        let rect = Self::modal_rect(area);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let proto = ui.graphics_caps.protocol;
        let title = match self.current() {
            Some(t) => format!(
                " Image {}/{}: {} · {}% · {} · ↑↓/Wheel Scroll · [/] Prev/Next · +/- Zoom · p Proto · o Open · d Save · Esc ",
                self.index + 1,
                self.items.len(),
                crate::ui::truncate_width(&t.display_name, 22),
                self.zoom_pct,
                proto.label()
            ),
            None => " Image Preview ".to_string(),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(&title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if let Some(ref err) = self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("Preview failed: {err}"),
                    Style::default().fg(theme.error),
                )),
                inner,
            );
            return;
        }

        let Some(target) = self.current() else {
            return;
        };
        let (cols, rows) = self.target_dimensions(ui.term_width, ui.term_height);
        let cache_key = format!(
            "hd:{}:{}:{}:{}:{:?}",
            target.page_id, target.media_index, cols, rows, proto
        );

        if let Some(rendered) = app.image_cache.get(&cache_key) {
            if rendered.pixel_payload.is_some() && proto != TerminalImageProtocol::Halfblock {
                let draw_w = rendered.cols.min(inner.width);
                let draw_h = rendered.rows.min(inner.height);
                let px_x = inner.x + inner.width.saturating_sub(draw_w) / 2;
                let px_y = inner.y + inner.height.saturating_sub(draw_h) / 2;
                ui.pending_pixel_graphic = Some(PendingPixelGraphic {
                    cache_key,
                    rect: Rect::new(px_x, px_y, draw_w, draw_h),
                    protocol: proto,
                });
            } else {
                let total_rows = rendered.lines.len() as u16;
                let max_scroll_y = total_rows.saturating_sub(inner.height);
                let sy = self.scroll_y.min(max_scroll_y);
                let sx = self.scroll_x.min(rendered.cols.saturating_sub(inner.width));
                frame.render_widget(
                    Paragraph::new(rendered.lines.clone()).scroll((sy, sx)),
                    inner,
                );
            }
        } else {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "Loading Notion image preview...",
                    theme.muted_style(),
                )),
                inner,
            );
        }
    }
}
