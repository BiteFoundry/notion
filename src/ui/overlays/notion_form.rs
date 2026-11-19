//! Notion Authoring Modals (`src/ui/overlays/notion_form.rs`):
//! - Slash-style Quick Append Composer (`a`: `/h1`, `/h2`, `/todo`, `/callout`, `/code`, `/quote`, `/toggle`)
//! - New Child Page / New Database Item modal (`n`)
//! - Database Filter modal (`f`)

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotionFormMode {
    QuickAppend,
    NewChildPage,
    NewDatabaseItem,
    DatabaseFilter,
}

#[derive(Debug, Clone)]
pub struct NotionFormOverlay {
    pub mode: NotionFormMode,
    pub title: String,
    pub body: String,
    pub focus_body: bool,
}

impl NotionFormOverlay {
    pub fn new(mode: NotionFormMode) -> Self {
        Self {
            mode,
            title: String::new(),
            body: String::new(),
            focus_body: matches!(mode, NotionFormMode::QuickAppend),
        }
    }

    pub fn accepts_text(&self) -> bool {
        true
    }

    pub fn select(&mut self, index: usize) {
        self.focus_body = index > 0;
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            Action::Tab | Action::PrevFocus => {
                if matches!(
                    self.mode,
                    NotionFormMode::NewChildPage | NotionFormMode::NewDatabaseItem
                ) {
                    self.focus_body = !self.focus_body;
                }
                OverlayOutcome::Handled
            }
            Action::Newline => {
                if self.focus_body {
                    self.body.push('\n');
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                if self.focus_body {
                    self.body.pop();
                } else {
                    self.title.pop();
                }
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                if self.focus_body {
                    self.body.push(*ch);
                } else {
                    self.title.push(*ch);
                }
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                if self.focus_body {
                    self.body.push_str(text);
                } else {
                    self.title.push_str(&text.replace(['\r', '\n'], " "));
                }
                OverlayOutcome::Handled
            }
            Action::Activate | Action::OverlaySubmit => self.submit(),
            _ => OverlayOutcome::Handled,
        }
    }

    fn submit(&self) -> OverlayOutcome {
        match self.mode {
            NotionFormMode::QuickAppend => {
                let text = self.body.trim().to_string();
                if text.is_empty() {
                    return OverlayOutcome::Close;
                }
                OverlayOutcome::CloseAndDispatch(Action::SubmitSlashAppend(text))
            }
            NotionFormMode::NewChildPage => {
                let title = self.title.trim().to_string();
                if title.is_empty() {
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::SubmitCreatePage {
                    title,
                    icon: None,
                    body: self.body.clone(),
                    in_database: false,
                })
            }
            NotionFormMode::NewDatabaseItem => {
                let title = self.title.trim().to_string();
                if title.is_empty() {
                    return OverlayOutcome::Handled;
                }
                OverlayOutcome::CloseAndDispatch(Action::SubmitCreatePage {
                    title,
                    icon: None,
                    body: self.body.clone(),
                    in_database: true,
                })
            }
            NotionFormMode::DatabaseFilter => {
                OverlayOutcome::CloseAndDispatch(Action::ApplyDbFilter(self.title.trim().to_string()))
            }
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let height = match self.mode {
            NotionFormMode::DatabaseFilter => 7,
            NotionFormMode::QuickAppend => 12,
            NotionFormMode::NewChildPage | NotionFormMode::NewDatabaseItem => 14,
        };
        let rect = centered_rect(area, area.width.saturating_sub(8).min(78), height);
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let heading = match self.mode {
            NotionFormMode::QuickAppend => {
                " Slash Quick Append · /h1 /h2 /todo /callout /code /quote /toggle · Enter Send "
            }
            NotionFormMode::NewChildPage => " New Child Page · Tab Body · Enter Create · Esc Cancel ",
            NotionFormMode::NewDatabaseItem => {
                " New Database Item · Tab Body · Enter Create · Esc Cancel "
            }
            NotionFormMode::DatabaseFilter => " Filter Database Rows · Enter Apply · Esc Clear ",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(heading, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width < 10 || inner.height < 3 {
            return;
        }

        match self.mode {
            NotionFormMode::DatabaseFilter => {
                let line = Line::from(vec![
                    Span::styled(" Filter (title or status): ", theme.muted_style()),
                    Span::raw(self.title.clone()),
                    Span::styled("▏", theme.accent_style()),
                ]);
                frame.render_widget(Paragraph::new(line), Rect::new(inner.x, inner.y + 1, inner.width, 1));
            }
            NotionFormMode::QuickAppend => {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        " Commands: /h1  /h2  /h3  /todo  /callout  /code  /quote  /toggle  /divider",
                        theme.muted_style(),
                    )),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                let mut lines: Vec<Line<'static>> = self
                    .body
                    .lines()
                    .map(|l| Line::from(format!(" {l}")))
                    .collect();
                if lines.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(" ", Style::default()),
                        Span::styled("▏", theme.accent_style()),
                        Span::styled(
                            " Type /todo Ship v0.1.0 or Markdown...",
                            theme.muted_style().add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                } else if let Some(last) = lines.last_mut() {
                    last.spans.push(Span::styled("▏", theme.accent_style()));
                }
                frame.render_widget(
                    Paragraph::new(lines).style(Style::default().bg(theme.bg_composer)),
                    Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(2)),
                );
            }
            NotionFormMode::NewChildPage | NotionFormMode::NewDatabaseItem => {
                let title_line = Line::from(vec![
                    Span::styled(" Title: ", theme.title(!self.focus_body)),
                    Span::raw(self.title.clone()),
                    if !self.focus_body {
                        Span::styled("▏", theme.accent_style())
                    } else {
                        Span::raw("")
                    },
                ]);
                frame.render_widget(
                    Paragraph::new(title_line),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(" Body (optional Markdown):", theme.title(self.focus_body))),
                    Rect::new(inner.x, inner.y + 2, inner.width, 1),
                );
                let body_display = if self.body.is_empty() {
                    if self.focus_body {
                        " ▏".to_string()
                    } else {
                        " (press Tab to add Markdown body)".to_string()
                    }
                } else if self.focus_body {
                    format!(" {}▏", self.body)
                } else {
                    format!(" {}", self.body)
                };
                frame.render_widget(
                    Paragraph::new(body_display).style(Style::default().bg(theme.bg_composer)),
                    Rect::new(inner.x, inner.y + 3, inner.width, inner.height.saturating_sub(3)),
                );
            }
        }
    }
}
