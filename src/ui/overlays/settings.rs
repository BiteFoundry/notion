//! Settings Overlay (`Ctrl+,`):
//! 5 curated Notion-inspired palettes (Clean Dark / Warm Paper / Tokyo Night / Catppuccin / Nord) + ANSI/NoColor,
//! Page Width mode (Centered / Full), Sync Interval, Image Protocol, Inline Images, Inline Hrefs, Notifications.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, Default)]
pub struct SettingsState {
    pub selected: usize,
}

const SETTING_COUNT: usize = 7;

impl SettingsState {
    pub fn select(&mut self, index: usize) {
        if index < SETTING_COUNT {
            self.selected = index;
        }
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                self.selected = (self.selected + SETTING_COUNT - 1) % SETTING_COUNT;
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                self.selected = (self.selected + 1) % SETTING_COUNT;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit | Action::Input(' ') | Action::CursorRight => {
                let dispatch = match self.selected {
                    0 => Action::SetTheme(app.config.theme.next()),
                    1 => Action::TogglePageWidth,
                    2 => Action::CycleSyncInterval,
                    3 => Action::CycleImageProtocol,
                    4 => Action::ToggleInlineImages,
                    5 => Action::ToggleShowLinkUrls,
                    6 => Action::ToggleNotify,
                    _ => Action::Noop,
                };
                OverlayOutcome::Dispatch(dispatch)
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
        let rect = centered_rect(area, 68, 13);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Settings (Ctrl+,) · Enter/Space Cycle · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let rows = [
            ("Theme Palette", app.config.theme.label().to_string()),
            ("Page Width Mode", ui.page_width.label().to_string()),
            (
                "Background Sync Interval",
                format!("{}s", app.config.sync_poll_ms / 1000),
            ),
            (
                "Image Protocol",
                ui.graphics_caps.protocol.label().to_string(),
            ),
            (
                "Inline Images",
                if ui.inline_images { "ON" } else { "OFF" }.to_string(),
            ),
            (
                "Inline Link [href] URLs",
                if ui.show_inline_hrefs { "ON" } else { "OFF" }.to_string(),
            ),
            (
                "Desktop Notifications",
                if app.config.notify { "ON" } else { "OFF" }.to_string(),
            ),
        ];

        for (idx, (label, val)) in rows.iter().take(inner.height as usize).enumerate() {
            let y = inner.y + idx as u16;
            let is_sel = idx == self.selected;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg_primary)
            };
            let line = Line::from(vec![
                Span::styled(format!(" {:<28} ", label), style),
                Span::styled(
                    format!("[ {val} ]"),
                    theme.accent_style().add_modifier(Modifier::BOLD),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(style),
                Rect::new(inner.x, y, inner.width, 1),
            );
            hitmap.add_line(inner.x, y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
