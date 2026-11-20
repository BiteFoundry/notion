//! Full-screen HD inline terminal image preview modal (`v` / `Alt+V` or clicking `[IMG ...]`).

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState,
    },
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::{
    img_preview::TerminalImageProtocol,
    models::NotionPageDetail,
    ui::{
        action::Action,
        app::{AppState, PendingPixelGraphic, UiState},
        hitmap::{ButtonId, HitMap, HitTarget},
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
        let max_cols = inner_w.saturating_mul(2).max(base_cols).min(240);
        let scaled_cols = ((base_cols as u32 * self.zoom_pct as u32) / 100)
            .clamp(8, max_cols as u32) as u16;
        // Pass `scrollable_max_rows >= scaled_cols` so `img_preview` fits tall images to width
        // and lets them scroll vertically immediately at 100% zoom.
        let scrollable_max_rows = ((inner_h as u32 * 4 * self.zoom_pct as u32) / 100)
            .max(scaled_cols as u32)
            .clamp(6, 240) as u16;
        (scaled_cols, scrollable_max_rows)
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay
            | Action::OpenImagePreview
            | Action::Input('q')
            | Action::Input('v') => OverlayOutcome::Close,
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
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.scroll_y = 0;
                self.scroll_x = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.scroll_y = u16::MAX;
                OverlayOutcome::Handled
            }
            Action::Input('h') => {
                self.scroll_x = self.scroll_x.saturating_sub(4);
                OverlayOutcome::Handled
            }
            Action::Input('l') => {
                self.scroll_x = self.scroll_x.saturating_add(4);
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
            Action::CursorLeft | Action::ToggleSidebar | Action::Input('[') => {
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
            Action::OpenActiveInBrowser | Action::Input('o') | Action::Input('O') => {
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
        &mut self,
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
        let close_label = "[✕ Esc]";
        let close_w = close_label.width() as u16;
        let show_close_pill = rect.width > close_w + 4;
        let title_budget = rect
            .width
            .saturating_sub(2 + if show_close_pill { close_w + 2 } else { 0 })
            as usize;

        let title = match self.current() {
            Some(t) => format!(
                " Image {}/{}: {} · {}% · {} · ↑↓/Wheel Scroll · [/] Prev/Next · +/- Zoom · p Proto · o Open · d Save ",
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
                crate::ui::truncate_width(&title, title_budget),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        if show_close_pill {
            let close_x = rect.right().saturating_sub(close_w + 1);
            let close_rect = Rect::new(close_x, rect.y, close_w, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    close_label,
                    Style::default().fg(theme.fg).bg(theme.bg_elevated),
                )),
                close_rect,
            );
            hitmap.add_line(
                close_x,
                rect.y,
                close_w,
                HitTarget::Button(ButtonId::ConfirmCancel),
            );
        }

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
            let is_pixel =
                rendered.pixel_payload.is_some() && proto != TerminalImageProtocol::Halfblock;
            let total_rows = if is_pixel {
                rendered.rows
            } else {
                rendered.lines.len() as u16
            };
            let total_cols = rendered.cols;

            let avail_h = if is_pixel
                && !ui.graphics_caps.cell_px_exact
                && proto == TerminalImageProtocol::Sixel
            {
                inner.height.saturating_sub(1).max(1)
            } else {
                inner.height
            };
            let show_scrollbar = total_rows > avail_h && inner.width > 2;
            let avail_w = if show_scrollbar {
                inner.width.saturating_sub(1)
            } else {
                inner.width
            };

            let max_scroll_y = total_rows.saturating_sub(avail_h);
            let max_scroll_x = total_cols.saturating_sub(avail_w);
            self.scroll_y = self.scroll_y.min(max_scroll_y);
            self.scroll_x = self.scroll_x.min(max_scroll_x);
            let sy = self.scroll_y;
            let sx = self.scroll_x;

            if is_pixel {
                let draw_w = total_cols.saturating_sub(sx).min(avail_w);
                let draw_h = total_rows.saturating_sub(sy).min(avail_h);
                let px_x = inner.x + avail_w.saturating_sub(draw_w) / 2;
                let px_y = inner.y + avail_h.saturating_sub(draw_h) / 2;
                ui.pending_pixel_graphic = Some(PendingPixelGraphic {
                    cache_key: format!("{cache_key}#crop:{sx},{sy},{avail_w},{avail_h}"),
                    rect: Rect::new(px_x, px_y, draw_w, draw_h),
                    protocol: proto,
                });
            } else {
                let view_w = total_cols.saturating_sub(sx).min(avail_w);
                let view_h = total_rows.saturating_sub(sy).min(avail_h);
                let pad_x = avail_w.saturating_sub(view_w) / 2;
                let pad_y = avail_h.saturating_sub(view_h) / 2;
                let draw_rect = Rect::new(inner.x + pad_x, inner.y + pad_y, view_w, view_h);
                frame.render_widget(
                    Paragraph::new(rendered.lines.clone()).scroll((sy, sx)),
                    draw_rect,
                );
            }

            if show_scrollbar {
                let sb_area = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
                let mut sb_state = ScrollbarState::new(max_scroll_y.saturating_add(1) as usize)
                    .position(sy as usize);
                let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(theme.accent))
                    .track_style(theme.muted_style());
                frame.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        auth::AuthManager,
        config::AppConfig,
        img_preview::{decode_and_render_for_protocol, TerminalGraphicsCaps},
        models::{NotionMediaRef, NotionPage},
        notion::NotionClient,
        storage::Storage,
        sync::{FocusTarget, SyncCommand, SyncController},
    };

    fn make_test_state() -> (AppState, UiState) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone(), None).unwrap();
        let client = NotionClient::new(&cfg, auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, client, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        ui.term_width = 100;
        ui.term_height = 30;
        (app, ui)
    }

    fn make_tall_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            Rgba([
                ((x * 255) / w.max(1)) as u8,
                ((y * 255) / h.max(1)) as u8,
                160,
                255,
            ])
        });
        let mut buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, ImageFormat::Png)
            .unwrap();
        buf.into_inner()
    }

    fn make_preview_overlay() -> ImagePreviewOverlay {
        let detail = NotionPageDetail {
            page: NotionPage {
                id: "page-1".to_string(),
                title: "Tall Image Page".to_string(),
                ..Default::default()
            },
            media: vec![
                NotionMediaRef {
                    block_id: "blk-1".to_string(),
                    url: "https://example.com/tall.png".to_string(),
                    display_name: "tall.png".to_string(),
                    mime_type: "image/png".to_string(),
                    is_image: true,
                },
                NotionMediaRef {
                    block_id: "blk-2".to_string(),
                    url: "https://example.com/second.png".to_string(),
                    display_name: "second.png".to_string(),
                    mime_type: "image/png".to_string(),
                    is_image: true,
                },
            ],
            ..Default::default()
        };
        ImagePreviewOverlay::from_notion_page(&detail, Some(0)).unwrap()
    }

    #[test]
    fn test_image_preview_scroll_clamps_and_renders_scrollbar_for_halfblock_and_sixel() {
        let png = make_tall_png(400, 2000);
        let area = Rect::new(0, 0, 100, 30);

        for proto in [TerminalImageProtocol::Halfblock, TerminalImageProtocol::Sixel] {
            let (mut app, mut ui) = make_test_state();
            ui.graphics_caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };

            let mut overlay = make_preview_overlay();
            let (cols, rows) = overlay.target_dimensions(ui.term_width, ui.term_height);
            let rendered =
                decode_and_render_for_protocol(&png, cols, rows, (25, 25, 25), ui.graphics_caps)
                    .unwrap();
            let total_rows = if proto == TerminalImageProtocol::Halfblock {
                rendered.lines.len() as u16
            } else {
                rendered.rows
            };
            let rect = ImagePreviewOverlay::modal_rect(area);
            let inner_h = rect.height.saturating_sub(2);
            let expected_max_scroll = total_rows.saturating_sub(inner_h);
            assert!(
                expected_max_scroll > 0,
                "expected scrollable tall image for {proto:?}, total_rows={total_rows}, inner_h={inner_h}"
            );

            let base_key = format!("hd:page-1:0:{cols}:{rows}:{proto:?}");
            app.image_cache.insert(base_key.clone(), rendered);

            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            let mut hitmap = HitMap::new();

            // Initial draw at scroll_y = 0
            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphic = None;
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            // Verify clickable [✕ Esc] close pill is registered in top-right border
            let close_region = hitmap
                .region_of(HitTarget::Button(ButtonId::ConfirmCancel))
                .expect("expected [✕ Esc] close pill hit target");
            assert_eq!(close_region.y, rect.y);
            assert!(close_region.right() <= rect.right());

            // Verify vertical scrollbar rendered on right-most inner column
            let sb_x = rect.right() - 2;
            let buf = terminal.backend().buffer();
            let has_sb_cell = (rect.y + 1..rect.bottom() - 1).any(|y| {
                let sym = buf[(sb_x, y)].symbol();
                !sym.trim().is_empty()
            });
            assert!(has_sb_cell, "expected scrollbar glyph in right inner column for {proto:?}");

            if proto == TerminalImageProtocol::Sixel {
                let pg0 = ui
                    .pending_pixel_graphic
                    .clone()
                    .expect("expected pending pixel graphic for Sixel");
                assert!(
                    pg0.cache_key.starts_with(&format!("{base_key}#crop:0,0,")),
                    "unexpected initial crop key: {}",
                    pg0.cache_key
                );
            }

            // MouseScroll down + MoveDown + PageDown -> ScrollBottom clamps to exact max_scroll_y
            assert_eq!(
                overlay.handle(&Action::MouseScroll {
                    delta: 3,
                    column: 50,
                    row: 15,
                }),
                OverlayOutcome::Handled
            );
            assert_eq!(overlay.handle(&Action::MoveDown), OverlayOutcome::Handled);
            assert_eq!(overlay.handle(&Action::PageDown), OverlayOutcome::Handled);
            assert_eq!(overlay.handle(&Action::ScrollBottom), OverlayOutcome::Handled);
            assert_eq!(overlay.scroll_y, u16::MAX);

            terminal
                .draw(|frame| {
                    ui.pending_pixel_graphic = None;
                    hitmap.clear();
                    overlay.draw(frame, area, &app, &mut ui, &mut hitmap);
                })
                .unwrap();

            assert_eq!(
                overlay.scroll_y, expected_max_scroll,
                "scroll_y must clamp in-place to max_scroll_y for {proto:?}"
            );

            if proto == TerminalImageProtocol::Sixel {
                let pg_bottom = ui
                    .pending_pixel_graphic
                    .clone()
                    .expect("expected pending pixel graphic at bottom for Sixel");
                assert!(
                    pg_bottom
                        .cache_key
                        .starts_with(&format!("{base_key}#crop:0,{expected_max_scroll},")),
                    "unexpected bottom crop key: {}",
                    pg_bottom.cache_key
                );
            }

            // Immediate upward response on wheel up (no unclamped runaway delay!)
            overlay.handle(&Action::MouseScroll {
                delta: -3,
                column: 50,
                row: 15,
            });
            assert_eq!(overlay.scroll_y, expected_max_scroll.saturating_sub(3));

            // ScrollTop resets both axes
            overlay.handle(&Action::Input('l'));
            assert!(overlay.scroll_x > 0);
            overlay.handle(&Action::ScrollTop);
            assert_eq!(overlay.scroll_y, 0);
            assert_eq!(overlay.scroll_x, 0);
        }

        // Verify defensive shortcut actions in handle()
        let mut ov = make_preview_overlay();
        assert_eq!(ov.handle(&Action::OpenImagePreview), OverlayOutcome::Close);
        assert_eq!(
            ov.handle(&Action::ToggleSidebar),
            OverlayOutcome::Dispatch(Action::LoadPreviewImage)
        );
        assert_eq!(ov.index, 1);
        assert_eq!(
            ov.handle(&Action::OpenActiveInBrowser),
            OverlayOutcome::Dispatch(Action::OpenUrl("https://example.com/second.png".to_string()))
        );
    }
}
