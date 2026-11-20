//! Context Menu Overlay (`.` or right-click) for Notion pages, databases, and sidebar items.

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::{
    models::DatabaseViewMode,
    ui::{
        action::Action,
        app::{AppState, UiState},
        hitmap::{HitMap, HitTarget},
        overlays::{OverlayKind, OverlayOutcome},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextTarget {
    Sidebar,
    PageCanvas,
    Database,
    Global,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
    pub label: String,
    pub shortcut: Option<String>,
    pub action: Action,
}

impl MenuItem {
    fn new(label: impl Into<String>, shortcut: Option<&str>, action: Action) -> Self {
        Self {
            label: label.into(),
            shortcut: shortcut.map(str::to_string),
            action,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextMenu {
    pub context: ContextTarget,
    pub anchor: (u16, u16),
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ContextMenu {
    pub fn build(target: ContextTarget, anchor: (u16, u16), _app: &AppState, _ui: &UiState) -> Self {
        let items = match target {
            ContextTarget::PageCanvas => vec![
                MenuItem::new("Quick Append (/h1, /todo, ...)", Some("a"), Action::OpenQuickAppend),
                MenuItem::new("Edit in $EDITOR", Some("e"), Action::RequestExternalEditor),
                MenuItem::new("New Child Page", Some("n"), Action::OpenNewPageModal),
                MenuItem::new("Toggle Favorite ★", Some("f"), Action::ToggleFavorite),
                MenuItem::new("Comments Drawer", Some("C"), Action::ToggleCommentsDrawer),
                MenuItem::new("Toggle Centered / Full Width", Some("w"), Action::TogglePageWidth),
                MenuItem::new("Extract Links", Some("u"), Action::OpenLinkExtractor),
                MenuItem::new("Preview Images", Some("v"), Action::OpenImagePreview),
                MenuItem::new("Copy Code / Markdown", Some("y"), Action::CopySelectionOrMarkdown),
                MenuItem::new("Open in Browser", Some("o"), Action::OpenActiveInBrowser),
                MenuItem::new("Archive Page", Some("D"), Action::ConfirmArchivePage),
            ],
            ContextTarget::Database => vec![
                MenuItem::new("Table View", Some("1"), Action::SetDbViewMode(DatabaseViewMode::Table)),
                MenuItem::new("Board (Kanban) View", Some("2"), Action::SetDbViewMode(DatabaseViewMode::Board)),
                MenuItem::new("List View", Some("3"), Action::SetDbViewMode(DatabaseViewMode::List)),
                MenuItem::new("Filter Rows", Some("f"), Action::OpenDbFilter),
                MenuItem::new("Cycle Sort", Some("s"), Action::CycleDbSort),
                MenuItem::new("New Database Item", Some("n"), Action::OpenNewPageModal),
                MenuItem::new("Open in Browser", Some("o"), Action::OpenActiveInBrowser),
            ],
            ContextTarget::Sidebar | ContextTarget::Global => vec![
                MenuItem::new("Quick Find & Commands", Some("Ctrl+K"), Action::OpenOverlay(OverlayKind::CommandPalette)),
                MenuItem::new("Switch Workspace", Some("Ctrl+S"), Action::OpenOverlay(OverlayKind::WorkspaceSwitcher)),
                MenuItem::new("New Child Page", Some("n"), Action::OpenNewPageModal),
                MenuItem::new("Sync Workspace", Some("Ctrl+R"), Action::RefreshAll),
                MenuItem::new("Settings & Palettes", Some("Ctrl+,"), Action::OpenOverlay(OverlayKind::Settings)),
            ],
        };

        Self {
            context: target,
            anchor,
            items,
            selected: 0,
        }
    }

    pub fn select(&mut self, index: usize) {
        if index < self.items.len() {
            self.selected = index;
        }
    }

    pub fn prev(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + self.items.len() - 1) % self.items.len();
        }
    }

    pub fn next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.selected = crate::ui::overlays::scroll_selection_clamped(
            self.selected,
            self.items.len(),
            delta,
        );
    }

    pub fn activate(&self) -> OverlayOutcome {
        match self.items.get(self.selected) {
            Some(item) => OverlayOutcome::CloseAndDispatch(item.action.clone()),
            None => OverlayOutcome::Close,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let width = 38u16.min(area.width.saturating_sub(2)).max(20);
        let height = (self.items.len() as u16 + 2)
            .min(area.height.saturating_sub(2))
            .max(4);
        let x = self.anchor.0.min(area.x + area.width.saturating_sub(width));
        let y = self.anchor.1.min(area.y + area.height.saturating_sub(height));
        let rect = Rect::new(x, y, width, height);

        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(" Actions ", theme.title(true))))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let visible_rows = inner.height as usize;
        let selected = self.selected.min(self.items.len().saturating_sub(1));
        let offset = crate::ui::nav_pane::selection_offset(selected, visible_rows, self.items.len());

        for (row, item) in self.items.iter().skip(offset).take(visible_rows).enumerate() {
            let idx = offset + row;
            let row_y = inner.y + row as u16;
            let is_sel = idx == selected;
            let style = if is_sel {
                theme.selection()
            } else {
                Style::default().bg(theme.bg_elevated).fg(theme.fg)
            };
            let sc = item
                .shortcut
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let left_w = (inner.width as usize).saturating_sub(sc.len() + 2);
            let left = crate::ui::truncate_width(&item.label, left_w);
            let line = Line::from(vec![
                Span::styled(format!(" {left}"), style),
                Span::styled(
                    sc,
                    if is_sel {
                        style
                    } else {
                        Style::default().bg(theme.bg_elevated).fg(theme.fg_muted)
                    },
                ),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(style),
                Rect::new(inner.x, row_y, inner.width, 1),
            );
            hitmap.add_line(inner.x, row_y, inner.width, HitTarget::OverlayItem { index: idx });
        }
    }
}
