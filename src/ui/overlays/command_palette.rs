//! Notion Quick Find & Command Palette (`Ctrl+K` / `Ctrl+P`):
//! Fuzzy SQLite FTS5 search across pages & databases combined with instant workspace commands.

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
    overlays::{centered_rect, scroll_selection_clamped, OverlayKind, OverlayOutcome},
};

#[derive(Debug, Clone)]
struct PaletteEntry {
    badge: String,
    label: String,
    detail: String,
    action: Action,
}

#[derive(Debug, Clone, Default)]
pub struct CommandPalette {
    pub query: String,
    pub selected: usize,
}

impl CommandPalette {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn select(&mut self, index: usize) {
        self.selected = index;
    }

    fn entries(&self, app: &AppState) -> Vec<PaletteEntry> {
        let q = self.query.trim();
        let lower = q.to_ascii_lowercase();
        let mut out = Vec::new();

        // 1. Live FTS5 / LIKE search when query is non-empty, else top recent pages/DBs
        if !q.is_empty() {
            if let Ok(hits) = app.storage.search_local(&app.workspace_id, q, 12) {
                for h in hits {
                    let is_db = h.item_type == "database";
                    let icon = h
                        .icon
                        .unwrap_or_else(|| if is_db { "🗂".into() } else { "📄".into() });
                    out.push(PaletteEntry {
                        badge: if is_db { "DB".into() } else { "PAGE".into() },
                        label: format!("{icon} {}", h.title),
                        detail: h.snippet,
                        action: if is_db {
                            Action::OpenDatabaseById(h.id)
                        } else {
                            Action::OpenPageById(h.id)
                        },
                    });
                }
            }
        } else {
            for p in app.pages.iter().filter(|p| !p.archived).take(6) {
                out.push(PaletteEntry {
                    badge: "PAGE".into(),
                    label: format!("{} {}", p.display_icon(), p.display_title()),
                    detail: p.properties_summary.clone(),
                    action: Action::OpenPageById(p.id.clone()),
                });
            }
            for d in app.databases.iter().take(4) {
                out.push(PaletteEntry {
                    badge: "DB".into(),
                    label: format!("{} {}", d.display_icon(), d.display_title()),
                    detail: d.description.clone(),
                    action: Action::OpenDatabaseById(d.id.clone()),
                });
            }
        }

        // 2. Workspace commands
        let commands = [
            ("Quick Append Block (/h1, /todo, /callout)", "a", Action::OpenQuickAppend),
            ("New Child Page / Database Item", "n", Action::OpenNewPageModal),
            ("Edit Active Page (In-Terminal Markdown)", "e", Action::OpenPageEditor),
            ("Open in External $EDITOR", "Ctrl+E", Action::RequestExternalEditor),
            ("Toggle Page Width (Centered / Full)", "w", Action::TogglePageWidth),
            ("Toggle Favorite Star", "f", Action::ToggleFavorite),
            ("Toggle Comments Peek Drawer", "C", Action::ToggleCommentsDrawer),
            ("Switch Notion Workspace", "Ctrl+S", Action::OpenOverlay(OverlayKind::WorkspaceSwitcher)),
            ("Sync Workspace Now", "Ctrl+R", Action::RefreshAll),
            ("Extract Links from Page", "u", Action::OpenLinkExtractor),
            ("Preview Page Images (HD)", "v", Action::OpenImagePreview),
            ("Open File Picker (zoxide)", "Ctrl+F", Action::OpenFilePicker),
            ("Open Active Item in Browser", "o", Action::OpenActiveInBrowser),
            ("Settings & Palettes", "Ctrl+,", Action::OpenOverlay(OverlayKind::Settings)),
            ("Keyboard Shortcuts Help", "?", Action::OpenOverlay(OverlayKind::Help)),
        ];

        for (label, sc, act) in commands {
            if lower.is_empty() || label.to_ascii_lowercase().contains(&lower) {
                out.push(PaletteEntry {
                    badge: "CMD".into(),
                    label: label.to_string(),
                    detail: sc.to_string(),
                    action: act,
                });
            }
        }

        out
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, _ui: &UiState) -> OverlayOutcome {
        let items = self.entries(app);
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::MoveUp => {
                if !items.is_empty() {
                    let cur = self.selected.min(items.len() - 1);
                    self.selected = (cur + items.len() - 1) % items.len();
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                if !items.is_empty() {
                    let cur = self.selected.min(items.len() - 1);
                    self.selected = (cur + 1) % items.len();
                }
                OverlayOutcome::Handled
            }
            Action::PageUp => {
                self.selected = scroll_selection_clamped(self.selected, items.len(), -5);
                OverlayOutcome::Handled
            }
            Action::PageDown => {
                self.selected = scroll_selection_clamped(self.selected, items.len(), 5);
                OverlayOutcome::Handled
            }
            Action::CursorHome | Action::ScrollTop => {
                self.selected = 0;
                OverlayOutcome::Handled
            }
            Action::CursorEnd | Action::ScrollBottom => {
                self.selected = items.len().saturating_sub(1);
                OverlayOutcome::Handled
            }
            Action::MouseScroll { delta, .. } => {
                self.selected = scroll_selection_clamped(self.selected, items.len(), *delta as isize);
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
                let idx = self.selected.min(items.len().saturating_sub(1));
                if let Some(entry) = items.get(idx) {
                    OverlayOutcome::CloseAndDispatch(entry.action.clone())
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
        let rect = centered_rect(area, area.width.saturating_sub(8).min(82), 20);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                " Quick Find & Commands (Ctrl+K) · Enter Open · Esc Close ",
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width < 10 || inner.height < 3 {
            return;
        }

        let input_line = Line::from(vec![
            Span::styled(" 🔍 ", theme.accent_style()),
            Span::raw(self.query.clone()),
            Span::styled("▏", theme.accent_style()),
        ]);
        frame.render_widget(
            Paragraph::new(input_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let items = self.entries(app);
        let list_h = inner.height.saturating_sub(2) as usize;
        let active_sel = self.selected.min(items.len().saturating_sub(1));
        let offset = crate::ui::nav_pane::selection_offset(active_sel, list_h, items.len());

        for (row, entry) in items.iter().skip(offset).take(list_h).enumerate() {
            let idx = offset + row;
            let y = inner.y + 2 + row as u16;
            let is_sel = idx == active_sel;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg_primary)
            };
            let badge_style = if entry.badge == "DB" {
                Style::default().fg(theme.accent_soft).add_modifier(Modifier::BOLD)
            } else if entry.badge == "PAGE" {
                Style::default().fg(theme.success).add_modifier(Modifier::BOLD)
            } else {
                theme.muted_style()
            };
            let left_budget = (inner.width as usize).saturating_sub(22).max(10);
            let line = Line::from(vec![
                Span::styled(format!(" [{:<4}] ", entry.badge), badge_style),
                Span::styled(crate::ui::truncate_width(&entry.label, left_budget), style),
                Span::styled("  ", style),
                Span::styled(
                    crate::ui::truncate_width(&entry.detail, 14),
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
