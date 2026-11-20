//! Multi-Workspace Switcher Overlay (`Ctrl+S`).

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
    overlays::{centered_rect, scroll_selection_clamped, OverlayOutcome},
};

#[derive(Debug, Clone, Default)]
pub struct WorkspaceSwitcherOverlay {
    pub selected: usize,
}

impl WorkspaceSwitcherOverlay {
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let count = app.config.workspaces.len();
        match action {
            Action::CloseOverlay | Action::Input('q') => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp | Action::Input('k') => {
                if count > 0 {
                    let cur = self.selected.min(count - 1);
                    self.selected = (cur + count - 1) % count;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown | Action::Input('j') => {
                if count > 0 {
                    let cur = self.selected.min(count - 1);
                    self.selected = (cur + 1) % count;
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = scroll_selection_clamped(self.selected, count, -5);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.selected = scroll_selection_clamped(self.selected, count, 5);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop | Action::Input('g') => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom | Action::Input('G') => {
                self.selected = count.saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.selected = scroll_selection_clamped(self.selected, count, *delta as isize);
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                if let Some(ws) = app.config.workspaces.get(self.selected) {
                    OverlayOutcome::CloseAndDispatch(Action::SwitchWorkspace(ws.name.clone()))
                } else {
                    OverlayOutcome::Close
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
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        let theme = &ui.theme;
        let rect = centered_rect(area, 64, 14);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Switch Notion Workspace (Ctrl+S) · Enter Switch · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if app.config.workspaces.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "No saved workspaces. Run `notion workspaces add <name> --token <ntn_...>`.",
                    theme.muted_style(),
                )),
                inner,
            );
            return;
        }

        let active = app.config.effective_workspace_name(None);
        let visible_rows = inner.height as usize;
        let sel = self
            .selected
            .min(app.config.workspaces.len().saturating_sub(1));
        let offset =
            crate::ui::nav_pane::selection_offset(sel, visible_rows, app.config.workspaces.len());

        for (row, ws) in app
            .config
            .workspaces
            .iter()
            .skip(offset)
            .take(visible_rows)
            .enumerate()
        {
            let idx = offset + row;
            let y = inner.y + row as u16;
            let is_sel = idx == sel;
            let is_active = ws.name.eq_ignore_ascii_case(&active);
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg_primary)
            };
            let icon = ws.workspace_icon.as_deref().unwrap_or("🏢");
            let active_badge = if is_active { "  ● active" } else { "" };
            let line = Line::from(vec![
                Span::styled(
                    format!(" {icon} {} ", ws.display_name()),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("({}) [{}]", ws.name, ws.auth_mode), theme.muted_style()),
                Span::styled(
                    active_badge,
                    Style::default().fg(theme.success).add_modifier(Modifier::BOLD),
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
