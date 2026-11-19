//! Generic path input overlay.

use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Default)]
pub struct PathInputState {
    pub value: String,
}

impl PathInputState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::Backspace => {
                self.value.pop();
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.value.push(*ch);
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.value.push_str(&text.replace(['\r', '\n'], ""));
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let p = self.value.trim();
                if p.is_empty() {
                    OverlayOutcome::Close
                } else {
                    OverlayOutcome::CloseAndDispatch(Action::AttachFormFile(PathBuf::from(p)))
                }
            }
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 64, 5);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(" File Path · Enter Open · Esc Cancel ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width > 0 && inner.height > 0 {
            let line = Line::from(vec![
                Span::raw(format!(" {}", self.value)),
                Span::styled("▏", theme.accent_style()),
            ]);
            frame.render_widget(Paragraph::new(line), inner);
        }
    }
}
