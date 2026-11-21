//! Local SQLite FTS5 + Korean `<3` char LIKE Search Overlay (`/`).

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::NotionSearchHit,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{centered_rect, scroll_selection_clamped, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Default)]
pub struct SearchOverlay {
    pub query: String,
    pub selected: usize,
}

impl SearchOverlay {
    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    fn hits(&self, app: &AppState) -> Vec<NotionSearchHit> {
        let q = self.query.trim();
        if q.is_empty() {
            return app
                .pages
                .iter()
                .filter(|p| !p.archived)
                .take(15)
                .map(|p| NotionSearchHit {
                    id: p.id.clone(),
                    workspace_id: p.workspace_id.clone(),
                    item_type: "page".into(),
                    title: p.display_title().to_string(),
                    icon: p.icon.clone(),
                    snippet: p.properties_summary.clone(),
                    url: p.url.clone(),
                    last_edited_time: p.last_edited_time.clone(),
                })
                .collect();
        }
        app.storage
            .search_local(&app.workspace_id, q, 30)
            .unwrap_or_default()
    }

    pub fn handle(&mut self, action: &Action, app: &AppState) -> OverlayOutcome {
        let hits = self.hits(app);
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp => {
                if !hits.is_empty() {
                    let cur = self.selected.min(hits.len() - 1);
                    self.selected = (cur + hits.len() - 1) % hits.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if !hits.is_empty() {
                    let cur = self.selected.min(hits.len() - 1);
                    self.selected = (cur + 1) % hits.len();
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = scroll_selection_clamped(self.selected, hits.len(), -5);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.selected = scroll_selection_clamped(self.selected, hits.len(), 5);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom => {
                self.selected = hits.len().saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.selected = scroll_selection_clamped(self.selected, hits.len(), *delta as isize);
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                self.query.pop();
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.query.push(*ch);
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => {
                let idx = self.selected.min(hits.len().saturating_sub(1));
                if let Some(hit) = hits.get(idx) {
                    let act = if hit.item_type == "database" {
                        Action::OpenDatabaseById(hit.id.clone())
                    } else {
                        Action::OpenPageById(hit.id.clone())
                    };
                    OverlayOutcome::CloseAndDispatch(act)
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
        let rect = centered_rect(area, area.width.saturating_sub(8).min(86), 20);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Search Notion (FTS5) · Enter Open · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width < 10 || inner.height < 3 {
            return;
        }

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" / ", theme.accent_style().add_modifier(Modifier::BOLD)),
                Span::raw(self.query.clone()),
                Span::styled("▏", theme.accent_style()),
            ])),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let hits = self.hits(app);
        if hits.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(" No matching pages or databases.", theme.muted_style())),
                Rect::new(inner.x, inner.y + 2, inner.width, 1),
            );
            return;
        }

        let list_h = inner.height.saturating_sub(2) as usize;
        let active_sel = self.selected.min(hits.len().saturating_sub(1));
        let offset = crate::ui::nav_pane::selection_offset(active_sel, list_h, hits.len());

        for (row, hit) in hits.iter().skip(offset).take(list_h).enumerate() {
            let idx = offset + row;
            let y = inner.y + 2 + row as u16;
            let is_sel = idx == active_sel;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg_primary)
            };
            let icon = hit
                .icon
                .as_deref()
                .unwrap_or(if hit.item_type == "database" { "🗂" } else { "📄" });
            let title_w = 28.min((inner.width as usize) / 2);
            let snip_w = (inner.width as usize).saturating_sub(title_w + 8);
            let line = Line::from(vec![
                Span::styled(format!(" {icon} "), style),
                Span::styled(
                    crate::ui::truncate_width(&hit.title, title_w),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", style),
                Span::styled(
                    crate::ui::truncate_width(&hit.snippet, snip_w),
                    theme.muted_style(),
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
