//! Media & Files Tray Overlay (`A`) for embedded images, files, and PDFs on the active Notion page.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, scroll_selection_clamped, OverlayOutcome},
};

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
            Action::Input('v') => OverlayOutcome::CloseAndDispatch(Action::OpenImagePreview),
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
            let icon = if item.is_image { "🖼" } else { "📎" };
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
