//! Media & Files Tray Overlay (`A`) for embedded images, files, and PDFs on the active Notion page.

use bitefoundry_html::{is_safe_raster_image, sniff_content_kind, ContentKind};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::NotionMediaRef,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, scroll_selection_clamped, OverlayOutcome},
    },
};

pub fn is_html_media(item: &NotionMediaRef) -> bool {
    let lower_mime = item.mime_type.to_ascii_lowercase();
    if lower_mime.contains("html") {
        return true;
    }
    sniff_content_kind(Some(&item.mime_type), Some(&item.display_name), None) == ContentKind::Html
        || sniff_content_kind(None, Some(&item.url), None) == ContentKind::Html
}

pub fn is_raster_image_media(item: &NotionMediaRef) -> bool {
    item.is_image
        && !is_html_media(item)
        && is_safe_raster_image(Some(&item.mime_type), Some(&item.display_name), None)
}

#[derive(Debug, Clone, Default)]
pub struct AttachmentsState {
    pub selected: usize,
}

impl AttachmentsState {
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let len = app
            .active_page_detail
            .as_ref()
            .map(|d| d.media.len())
            .unwrap_or(0);
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = scroll_selection_clamped(self.selected, len, -5);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.selected = scroll_selection_clamped(self.selected, len, 5);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.selected = len.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.selected = scroll_selection_clamped(self.selected, len, *delta as isize);
                OverlayOutcome::Handled
            }
            Action::Activate | Action::Input('d') => {
                if len > 0 {
                    OverlayOutcome::CloseAndDispatch(Action::DownloadMedia(
                        self.selected.min(len - 1),
                    ))
                } else {
                    OverlayOutcome::Close
                }
            }
            Action::Input('v') => {
                if let Some(detail) = app.active_page_detail.as_ref() {
                    if let Some(item) = detail.media.get(self.selected.min(len.saturating_sub(1))) {
                        if is_html_media(item) {
                            return OverlayOutcome::CloseAndDispatch(Action::FetchHtmlPreview {
                                title: item.display_name.clone(),
                                url: item.url.clone(),
                                block_id: (!item.block_id.trim().is_empty())
                                    .then_some(item.block_id.clone()),
                                page_id: Some(detail.page.id.clone()),
                            });
                        }
                        if is_raster_image_media(item) {
                            return OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview);
                        }
                        return OverlayOutcome::Dispatch(Action::ShowToast(format!(
                            "Preview unavailable for {} ({}) — press Enter/d to download",
                            item.display_name, item.mime_type
                        )));
                    }
                }
                OverlayOutcome::Handled
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 72, 16);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Page Media & Files (A) · Enter/d Download · v Preview · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let media = app
            .active_page_detail
            .as_ref()
            .map(|d| d.media.as_slice())
            .unwrap_or(&[]);
        if media.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled("No media or files on this page.", theme.muted_style())),
                inner,
            );
            return;
        }

        let visible_rows = inner.height as usize;
        let sel = self.selected.min(media.len().saturating_sub(1));
        let offset = crate::ui::nav_pane::selection_offset(sel, visible_rows, media.len());
        for (row, item) in media.iter().skip(offset).take(visible_rows).enumerate() {
            let idx = offset + row;
            let y = inner.y + row as u16;
            let style = if idx == sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg_primary)
            };
            let icon = if is_html_media(item) {
                "🌐"
            } else if is_raster_image_media(item) {
                "🖼"
            } else {
                "📎"
            };
            let label = format!(" {icon} {}  ({})", item.display_name, item.mime_type);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    crate::ui::truncate_width(&label, inner.width as usize),
                    style,
                ))
                .style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
